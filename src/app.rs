//! 主界面：参考图 + 提示词 + 参数 → 创建任务 → 轮询 → 自动下载。
//!
//! 持久化约定：
//! - 连接设置（API Key / 业务空间 ID / 地域 / 输出目录）在弹窗里编辑，点「保存」写入 config.json
//! - 生成参数一旦被修改就自动写入 config.json（短暂防抖，避免连续点击写多次）
//! - 提示词单独存到 prompt.txt，停止输入 3 秒后自动保存

use std::path::PathBuf;
use std::time::{Duration, Instant};

use gpui_kit::base::{Link, StyledExt};
use gpui_kit::component::status_bar::StatusBar;
use gpui_kit::component::{ActiveTheme, Sizable};
use gpui_kit::component::{
    Disableable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    dialog::DialogFooter,
    h_flex,
    input::{Input, InputEvent, InputState, NumberInput, Textarea, TextareaState},
    list::{List, ListState},
    notification::Notification,
    progress::Progress,
    switch::Switch,
    v_flex,
};
use gpui_kit::prelude::*;
use gpui_kit::{
    AnyElement, ClickEvent, Context, Entity, ExternalPaths, PathPromptOptions, Render,
    SharedString, Subscription, Window, div, px,
};
use semver::Version;

use crate::api::{self, CreateRequest, MediaItem, Parameters, RequestInput, Usage};
use crate::config::{Config, MAX_DURATION, MIN_DURATION, MODELS, RATIOS, REGIONS, RESOLUTIONS};
use crate::constants::VERSION;
use crate::media::{self, RefImage};
use crate::ref_images::RefImagesDelegate;
use crate::settings::SettingsForm;
use crate::task::{
    self, Debouncer, Phase, PhaseKind, PollOutcome, ResultInfo, download_with_progress, offload,
    poll_until_done, update_view, write_after_debounce,
};
use crate::templates_ui::PromptTemplatesDialog;
use crate::types::update::RepoVersion;
use crate::ui::{self, Palette};

/// 提示词停止输入后多久落盘
const PROMPT_SAVE_DELAY: Duration = Duration::from_secs(3);
/// 参数修改后多久落盘（防抖，连续点击只写一次）
const CONFIG_SAVE_DELAY: Duration = Duration::from_millis(400);
/// 轮询期间每几次查询记一条通知，避免刷屏
const POLL_LOG_EVERY: u32 = 4;

/// 输出文件名带时间戳，避免同一秒内多次生成互相覆盖。
fn output_path(output_dir: &str) -> PathBuf {
    PathBuf::from(output_dir).join(format!(
        "wan3-{}.mp4",
        chrono::Local::now().format("%Y%m%d-%H%M%S")
    ))
}

// ------------------------------------------------------------------ 主视图

pub struct WanApp {
    config: Config,

    api_key: Entity<InputState>,
    workspace_id: Entity<InputState>,
    output_dir: Entity<InputState>,
    region_index: usize,
    settings_auto_opened: bool,
    update_available_dialog_opened: bool,

    prompt: Entity<TextareaState>,
    url_input: Entity<InputState>,
    duration: Entity<InputState>,
    smart_duration: bool,
    resolution_index: usize,
    ratio_index: usize,
    model_index: usize,
    audio: bool,
    /// 单实例模式：只在启动时读取，运行中改动下次启动才生效
    single_instance: bool,

    images: Vec<RefImage>,
    /// 参考图列表（虚拟化 + 拖拽重排）；顺序即「图1」「图2」的引用顺序
    list_state: Entity<ListState<RefImagesDelegate>>,

    phase: Phase,
    /// 待推送的通知，在下一帧统一交给窗口右上角的 Notification 组件
    notifications: Vec<Notification>,
    message: Option<SharedString>,
    result: Option<ResultInfo>,

    /// 落盘防抖：参数与提示词各一份，互不干扰
    config_saver: Debouncer,
    prompt_saver: Debouncer,
    prompt_dirty: bool,
    prompt_status: Option<SharedString>,

    _prompt_subscription: Subscription,
    _duration_subscription: Subscription,
}

impl WanApp {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let config = Config::load();

        let api_key = ui::text_input(window, cx, "sk-…", config.api_key.clone());
        let workspace_id = ui::text_input(
            window,
            cx,
            "业务空间 ID，留空则用 dashscope.aliyuncs.com",
            config.workspace_id.clone(),
        );
        let output_dir = ui::text_input(window, cx, "视频保存目录", config.output_dir.clone());
        let url_input = ui::text_input(window, cx, "粘贴 https:// 图片地址", "");
        let prompt = cx.new(|cx| {
            TextareaState::new(window, cx)
                .rows(5)
                .placeholder("描述视频；可用「图1」「图2」指代上方的参考图")
                .default_value(Config::load_prompt())
        });

