//! 单实例：第二次启动时唤醒已在运行的实例，然后让新进程安静退出。
//!
//! `app_single_instance` 的唤醒回调跑在监听线程上，而 GPUI 的 `App` 不是 `Send`，
//! 回调里拿不到窗口。所以回调只置一个原子标志，主线程用前台任务轮询这个标志，
//! 收到信号后把主窗口恢复并激活到最前。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use app_single_instance::{PrimaryHandle, notify_if_running, start_primary};
use gpui_kit::{AnyWindowHandle, App};

use crate::constants::APP_ID;

/// 唤醒信号的轮询间隔：够快让人感觉「立刻响应」，又不至于让主循环空转太频繁。
const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// 主实例句柄。
///
/// 由 `main` 持有到进程退出；丢弃它会关闭监听线程，之后新启动的实例就找不到本进程了。
pub struct SingleInstance {
    /// 监听线程置位、主线程清位的唤醒标志
    wake: Arc<AtomicBool>,
    /// 持有 IPC 监听线程
    _primary: PrimaryHandle,
}

impl SingleInstance {
    /// 抢占主实例身份。
    ///
    /// 已经有实例在运行时先唤醒它，然后返回 `None`，此时调用方应直接退出。
    pub fn claim() -> Option<Self> {
        if notify_if_running(APP_ID) {
            return None;
        }

        let wake = Arc::new(AtomicBool::new(false));
        let signal = wake.clone();
        let primary = start_primary(APP_ID, move || signal.store(true, Ordering::SeqCst));

        Some(Self {
            wake,
            _primary: primary,
        })
    }

    /// 开始监听唤醒信号：每收到一次就把 `window` 恢复并激活到最前。
    pub fn watch(&self, window: AnyWindowHandle, cx: &mut App) {
        let wake = self.wake.clone();

        cx.spawn(async move |cx| {
            loop {
                cx.background_executor().timer(POLL_INTERVAL).await;

                if !wake.swap(false, Ordering::SeqCst) {
                    continue;
                }

                // 窗口关掉后主循环也会退出，轮询没有再继续的意义
                let activated = window.update(cx, |_, window, _| window.activate_window());
                if activated.is_err() {
                    break;
                }
            }
        })
        .detach();
    }
}
