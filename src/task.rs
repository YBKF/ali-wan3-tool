//! 视频生成任务：阶段状态机，以及「提交 → 轮询 → 下载」这条异步管线。
//!
//! 这个模块不认识 `WanApp`，只负责推进流程并把进度回吐给调用方，
//! 视图侧要做的只是把进度映射成自己的状态。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use gpui_kit::{AsyncApp, Context, SharedString, WeakEntity};

use crate::api::{self, TaskOutcome, Usage};
use crate::config::Config;
use crate::media::human_size;

/// 官方建议的轮询间隔
pub const POLL_INTERVAL: Duration = Duration::from_secs(15);
/// 最长轮询次数，约 40 分钟
pub const MAX_POLLS: u32 = 160;
/// 下载进度的刷新间隔
const DOWNLOAD_TICK: Duration = Duration::from_millis(500);

/// 任务阶段。
#[derive(Clone)]
pub enum Phase {
    Idle,
    Submitting,
    Polling { status: SharedString, polls: u32 },
    Downloading { received: u64, total: u64 },
    Done,
    Failed(SharedString),
}

/// 阶段的粗分类，视图据此决定配色。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PhaseKind {
    Idle,
    Busy,
    Done,
    Failed,
}

impl Phase {
    pub fn kind(&self) -> PhaseKind {
        match self {
            Phase::Idle => PhaseKind::Idle,
            Phase::Submitting | Phase::Polling { .. } | Phase::Downloading { .. } => {
                PhaseKind::Busy
            }
            Phase::Done => PhaseKind::Done,
            Phase::Failed(_) => PhaseKind::Failed,
        }
    }

    pub fn is_busy(&self) -> bool {
        self.kind() == PhaseKind::Busy
    }

    /// 状态徽标文案。
    pub fn label(&self) -> &'static str {
        match self {
            Phase::Idle => "待命",
            Phase::Submitting => "提交任务中",
            Phase::Polling { .. } => "生成中",
            Phase::Downloading { .. } => "下载中",
            Phase::Done => "已完成",
            Phase::Failed(_) => "失败",
        }
    }

    /// 阶段自身的补充说明。返回 `None` 表示「没有额外信息」，
    /// 由调用方回落到上一条提示文本。
    pub fn detail(&self) -> Option<String> {
        match self {
            Phase::Polling { status, polls: 0 } => {
                Some(format!("{status} · 等待首次查询（每 15 秒轮询一次）"))
            }
            Phase::Polling { status, polls } => {
                Some(format!("{status} · 第 {polls} 次轮询（每 15 秒一次）"))
            }
            Phase::Downloading { received, total } if *total > 0 => Some(format!(
                "{} / {}",
                human_size(*received),
                human_size(*total)
            )),
            Phase::Downloading { received, .. } => Some(human_size(*received)),
            Phase::Failed(message) => Some(message.to_string()),
            Phase::Idle | Phase::Submitting | Phase::Done => None,
        }
    }

    /// 进度条百分比。`None` 表示进度不确定，界面上用转圈动画代替。
    pub fn progress(&self) -> Option<f32> {
        match self {
            Phase::Downloading { received, total } if *total > 0 => {
                Some(*received as f32 / *total as f32 * 100.)
            }
            _ => None,
        }
    }
}

/// 轮询的终态。只有失败/成功两种，不会出现「还在跑」。
pub enum PollOutcome {
    Succeeded {
        video_url: String,
        usage: Option<Usage>,
    },
    Failed {
        code: String,
        message: String,
    },
}

/// 一次成功生成的产物。
pub struct ResultInfo {
    pub video_url: String,
    pub saved: Option<PathBuf>,
    pub usage: Option<Usage>,
    pub elapsed: Duration,
}