        let owner = cx.weak_entity();
        let list_state = cx.new(|cx| ListState::new(RefImagesDelegate::new(owner), window, cx));

        let smart_duration = config.duration < 0;
        let duration_default = if smart_duration {
            5
        } else {
            config.duration.clamp(MIN_DURATION, MAX_DURATION)
        };
        let duration = ui::number_input(
            window,
            cx,
            duration_default.to_string(),
            MIN_DURATION as f64..=MAX_DURATION as f64,
        );

        let prompt_subscription = cx.subscribe(&prompt, |this, _state, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.mark_prompt_dirty(cx);
            }
        });
        let duration_subscription =
            cx.subscribe(&duration, |this, _state, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.mark_config_dirty(CONFIG_SAVE_DELAY, cx);
                }
            });

        Self {
            region_index: REGIONS
                .iter()
                .position(|(_, value)| *value == config.region)
                .unwrap_or(0),
            model_index: MODELS
                .iter()
                .position(|(_, value)| *value == config.model)
                .unwrap_or(0),
            resolution_index: RESOLUTIONS
                .iter()
                .position(|value| *value == config.resolution)
                .unwrap_or(2),
            ratio_index: RATIOS
                .iter()
                .position(|value| *value == config.ratio)
                .unwrap_or(0),
            audio: config.audio,
            single_instance: config.single_instance,
            smart_duration,
            settings_auto_opened: false,
            update_available_dialog_opened: false,
            config,
            api_key,
            workspace_id,
            output_dir,
            prompt,
            url_input,
            duration,
            images: Vec::new(),
            list_state,
            phase: Phase::Idle,
            notifications: Vec::new(),
            message: None,
            result: None,
            config_saver: Debouncer::default(),
            prompt_saver: Debouncer::default(),
            prompt_dirty: false,
            prompt_status: None,
            _prompt_subscription: prompt_subscription,
            _duration_subscription: duration_subscription,
        }
    }

    // ---------------------------------------------------------------- 配置快照

    /// 时长输入框的当前值，非法输入回落到 5 秒。
    fn duration_seconds(&self, cx: &Context<Self>) -> i32 {
        self.duration
            .read(cx)
            .value()
            .to_string()
            .trim()
            .parse::<i32>()
            .unwrap_or(5)
            .clamp(MIN_DURATION, MAX_DURATION)
    }

    fn snapshot_config(&self, cx: &Context<Self>) -> Config {
        let mut config = self.config.clone();
        config.api_key = self.api_key.read(cx).value().trim().to_string();
        config.workspace_id = self.workspace_id.read(cx).value().trim().to_string();
        config.output_dir = self.output_dir.read(cx).value().trim().to_string();
        config.region = REGIONS[self.region_index].1.to_string();
        config.model = MODELS[self.model_index].1.to_string();
        config.resolution = RESOLUTIONS[self.resolution_index].to_string();
        config.ratio = RATIOS[self.ratio_index].to_string();
        config.audio = self.audio;
        config.single_instance = self.single_instance;
        config.duration = if self.smart_duration {
            -1
        } else {
            self.duration_seconds(cx)
        };
        config
    }

    // ---------------------------------------------------------------- 提示出口

    /// 排队一条通知，下一帧由 `Render for WanApp` 推送到窗口右上角。
    fn push_note(&mut self, cx: &mut Context<Self>, note: Notification) {
        self.notifications.push(note);
        cx.notify();
    }

    /// 过程信息，以 info 通知呈现。
    fn log(&mut self, message: impl Into<String>, cx: &mut Context<Self>) {
        self.push_note(cx, Notification::info(message.into()));
    }

    /// 需要用户注意但不致命的问题，以 warning 通知呈现。
    fn warn(&mut self, message: impl Into<String>, cx: &mut Context<Self>) {
        let message = message.into();
        self.message = Some(SharedString::from(message.clone()));
        self.push_note(cx, Notification::warning(message));
    }

    /// 失败，以常驻的 error 通知呈现（需手动关闭）。
    fn fail(&mut self, message: impl Into<String>, cx: &mut Context<Self>) {
        let message = message.into();
        self.phase = Phase::Failed(SharedString::from(message.clone()));
        self.message = Some(SharedString::from(message.clone()));
        self.push_note(cx, Notification::error(message).autohide(false));
    }

    // ---------------------------------------------------------------- 持久化

    /// 生成参数一改就落盘；连续点击只写最后一次。
    fn mark_config_dirty(&mut self, delay: Duration, cx: &mut Context<Self>) {
        let revision = self.config_saver.bump();
        let config = self.snapshot_config(cx);
        self.config = config.clone();

        cx.spawn(async move |this, cx| {
            let outcome = write_after_debounce(
                &this,
                cx,
                delay,
                |app| app.config_saver.is_current(revision),
                move || config.save(),
            )
            .await;

            if let Some(Err(error)) = outcome {
                update_view(&this, cx, |app, cx| {
                    app.log(format!("参数保存失败：{error:#}"), cx)
                });
            }
        })
        .detach();
    }

    /// 提示词停止输入 3 秒后写入 prompt.txt。
    fn mark_prompt_dirty(&mut self, cx: &mut Context<Self>) {
        let revision = self.prompt_saver.bump();
        let text = self.prompt.read(cx).value().to_string();
        self.prompt_dirty = true;
        self.prompt_status = Some(SharedString::from("待保存…"));
        cx.notify();

        cx.spawn(async move |this, cx| {
            let outcome = write_after_debounce(
                &this,
                cx,
                PROMPT_SAVE_DELAY,
                |app| app.prompt_saver.is_current(revision),
                move || Config::save_prompt(&text),
            )
            .await;

            let Some(outcome) = outcome else {
                return;
            };
            update_view(&this, cx, |app, cx| {
                app.prompt_dirty = false;
                app.prompt_status = Some(match &outcome {
                    Ok(()) => SharedString::from("已保存"),
                    Err(error) => {
                        app.log(format!("提示词保存失败：{error:#}"), cx);
                        SharedString::from(format!("保存失败：{error}"))
                    }
                });
            });
        })
        .detach();
    }

    /// 把 `text` 替换到提示词输入框。
    pub(crate) fn replace_prompt(
        &mut self,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.prompt.update(cx, |state, cx| {
            state.set_value(SharedString::new(text), window, cx);
        });
        self.mark_prompt_dirty(cx);
        cx.notify();
    }

    // ---------------------------------------------------------------- 提示词模版弹窗

    fn open_prompt_templates_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let owner = cx.weak_entity();
        let panel = cx.new(|_| PromptTemplatesDialog::new(owner));

        window.open_dialog(cx, move |dialog, _window, _cx| {
            let panel = panel.clone();
            dialog
                .title("提示词模版管理")
                .w(px(640.))
                .child(panel)
                .footer(
                    DialogFooter::new().child(
                        Button::new("prompt-templates-close")
                            .label("关闭")
                            .outline()
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    ),
                )
        });
    }

    // ---------------------------------------------------------------- 连接设置弹窗

    fn open_settings_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let owner = cx.weak_entity();
        let api_key = self.api_key.clone();
        let workspace_id = self.workspace_id.clone();
        let output_dir = self.output_dir.clone();
        let region_index = self.region_index;
        let form = cx.new(move |_| {
            SettingsForm::new(owner, api_key, workspace_id, output_dir, region_index)
        });

        let footer_form = form.clone();
        window.open_dialog(cx, move |dialog, _window, _cx| {
            let form = form.clone();
            let footer_form = footer_form.clone();
            dialog.title("连接设置").w(px(580.)).child(form).footer(
                DialogFooter::new()
                    .child(
                        Button::new("settings-close")
                            .label("关闭")
                            .outline()
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("settings-save")
                            .label("保存")
                            .primary()
                            .on_click(move |_, window, cx| {
                                footer_form.update(cx, |form, cx| form.save(cx));
                                window.close_dialog(cx);
                            }),
                    ),
            )
        });
    }

    // ---------------------------------------------------------------- 更新检查弹窗
    fn open_update_available_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.open_dialog(cx, move |dialog, _window, cx| {
            let theme = cx.theme();

            dialog
                .title("有新版本可用")
                .child(
                    v_flex().child(
                        Link::new("update-release-addr")
                            .href("https://gitee.com/YBKF/ali-wan3-tool/releases/latest")
                            .open_with(|href, _, _, cx| cx.open_url(href))
                            .cursor_pointer()
                            .h_7()
                            .px_3()
                            .py_0()
                            .flex()
                            .items_center()
                            .border_1()
                            .border_color(theme.border)
                            .text_color(theme.link)
                            .font_light()
                            .child("打开最新发布版页面 →"),
                    ),
                )
                .footer(
                    DialogFooter::new().child(
                        Button::new("update-close")
                            .label("关闭")
                            .outline()
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    ),
                )
        });
    }

    // ---------------------------------------------------------------- 参考图

    fn pick_local_images(&mut self, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some(SharedString::from("选择参考图")),
        });

        cx.spawn(async move |this, cx| match receiver.await {
            Ok(Ok(Some(paths))) => {
                this.update(cx, |this, cx| this.add_local_paths(paths, cx))
                    .ok();
            }
            Ok(Ok(None)) => {}
            Ok(Err(error)) => {
                this.update(cx, |this, cx| {
                    this.warn(format!("打开文件选择器失败：{error}"), cx)
                })
                .ok();
            }
            Err(_) => {}
        })
        .detach();
    }

    fn add_local_paths(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        let mut files: Vec<PathBuf> = paths.into_iter().filter(|path| path.is_file()).collect();
        if files.is_empty() {
            return;
        }

        let room = self.image_room(cx);
        if room == 0 {
            return;
        }
        if files.len() > room {
            self.log(format!("最多再添加 {room} 张，已忽略多余的图片"), cx);
            files.truncate(room);
        }

        self.log(format!("正在处理 {} 张图片…", files.len()), cx);
        let decode = cx.background_executor().spawn(async move {
            files
                .into_iter()
                .map(|path| {
                    let prepared = media::prepare_local(&path);
                    (path, prepared)
                })
                .collect::<Vec<_>>()
        });

        cx.spawn(async move |this, cx| {
            let decoded = decode.await;
            this.update(cx, |this, cx| {
                for (path, result) in decoded {
                    match result {
                        Ok(image) => this.append_image(image, cx),
                        Err(error) => this.warn(format!("{}：{error:#}", path.display()), cx),
                    }
                }
            })
            .ok();
        })
        .detach();
    }

    fn add_remote_image(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.image_room(cx) == 0 {
            return;
        }

        let url = self.url_input.read(cx).value().trim().to_string();
        match media::prepare_remote(&url) {
            Ok(image) => {
                self.append_image(image, cx);
                self.url_input.update(cx, |state, cx| {
                    state.set_value(SharedString::from(""), window, cx);
                });
                cx.notify();
            }
            Err(error) => self.warn(format!("{error:#}"), cx),
        }
    }

    /// 剩余可添加的参考图数量（上限 10 张）；已经满了会提示一次并返回 0。
    fn image_room(&mut self, cx: &mut Context<Self>) -> usize {
        let room = media::MAX_IMAGES.saturating_sub(self.images.len());
        if room == 0 {
            self.warn("参考图最多 10 张", cx);
        }
        room
    }

    /// 追加一张参考图：记日志、刷新列表、重绘。
    ///
    /// 改 `self.images` 的唯一入口，保证列表与主视图不会走散。
    fn append_image(&mut self, image: RefImage, cx: &mut Context<Self>) {
        self.log(
            format!("已添加参考图 {}（{}）", image.label, image.info),
            cx,
        );
        self.images.push(image);
        self.sync_image_list(cx);
        cx.notify();
    }

    /// 把 `self.images` 整份灌进参考图列表。
    ///
    /// 只能在事件回调里调用：列表的重排/删除是自己改完 delegate 再写回主视图的，
    /// 若在渲染期反过来读主视图会和它自己的 `Render` 借冲突。
    fn sync_image_list(&mut self, cx: &mut Context<Self>) {
        let items = self.images.clone();
        self.list_state.update(cx, |state, cx| {
            state.delegate_mut().reset(items);
            cx.notify();
        });
    }

    /// 参考图列表增删/重排后的回写入口。
    pub(crate) fn apply_images(&mut self, images: Vec<RefImage>, cx: &mut Context<Self>) {
        self.images = images;
        cx.notify();
    }

    /// 连接设置弹窗的落盘入口：写回地域并整体保存一次。
    pub(crate) fn apply_connection_settings(
        &mut self,
        region_index: usize,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<()> {
        self.region_index = region_index;
        let config = self.snapshot_config(cx);
        let saved = config.save();
        self.config = config;
        cx.notify();
        saved
    }

    // ---------------------------------------------------------------- 生成流程

    /// 生成完成，切入下载阶段。
    fn begin_download(&mut self, elapsed: Duration, usage: Option<&Usage>, cx: &mut Context<Self>) {
        self.phase = Phase::Downloading {
            received: 0,
            total: 0,
        };
        self.log(
            format!(
                "生成完成（耗时 {}），开始下载…",
                task::format_elapsed(elapsed)
            ),
            cx,
        );
        if let Some(usage) = usage {
            self.log(format!("用量：{}", usage.describe()), cx);
        }
    }

    /// 下载结束。
    ///
    /// 下载失败不算流程失败：视频已经生成，链接 24 小时内还能救回来。
    fn finish_download(
        &mut self,
        video_url: String,
        destination: PathBuf,
        usage: Option<Usage>,
        downloaded: Result<u64, String>,
        elapsed: Duration,
        cx: &mut Context<Self>,
    ) {
        let saved = match downloaded {
            Ok(bytes) => {
                self.log(
                    format!(
                        "已保存 {}（{}）",
                        destination.display(),
                        media::human_size(bytes)
                    ),
                    cx,
                );
                self.message = Some(SharedString::from(format!(
                    "已保存到 {}",
                    destination.display()
                )));
                Some(destination)
            }
            Err(error) => {
                self.warn(
                    format!("视频已生成但下载失败：{error}。链接 24 小时内有效，请尽快手动下载"),
                    cx,
                );
                None
            }
        };

        self.phase = Phase::Done;
        self.result = Some(ResultInfo {
            video_url,
            saved,
            usage,
            elapsed,
        });
    }

    fn start_generation(&mut self, cx: &mut Context<Self>) {
        if self.phase.is_busy() {
            return;
        }

        let config = self.snapshot_config(cx);
        self.config = config.clone();

        let prompt = self.prompt.read(cx).value().trim().to_string();

        if config.api_key.is_empty() {
            self.warn("请先在「连接设置」里填写 API Key", cx);
            return;
        }
        if config.output_dir.is_empty() {
            self.warn("请填写输出目录", cx);
            return;
        }
        if prompt.is_empty() && self.images.is_empty() {
            self.warn("提示词与参考图至少填入其一", cx);
            return;
        }
        if self.images.len() > media::MAX_IMAGES {
            self.warn("参考图最多 10 张", cx);
            return;
        }

        if let Err(error) = config.save() {
            self.log(format!("配置保存失败：{error}"), cx);
        }

        let request = CreateRequest {
            model: config.model.clone(),
            input: RequestInput {
                prompt: (!prompt.is_empty()).then(|| prompt.clone()),
                media: self
                    .images
                    .iter()
                    .map(|image| MediaItem {
                        kind: "reference_image".to_string(),
                        url: image.media_url.clone(),
                    })
                    .collect(),
            },
            parameters: Parameters {
                resolution: config.resolution.clone(),
                ratio: config.ratio.clone(),
                duration: config.duration,
                audio: config.audio,
            },
        };

        self.result = None;
        self.message = None;
        self.phase = Phase::Submitting;
        self.log(
            format!(
                "提交任务 model={} resolution={} ratio={} duration={} audio={} 参考图={}张",
                config.model,
                config.resolution,
                config.ratio,
                if config.duration < 0 {
                    "智能".to_string()
                } else {
                    format!("{}s", config.duration)
                },
                config.audio,
                self.images.len()
            ),
            cx,
        );

        let started = Instant::now();
        let create_config = config.clone();

        cx.spawn(async move |this, cx| {
            let request_config = create_config.clone();
            let created = offload(cx, move || api::create_task(&request_config, &request)).await;

            let task_id = match created {
                Ok(task_id) => task_id,
                Err(error) => {
                    update_view(&this, cx, |app, cx| app.fail(format!("{error:#}"), cx));
                    return;
                }
            };

            update_view(&this, cx, |app, cx| {
                app.phase = Phase::Polling {
                    status: SharedString::from("PENDING"),
                    polls: 0,
                };
                app.log(format!("任务已创建：{task_id}"), cx);
            });

            let outcome = poll_until_done(cx, &create_config, &task_id, |cx, status, poll| {
                update_view(&this, cx, |app, cx| {
                    app.phase = Phase::Polling {
                        status: SharedString::from(status),
                        polls: poll,
                    };
                    // 每 4 次（约 1 分钟）报一次心跳，避免刷屏
                    if poll % POLL_LOG_EVERY == 0 {
                        app.log(
                            format!(
                                "{status} · 已等待 {}",
                                task::format_elapsed(started.elapsed())
                            ),
                            cx,
                        );
                    }
                });
            })
            .await;

            match outcome {
                Ok(PollOutcome::Succeeded { video_url, usage }) => {
                    let elapsed = started.elapsed();
                    let destination = output_path(&create_config.output_dir);
                    update_view(&this, cx, |app, cx| {
                        app.begin_download(elapsed, usage.as_ref(), cx)
                    });

                    let downloaded = download_with_progress(
                        cx,
                        &video_url,
                        &destination,
                        |cx, received, total| {
                            update_view(&this, cx, |app, _| {
                                app.phase = Phase::Downloading { received, total };
                            });
                        },
                    )
                    .await;

                    update_view(&this, cx, |app, cx| {
                        app.finish_download(video_url, destination, usage, downloaded, elapsed, cx)
                    });
                }
                Ok(PollOutcome::Failed { code, message }) => {
                    update_view(&this, cx, |app, cx| {
                        app.fail(format!("任务失败：{code} {message}"), cx)
                    });
                }
                Err(error) => update_view(&this, cx, |app, cx| app.fail(error, cx)),
            }
        })
        .detach();
    }

    // ---------------------------------------------------------------- 渲染

    fn endpoint_summary(&self, cx: &Context<Self>) -> SharedString {
        let workspace = self.workspace_id.read(cx).value().trim().to_string();
        let (region_name, region) = REGIONS[self.region_index];
        if workspace.is_empty() {
            SharedString::from(format!("端点 dashscope.aliyuncs.com · {region_name}"))
        } else {
            SharedString::from(format!("端点 {workspace}.{region}.maas.aliyuncs.com"))
        }
    }

    fn render_header(&mut self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        h_flex()
            .w_full()
            .items_center()
            .justify_between()
            .gap_4()
            .px_5()
            .py_3()
            .border_b_1()
            .border_color(p.border)
            .child(
                v_flex()
                    .gap_1()
                    .child(div().text_lg().child("万相 3.0 视频生成"))
                    .child(ui::hint(p, "全能参考：参考图 + 提示词 → 视频")),
            )
            .child(
                h_flex()
                    .gap_3()
                    .items_center()
                    .child(ui::hint(p, self.endpoint_summary(cx)))
                    .child(
                        Switch::new("single-instance-switch")
                            .label("单实例模式 · 重启生效")
                            .checked(self.single_instance)
                            .on_change(cx.listener(|this, checked: &bool, _, cx| {
                                this.single_instance = *checked;
                                this.mark_config_dirty(CONFIG_SAVE_DELAY, cx);
                            })),
                    )
                    .child(
                        Button::new("open-settings")
                            .label("连接设置")
                            .outline()
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.open_settings_dialog(window, cx)
                            })),
                    ),
            )
            .into_any_element()
    }

    fn render_prompt(&mut self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let status = self
            .prompt_status
            .clone()
            .unwrap_or_else(|| SharedString::from("已保存"));
        let body = v_flex()
            .gap_2()
            .w_full()
            .child(
                h_flex().w_full().items_center().gap_3().child(
                    Button::new("manage-prompt-templates")
                        .label("管理提示词模版")
                        .primary()
                        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                            this.open_prompt_templates_dialog(window, cx)
                        })),
                ),
            )
            .child(ui::scroll_contained(Textarea::new(&self.prompt).w_full()))
            .child(ui::hint(
                p,
                "最多 20000 字符；用「图1」「图2」指代参考图的排列顺序。",
            ))
            .child(ui::hint(
                p,
                format!(
                    "提示词修改后 3 秒自动保存到 {} · {status}",
                    Config::prompt_file_path().display()
                ),
            ))
            .into_any_element();

        ui::section(p, Some("提示词 prompt"), None, body)
    }

    fn render_images(&mut self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let count = self.images.len();

        // 列表为空时换成一行提示，避免 0 高度的虚拟列表
        let list: AnyElement = if count == 0 {
            ui::hint(
                p,
                "还没有参考图：点击「选择本地图片」，或把图片直接拖进这个区域，也可以粘贴公网的图片地址。",
            )
        } else {
            // 列表滚到边界后，滚轮不再带动外面整页
            ui::scroll_contained(List::new(&self.list_state).max_h(px(ui::IMAGE_LIST_MAX_HEIGHT)))
        };

        let controls = h_flex()
            .gap_2()
            .w_full()
            .child(
                Button::new("pick-images")
                    .label("选择本地图片")
                    .primary()
                    .on_click(
                        cx.listener(|this, _: &ClickEvent, _, cx| this.pick_local_images(cx)),
                    ),
            )
            .child(Input::new(&self.url_input).flex_1())
            .child(
                Button::new("add-remote-image")
                    .label("添加链接")
                    .outline()
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.add_remote_image(window, cx)
                    })),
            )
            .child(
                Button::new("clear-images")
                    .label("清空")
                    .outline()
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.images.clear();
                        this.sync_image_list(cx);
                        cx.notify();
                    })),
            );

        let body = v_flex()
            .gap_3()
            .w_full()
            .child(
                div()
                    .text_sm()
                    .text_color(p.muted_fg)
                    .child(SharedString::from(format!(
                        "已选 {count}/{} 张 · 按住某一行拖动可调整顺序 · 本地图片自动转 Base64 上传（单张 ≤ 20MB，超限会自动压缩）",
                        media::MAX_IMAGES
                    ))),
            )
            .child(controls)
            .child(list)
            .can_drop(|drag: &dyn std::any::Any, _, _| drag.is::<ExternalPaths>())
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                this.add_local_paths(paths.paths().to_vec(), cx);
            }))
            .into_any_element();

        ui::section(
            p,
            Some("参考图 reference_image"),
            Some("最多 10 张参考图"),
            body,
        )
    }

    fn render_params(&mut self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let resolution_options: Vec<(&'static str, &'static str)> =
            RESOLUTIONS.iter().map(|value| (*value, *value)).collect();
        let ratio_options: Vec<(&'static str, &'static str)> =
            RATIOS.iter().map(|value| (*value, *value)).collect();
        let model_options: Vec<(&'static str, &'static str)> = MODELS.to_vec();

        let body = v_flex()
            .gap_4()
            .w_full()
            .child(ui::field_row(
                p,
                "清晰度 resolution",
                ui::segmented(
                    "resolution",
                    &resolution_options,
                    RESOLUTIONS[self.resolution_index],
                    cx,
                    |this: &mut WanApp, value, cx| {
                        if let Some(index) = RESOLUTIONS.iter().position(|v| *v == value) {
                            this.resolution_index = index;
                            this.mark_config_dirty(CONFIG_SAVE_DELAY, cx);
                        }
                    },
                ),
            ))
            .child(ui::field_row(
                p,
                "比例 ratio",
                ui::segmented(
                    "ratio",
                    &ratio_options,
                    RATIOS[self.ratio_index],
                    cx,
                    |this: &mut WanApp, value, cx| {
                        if let Some(index) = RATIOS.iter().position(|v| *v == value) {
                            this.ratio_index = index;
                            this.mark_config_dirty(CONFIG_SAVE_DELAY, cx);
                        }
                    },
                ),
            ))
            .child(ui::field_row(
                p,
                "模型 model",
                ui::segmented(
                    "model",
                    &model_options,
                    MODELS[self.model_index].1,
                    cx,
                    |this: &mut WanApp, value, cx| {
                        if let Some(index) = MODELS.iter().position(|(_, v)| *v == value) {
                            this.model_index = index;
                            this.mark_config_dirty(CONFIG_SAVE_DELAY, cx);
                        }
                    },
                ),
            ))
            .child(
                h_flex()
                    .gap_6()
                    .items_start()
                    .w_full()
                    .child(ui::field_row(
                        p,
                        "时长 duration（2~30 秒）",
                        h_flex()
                            .gap_3()
                            .items_center()
                            .child(
                                NumberInput::new(&self.duration)
                                    .w(px(120.))
                                    .disabled(self.smart_duration),
                            )
                            .child(
                                Switch::new("smart-duration")
                                    .label("智能时长")
                                    .checked(self.smart_duration)
                                    .on_change(cx.listener(|this, checked: &bool, _, cx| {
                                        this.smart_duration = *checked;
                                        this.mark_config_dirty(CONFIG_SAVE_DELAY, cx);
                                    })),
                            )
                            .into_any_element(),
                    ))
                    .child(ui::field_row(
                        p,
                        "声音 audio",
                        Switch::new("audio-switch")
                            .label("输出视频包含声音")
                            .checked(self.audio)
                            .on_change(cx.listener(|this, checked: &bool, _, cx| {
                                this.audio = *checked;
                                this.mark_config_dirty(CONFIG_SAVE_DELAY, cx);
                            }))
                            .into_any_element(),
                    )),
            )
            .into_any_element();

        ui::section(
            p,
            Some("生成参数 parameters"),
            Some(
                "修改后自动保存到 config.json；resolution / ratio / duration / audio 会原样写入请求体",
            ),
            body,
        )
    }

    fn render_result(&mut self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let Some(result) = self.result.as_ref() else {
            return div().into_any_element();
        };

        let video_url = result.video_url.clone();
        let saved = result.saved.clone();
        let elapsed = task::format_elapsed(result.elapsed);
        let usage_text = result.usage.as_ref().map(|usage| usage.describe());

        let saved_text = match saved.as_ref() {
            Some(path) => format!("已保存：{}", path.display()),
            None => "未保存到本地（链接 24 小时内有效）".to_string(),
        };

        let mut body = v_flex()
            .gap_2()
            .w_full()
            .child(div().text_sm().child(SharedString::from(saved_text)))
            .child(
                div()
                    .text_sm()
                    .text_color(p.muted_fg)
                    .child(SharedString::from(format!("生成耗时 {elapsed}"))),
            );

        if let Some(usage_text) = usage_text {
            body = body.child(
                div()
                    .text_sm()
                    .text_color(p.muted_fg)
                    .child(SharedString::from(usage_text)),
            );
        }

        body = body.child(
            div()
                .text_sm()
                .text_color(p.muted_fg)
                .child(SharedString::from(video_url.clone())),
        );

        let mut actions = h_flex().gap_2();

        if let Some(path) = saved.clone() {
            let play_path = path.clone();
            actions = actions.child(
                Button::new("open-video")
                    .label("播放视频")
                    .primary()
                    .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                        cx.open_with_system(&play_path);
                    })),
            );
            let folder = path.clone();
            actions = actions.child(
                Button::new("reveal-video")
                    .label("打开所在文件夹")
                    .outline()
                    .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                        cx.reveal_path(&folder);
                    })),
            );
        }

        let link = video_url.clone();
        actions = actions.child(
            Button::new("open-link")
                .label("打开视频链接")
                .outline()
                .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                    cx.open_url(&link);
                })),
        );

        body = body.child(actions);

        ui::section(p, Some("生成结果"), None, body.into_any_element())
    }

    fn render_footer(&mut self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let busy = self.phase.is_busy();
        let color = match self.phase.kind() {
            PhaseKind::Idle => p.muted_fg,
            PhaseKind::Busy | PhaseKind::Done => p.primary,
            PhaseKind::Failed => p.danger,
        };
        // 阶段自己说不清的时候，回落到最近一条提示
        let detail = self
            .phase
            .detail()
            .or_else(|| self.message.as_ref().map(|message| message.to_string()))
            .unwrap_or_default();
        let progress = self.phase.progress();

        v_flex()
            .w_full()
            .child(
                v_flex()
                    .gap_2()
                    .w_full()
                    .px_5()
                    .py_3()
                    .border_t_1()
                    .border_color(p.border)
                    .child(
                        h_flex().gap_3().items_center().justify_between().child(
                            Button::new("generate")
                                .label(if busy { "生成中…" } else { "开始生成" })
                                .primary()
                                .loading(busy)
                                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                    this.start_generation(cx)
                                })),
                        ),
                    ),
            )
            .when(busy, |this| {
                this.child(
                    Progress::new("run-progress")
                        .xsmall()
                        .rounded(px(0.))
                        .value(progress.unwrap_or(0.))
                        .loading(progress.is_none()),
                )
            })
            .child(
                StatusBar::new().left(
                    h_flex()
                        .gap_3()
                        .min_w(px(0.0))
                        .child(
                            div()
                                .text_sm()
                                .font_bold()
                                .text_color(color)
                                .child(self.phase.label()),
                        )
                        .child(
                            div()
                                .text_sm()
                                .text_color(p.muted_fg)
                                .overflow_hidden()
                                .text_ellipsis()
                                .flex_1()
                                .child(SharedString::from(detail)),
                        ),
                ),
            )
            .into_any_element()
    }
}