/// 把阻塞调用丢到后台线程，回到前台 await 结果。
///
/// HTTP 用的是同步客户端，必须隔离，否则会卡住渲染线程。
pub async fn offload<T, W>(cx: &AsyncApp, work: W) -> T
where
    T: Send + 'static,
    W: FnOnce() -> T + Send + 'static,
{
    cx.background_executor().spawn(async move { work() }).await
}

/// 回到前台更新视图并重绘。
///
/// 窗口关掉后残留的异步任务不该 panic，所以实体已销毁时安静地忽略。
pub fn update_view<T: 'static>(
    view: &WeakEntity<T>,
    cx: &mut AsyncApp,
    update: impl FnOnce(&mut T, &mut Context<T>),
) {
    let _ = view.update(cx, |this, cx| {
        update(this, cx);
        cx.notify();
    });
}

/// 轮询任务直到结束或超时。
///
/// 每次查询都会调用 `on_poll`，由调用方决定怎么展示（甚至忽略）。
pub async fn poll_until_done(
    cx: &mut AsyncApp,
    config: &Config,
    task_id: &str,
    mut on_poll: impl FnMut(&mut AsyncApp, &'static str, u32),
) -> Result<PollOutcome, String> {
    for poll in 1..=MAX_POLLS {
        let interval = cx.background_executor().timer(POLL_INTERVAL);
        interval.await;

        let config = config.clone();
        let task_id = task_id.to_string();
        let queried = offload(cx, move || api::query_task(&config, &task_id)).await;

        match queried {
            Ok(TaskOutcome::Pending) => on_poll(cx, "PENDING", poll),
            Ok(TaskOutcome::Running) => on_poll(cx, "RUNNING", poll),
            Ok(TaskOutcome::Succeeded { video_url, usage }) => {
                return Ok(PollOutcome::Succeeded { video_url, usage });
            }
            Ok(TaskOutcome::Failed { code, message }) => {
                return Ok(PollOutcome::Failed { code, message });
            }
            Err(error) => return Err(format!("{error:#}")),
        }
    }

    Err(format!(
        "任务超时：等待超过 {} 分钟仍未完成，请稍后重试",
        MAX_POLLS as u64 * POLL_INTERVAL.as_secs() / 60
    ))
}

/// 下载生成的视频，期间按 `on_progress` 回报已下载/总字节数。
///
/// 下载跑在后台线程，前台只按固定节拍读进度；这样既不用给同步客户端加回调，
/// 也不会阻塞界面。
pub async fn download_with_progress(
    cx: &mut AsyncApp,
    url: &str,
    destination: &Path,
    mut on_progress: impl FnMut(&mut AsyncApp, u64, u64),
) -> Result<u64, String> {
    let received = Arc::new(AtomicU64::new(0));
    let total = Arc::new(AtomicU64::new(0));
    let finished: Arc<Mutex<Option<Result<u64, String>>>> = Arc::new(Mutex::new(None));

    {
        let url = url.to_string();
        let destination = destination.to_path_buf();
        let received = received.clone();
        let total = total.clone();
        let finished = finished.clone();

        cx.background_executor()
            .spawn(async move {
                let result = api::download_video(&url, &destination, &received, &total)
                    .map_err(|error| format!("{error:#}"));
                if let Ok(mut slot) = finished.lock() {
                    *slot = Some(result);
                }
            })
            .detach();
    }

    loop {
        let tick = cx.background_executor().timer(DOWNLOAD_TICK);
        tick.await;

        let downloaded = received.load(Ordering::Relaxed);
        let expected = total.load(Ordering::Relaxed);
        let result = finished.lock().ok().and_then(|mut slot| slot.take());

        on_progress(cx, downloaded, expected);

        if let Some(result) = result {
            return result;
        }
    }
}

/// 版本号防抖器。
///
/// 每次修改 `bump()` 一个版本号；落盘任务拿着自己那一版去比对，
/// 只要期间又改过就作废——连点十下也只会写一次。
#[derive(Default)]
pub struct Debouncer {
    revision: u64,
}

impl Debouncer {
    /// 记一次修改，返回本次的版本号。
    pub fn bump(&mut self) -> u64 {
        self.revision = self.revision.wrapping_add(1);
        self.revision
    }

    pub fn is_current(&self, revision: u64) -> bool {
        self.revision == revision
    }
}

/// 等待 `delay` 后执行一次写入，返回 `None` 表示期间又改过、本次作废。
pub async fn write_after_debounce<T, S, W>(
    view: &WeakEntity<T>,
    cx: &mut AsyncApp,
    delay: Duration,
    is_current: S,
    write: W,
) -> Option<anyhow::Result<()>>
where
    T: 'static,
    S: FnOnce(&T) -> bool,
    W: FnOnce() -> anyhow::Result<()> + Send + 'static,
{
    let delay_timer = cx.background_executor().timer(delay);
    delay_timer.await;

    if !view.update(cx, |this, _| is_current(this)).unwrap_or(false) {
        return None;
    }

    Some(offload(cx, write).await)
}

/// 人类可读的耗时，例如 `8s` / `2m05s`。
pub fn format_elapsed(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    if seconds < 60 {
        format!("{seconds}s")
    } else {
        format!("{}m{:02}s", seconds / 60, seconds % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_kind_drives_busy_state() {
        assert!(!Phase::Idle.is_busy());
        assert!(Phase::Submitting.is_busy());
        assert!(
            Phase::Polling {
                status: SharedString::from("RUNNING"),
                polls: 3,
            }
            .is_busy()
        );
        assert!(
            Phase::Downloading {
                received: 1,
                total: 2,
            }
            .is_busy()
        );
        assert!(!Phase::Done.is_busy());
        assert!(!Phase::Failed(SharedString::from("x")).is_busy());
    }

    #[test]
    fn polling_detail_reports_poll_count() {
        let waiting = Phase::Polling {
            status: SharedString::from("PENDING"),
            polls: 0,
        };
        assert!(waiting.detail().unwrap().contains("等待首次查询"));

        let polling = Phase::Polling {
            status: SharedString::from("RUNNING"),
            polls: 7,
        };
        assert_eq!(
            polling.detail().unwrap(),
            "RUNNING · 第 7 次轮询（每 15 秒一次）"
        );
    }

    /// 未知总大小时不能给出百分比，界面要用转圈而不是假进度。
    #[test]
    fn download_progress_is_unknown_without_total() {
        assert_eq!(
            Phase::Downloading {
                received: 1024,
                total: 0,
            }
            .progress(),
            None
        );
        assert_eq!(
            Phase::Downloading {
                received: 50,
                total: 200,
            }
            .progress(),
            Some(25.)
        );
        assert_eq!(Phase::Submitting.progress(), None);
    }

    #[test]
    fn phases_without_extra_detail_fall_back_to_caller() {
        assert!(Phase::Idle.detail().is_none());
        assert!(Phase::Done.detail().is_none());
        assert_eq!(
            Phase::Failed(SharedString::from("任务失败：InvalidParameter")).detail(),
            Some("任务失败：InvalidParameter".to_string())
        );
    }

    #[test]
    fn elapsed_formats_minutes() {
        assert_eq!(format_elapsed(Duration::from_secs(8)), "8s");
        assert_eq!(format_elapsed(Duration::from_secs(59)), "59s");
        assert_eq!(format_elapsed(Duration::from_secs(60)), "1m00s");
        assert_eq!(format_elapsed(Duration::from_secs(125)), "2m05s");
    }

    /// 只有最新那一版算「还有效」，旧版本必须作废，否则连点会写多次。
    #[test]
    fn debouncer_only_keeps_latest_revision() {
        let mut debouncer = Debouncer::default();

        let first = debouncer.bump();
        assert!(debouncer.is_current(first));

        let second = debouncer.bump();
        assert!(!debouncer.is_current(first));
        assert!(debouncer.is_current(second));
    }
}