impl Render for WanApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 把排队的过程信息/错误推到窗口右上角的 Notification 层
        if !self.notifications.is_empty() {
            let notes = std::mem::take(&mut self.notifications);
            window.defer(cx, move |window, cx| {
                for note in notes {
                    window.push_notification(note, cx);
                }
            });
        }

        // 未配置 API Key 时自动弹出连接设置
        if !self.settings_auto_opened {
            self.settings_auto_opened = true;
            if self.api_key.read(cx).value().trim().is_empty() {
                let entity = cx.entity();
                window.defer(cx, move |window, cx| {
                    entity.update(cx, |this, cx| this.open_settings_dialog(window, cx));
                });
            }
        }

        // 检查到远端仓库最新发布版的版本号大于本地版本时，弹出窗口提示手动下载更新
        if !self.update_available_dialog_opened {
            self.update_available_dialog_opened = true;
            cx.spawn_in(window, async move |this, cx| {
                this.update_in(cx, |state, window, cx| {
                    // GitHub 与 Gitee 的 release JSON 结构相同，依次尝试
                    const SOURCES: [&str; 2] = [
                        "https://gitee.com/api/v5/repos/YBKF/ali-wan3-tool/releases/latest",
                        "https://api.github.com/repos/YBKF/ali-wan3-tool/releases/latest",
                    ];

                    let Ok(local) = Version::parse(VERSION) else {
                        state.warn(format!("本地版本号无法解析：{VERSION}"), cx);
                        return;
                    };

                    let mut remote = None;
                    for (i, url) in SOURCES.iter().enumerate() {
                        let fetched = ureq::get(*url)
                            .call()
                            .map_err(|e| e.to_string())
                            .and_then(|mut r| {
                                r.body_mut()
                                    .read_json::<RepoVersion>()
                                    .map_err(|e| e.to_string())
                            })
                            .and_then(|repo| Version::try_from(repo).map_err(|e| e.to_string()));

                        match fetched {
                            Ok(version) => {
                                remote = Some(version);
                                break;
                            }
                            // 还有备用源时提示切换，最后一个源失败就直接报错
                            Err(e) if i + 1 < SOURCES.len() => {
                                state.warn(format!("检查更新：{e} 已切换至下一个源"), cx)
                            }
                            Err(e) => state.warn(format!("检查更新：{e}"), cx),
                        }
                    }

                    // 远端版本大于本地版本则弹出更新弹窗
                    if remote.is_some_and(|remote| remote > local) {
                        state.open_update_available_dialog(window, cx);
                    }
                })
                .ok();
            })
            .detach();
        }

        let p = ui::Palette::new(cx);

        let header = self.render_header(&p, cx);
        let params = self.render_params(&p, cx);
        let images = self.render_images(&p, cx);
        let prompt = self.render_prompt(&p, cx);
        let result = self.render_result(&p, cx);
        let footer = self.render_footer(&p, cx);

        v_flex()
            .size_full()
            .bg(p.bg)
            .text_color(p.fg)
            .child(header)
            .child(
                div().id("main-scroll").flex_1().overflow_y_scroll().child(
                    v_flex()
                        .w_full()
                        .gap_4()
                        .p_5()
                        .child(params)
                        .child(images)
                        .child(prompt)
                        .child(result),
                ),
            )
            .child(footer)
    }
}
