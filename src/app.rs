//! 主界面：参考图 + 提示词 + 参数 → 创建任务 → 轮询 → 自动下载。
//!
//! 持久化约定：
//! - 连接设置（API Key / 业务空间 ID / 地域 / 输出目录）在弹窗里编辑，点「保存」写入 config.json
//! - 生成参数一旦被修改就自动写入 config.json（短暂防抖，避免连续点击写多次）
//! - 提示词单独存到 prompt.txt，停止输入 3 秒后自动保存

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use gpui_kit::base::StyledExt;
use gpui_kit::component::Sizable;
use gpui_kit::component::status_bar::StatusBar;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    dialog::DialogFooter,
    h_flex,
    input::{Input, InputEvent, InputState, NumberInput, Textarea, TextareaState},
    label::Label,
    notification::Notification,
    progress::Progress,
    switch::Switch,
    v_flex,
};
use gpui_kit::prelude::*;
use gpui_kit::{
    AnyElement, App, ClickEvent, Context, ElementId, Entity, ExternalPaths, Hsla, ObjectFit,
    PathPromptOptions, Pixels, Render, SharedString, Subscription, WeakEntity, Window, div, img,
    px,
};

use crate::api::{self, CreateRequest, MediaItem, Parameters, RequestInput, TaskOutcome, Usage};
use crate::config::{Config, MAX_DURATION, MIN_DURATION, MODELS, RATIOS, REGIONS, RESOLUTIONS};
use crate::media::{self, RefImage};

/// 提示词停止输入后多久落盘
const PROMPT_SAVE_DELAY: Duration = Duration::from_secs(3);
/// 参数修改后多久落盘（防抖，连续点击只写一次）
const CONFIG_SAVE_DELAY: Duration = Duration::from_millis(400);

#[derive(Clone, Copy)]
struct Palette {
    bg: Hsla,
    fg: Hsla,
    primary: Hsla,
    muted: Hsla,
    muted_fg: Hsla,
    border: Hsla,
    danger: Hsla,
    radius: Pixels,
    radius_lg: Pixels,
}

impl Palette {
    fn new(cx: &App) -> Self {
        let theme = cx.theme();
        Self {
            bg: theme.background,
            fg: theme.foreground,
            primary: theme.primary,
            muted: theme.muted,
            muted_fg: theme.muted_foreground,
            border: theme.border,
            danger: theme.danger,
            radius: theme.radius,
            radius_lg: theme.radius_lg,
        }
    }
}

#[derive(Clone)]
enum Phase {
    Idle,
    Submitting,
    Polling { status: SharedString, polls: u32 },
    Downloading { received: u64, total: u64 },
    Done,
    Failed(SharedString),
}

struct ResultInfo {
    video_url: String,
    saved: Option<PathBuf>,
    usage: Option<Usage>,
    elapsed: String,
}

// ------------------------------------------------------------------ 通用渲染部件

fn section(
    p: &Palette,
    title: &'static str,
    hint: Option<&'static str>,
    body: AnyElement,
) -> AnyElement {
    let mut head = v_flex().gap_1().child(Label::new(title));
    if let Some(hint) = hint {
        head = head.child(div().text_sm().text_color(p.muted_fg).child(hint));
    }
    v_flex()
        .gap_3()
        .p_4()
        .w_full()
        .rounded(p.radius_lg)
        .border_1()
        .border_color(p.border)
        .child(head)
        .child(body)
        .into_any_element()
}

fn field_row(p: &Palette, label: &str, control: AnyElement) -> AnyElement {
    v_flex()
        .gap_2()
        .w_full()
        .child(
            div()
                .text_sm()
                .text_color(p.muted_fg)
                .child(SharedString::from(label.to_string())),
        )
        .child(control)
        .into_any_element()
}

fn options_row<T, F>(
    prefix: &'static str,
    options: &[(&'static str, &'static str)],
    selected: &str,
    cx: &mut Context<T>,
    on_pick: F,
) -> AnyElement
where
    T: 'static,
    F: Fn(&mut T, &'static str, &mut Context<T>) + Clone + 'static,
{
    let buttons: Vec<AnyElement> = options
        .iter()
        .map(|(label, value)| {
            let value: &'static str = value;
            let active = value == selected;
            let handler = on_pick.clone();
            let id = ElementId::Name(format!("{prefix}:{value}").into());
            let button = Button::new(id).label(SharedString::from(*label));
            let button = if active {
                button.primary()
            } else {
                button.outline()
            };
            button
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    handler(this, value, cx);
                    cx.notify();
                }))
                .into_any_element()
        })
        .collect();

    h_flex()
        .flex_wrap()
        .gap_2()
        .children(buttons)
        .into_any_element()
}

// ------------------------------------------------------------------ 连接设置弹窗

/// 弹窗内容做成独立视图，改动地域后能立刻重绘（弹窗 builder 不保证重跑）。
struct SettingsForm {
    owner: WeakEntity<WanApp>,
    api_key: Entity<InputState>,
    workspace_id: Entity<InputState>,
    output_dir: Entity<InputState>,
    region_index: usize,
    hint: Option<SharedString>,
}

impl SettingsForm {
    fn pick_output_dir(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(SharedString::from("选择视频保存目录")),
        });

        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(paths))) = receiver.await
                && let Some(first) = paths.into_iter().next()
            {
                this.update_in(cx, |this, window, cx| {
                    let value = SharedString::from(first.to_string_lossy().to_string());
                    this.output_dir.update(cx, |state, cx| {
                        state.set_value(value, window, cx);
                    });
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let region_index = self.region_index;
        let owner = self.owner.clone();

        let outcome = owner
            .update(cx, |this, cx| {
                this.region_index = region_index;
                let config = this.snapshot_config(cx);
                let saved = config.save();
                this.config = config;
                cx.notify();
                saved
            })
            .unwrap_or(Ok(()));

        self.hint = Some(match outcome {
            Ok(()) => SharedString::from("已保存"),
            Err(error) => SharedString::from(format!("保存失败：{error}")),
        });
        cx.notify();
    }
}

impl Render for SettingsForm {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::new(cx);
        let region_options: Vec<(&'static str, &'static str)> = REGIONS.to_vec();
        let region_index = self.region_index;
        let file_path = Config::file_path().display().to_string();
        let hint = self.hint.clone();

        let mut body = v_flex()
            .gap_3()
            .w_full()
            .child(field_row(
                &p,
                "API Key（作为 Authorization: Bearer … 发送）",
                Input::new(&self.api_key).w_full().into_any_element(),
            ))
            .child(field_row(
                &p,
                "业务空间 ID（留空则回退经典 DashScope 域名）",
                Input::new(&self.workspace_id).w_full().into_any_element(),
            ))
            .child(field_row(
                &p,
                "地域（必须与 API Key、模型同地域）",
                options_row(
                    "region",
                    &region_options,
                    REGIONS[region_index].1,
                    cx,
                    |this: &mut SettingsForm, value, _cx| {
                        if let Some(index) = REGIONS.iter().position(|(_, v)| *v == value) {
                            this.region_index = index;
                        }
                    },
                ),
            ))
            .child(field_row(
                &p,
                "输出目录（生成完成后自动下载到这里）",
                h_flex()
                    .gap_2()
                    .w_full()
                    .child(Input::new(&self.output_dir).flex_1())
                    .child(
                        Button::new("pick-output-dir")
                            .label("选择目录")
                            .outline()
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.pick_output_dir(window, cx)
                            })),
                    )
                    .into_any_element(),
            ))
            .child(
                div()
                    .text_sm()
                    .text_color(p.muted_fg)
                    .child(SharedString::from(format!("配置文件：{file_path}"))),
            );

        if let Some(hint) = hint {
            body = body.child(div().text_sm().text_color(p.primary).child(hint));
        }

        body.into_any_element()
    }
}

// ------------------------------------------------------------------ 主视图

pub struct WanApp {
    config: Config,

    api_key: Entity<InputState>,
    workspace_id: Entity<InputState>,
    output_dir: Entity<InputState>,
    region_index: usize,
    settings_auto_opened: bool,

    prompt: Entity<TextareaState>,
    url_input: Entity<InputState>,
    duration: Entity<InputState>,
    smart_duration: bool,
    resolution_index: usize,
    ratio_index: usize,
    model_index: usize,
    audio: bool,

    images: Vec<RefImage>,

    phase: Phase,
    progress: f32,
    /// 待推送的通知，在下一帧统一交给窗口右上角的 Notification 组件
    notifications: Vec<Notification>,
    message: Option<SharedString>,
    result: Option<ResultInfo>,

    /// 提示词防抖版本号，只有最后一次修改才会落盘
    prompt_revision: u64,
    prompt_dirty: bool,
    prompt_status: Option<SharedString>,
    config_revision: u64,

    _prompt_subscription: Subscription,
    _duration_subscription: Subscription,
}

impl WanApp {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let config = Config::load();

        let api_key = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("sk-…")
                .default_value(config.api_key.clone())
        });
        let workspace_id = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("业务空间 ID，留空则用 dashscope.aliyuncs.com")
                .default_value(config.workspace_id.clone())
        });
        let output_dir =
            cx.new(|cx| InputState::new(window, cx).default_value(config.output_dir.clone()));
        let prompt = cx.new(|cx| {
            TextareaState::new(window, cx)
                .rows(5)
                .placeholder("描述视频；可用「图1」「图2」指代上方的参考图")
                .default_value(Config::load_prompt())
        });
        let url_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("粘贴 https:// 图片地址"));

        let smart_duration = config.duration < 0;
        let duration_default = if smart_duration {
            5
        } else {
            config.duration.clamp(MIN_DURATION, MAX_DURATION)
        };
        let duration = cx.new(|cx| {
            InputState::new(window, cx)
                .min(MIN_DURATION as f64)
                .max(MAX_DURATION as f64)
                .default_value(duration_default.to_string())
        });

        let prompt_subscription = cx.subscribe(&prompt, |this, _state, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.schedule_prompt_save(cx);
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
            smart_duration,
            settings_auto_opened: false,
            config,
            api_key,
            workspace_id,
            output_dir,
            prompt,
            url_input,
            duration,
            images: Vec::new(),
            phase: Phase::Idle,
            progress: 0.0,
            notifications: Vec::new(),
            message: None,
            result: None,
            prompt_revision: 0,
            prompt_dirty: false,
            prompt_status: None,
            config_revision: 0,
            _prompt_subscription: prompt_subscription,
            _duration_subscription: duration_subscription,
        }
    }

    fn busy(&self) -> bool {
        matches!(
            self.phase,
            Phase::Submitting | Phase::Polling { .. } | Phase::Downloading { .. }
        )
    }

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
        config.duration = if self.smart_duration {
            -1
        } else {
            self.duration_seconds(cx)
        };
        config
    }

    /// 排队一条通知，下一帧由 `Render for WanApp` 推送到窗口右上角。
    fn push_note(&mut self, note: Notification, cx: &mut Context<Self>) {
        self.notifications.push(note);
        cx.notify();
    }

    /// 过程信息，以 info 通知呈现。
    fn log(&mut self, cx: &mut Context<Self>, message: impl Into<String>) {
        self.push_note(Notification::info(message.into()), cx);
    }

    /// 需要用户注意但不致命的问题，以 warning 通知呈现。
    fn warn(&mut self, message: impl Into<String>, cx: &mut Context<Self>) {
        let message = message.into();
        self.message = Some(SharedString::from(message.clone()));
        self.push_note(Notification::warning(message), cx);
    }

    /// 失败，以常驻的 error 通知呈现（需手动关闭）。
    fn fail(&mut self, message: impl Into<String>, cx: &mut Context<Self>) {
        let message = message.into();
        self.phase = Phase::Failed(SharedString::from(message.clone()));
        self.message = Some(SharedString::from(message.clone()));
        self.progress = 0.0;
        self.push_note(Notification::error(message).autohide(false), cx);
    }

    // ---------------------------------------------------------------- 持久化

    /// 生成参数一旦修改就落盘；用版本号做防抖，连续点击只写最后一次。
    fn mark_config_dirty(&mut self, delay: Duration, cx: &mut Context<Self>) {
        self.config_revision = self.config_revision.wrapping_add(1);
        let revision = self.config_revision;
        let config = self.snapshot_config(cx);
        self.config = config.clone();

        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            let is_latest = this
                .update(cx, |this, _| this.config_revision == revision)
                .unwrap_or(false);
            if !is_latest {
                return;
            }

            let saved = cx
                .background_executor()
                .spawn(async move { config.save() })
                .await;
            if let Err(error) = saved {
                this.update(cx, |this, cx| {
                    this.log(cx, format!("参数保存失败：{error:#}"));
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    /// 提示词停止输入 3 秒后写入 prompt.txt。
    fn schedule_prompt_save(&mut self, cx: &mut Context<Self>) {
        self.prompt_revision = self.prompt_revision.wrapping_add(1);
        self.prompt_dirty = true;
        self.prompt_status = Some(SharedString::from("待保存…"));
        let revision = self.prompt_revision;
        let text = self.prompt.read(cx).value().to_string();
        cx.notify();

        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(PROMPT_SAVE_DELAY).await;
            let is_latest = this
                .update(cx, |this, _| this.prompt_revision == revision)
                .unwrap_or(false);
            if !is_latest {
                return;
            }

            let saved = cx
                .background_executor()
                .spawn(async move { Config::save_prompt(&text) })
                .await;

            this.update(cx, |this, cx| {
                this.prompt_dirty = false;
                match saved {
                    Ok(()) => {
                        this.prompt_status = Some(SharedString::from("已保存"));
                    }
                    Err(error) => {
                        this.prompt_status = Some(SharedString::from(format!("保存失败：{error}")));
                        this.log(cx, format!("提示词保存失败：{error:#}"));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    // ---------------------------------------------------------------- 连接设置弹窗

    fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let owner = cx.weak_entity();
        let api_key = self.api_key.clone();
        let workspace_id = self.workspace_id.clone();
        let output_dir = self.output_dir.clone();
        let region_index = self.region_index;
        let form = cx.new(move |_| SettingsForm {
            owner,
            api_key,
            workspace_id,
            output_dir,
            region_index,
            hint: None,
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

        let room = media::MAX_IMAGES.saturating_sub(self.images.len());
        if room == 0 {
            self.warn("参考图最多 10 张", cx);
            return;
        }
        if files.len() > room {
            self.log(cx, format!("最多再添加 {room} 张，已忽略多余的图片"));
            files.truncate(room);
        }

        self.log(cx, format!("正在处理 {} 张图片…", files.len()));
        let task = cx.background_executor().spawn(async move {
            files
                .into_iter()
                .map(|path| {
                    let result = media::prepare_local(&path);
                    (path, result)
                })
                .collect::<Vec<_>>()
        });

        cx.spawn(async move |this, cx| {
            let results = task.await;
            this.update(cx, |this, cx| {
                for (path, result) in results {
                    match result {
                        Ok(image) => {
                            this.log(
                                cx,
                                format!("已添加参考图 {}（{}）", image.label, image.info),
                            );
                            this.images.push(image);
                        }
                        Err(error) => {
                            this.warn(format!("{}：{error:#}", path.display()), cx);
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn add_remote_image(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.images.len() >= media::MAX_IMAGES {
            self.warn("参考图最多 10 张", cx);
            return;
        }
        let url = self.url_input.read(cx).value().trim().to_string();
        match media::prepare_remote(&url) {
            Ok(image) => {
                self.log(cx, format!("已添加远程参考图 {}", image.label));
                self.images.push(image);
                self.url_input.update(cx, |state, cx| {
                    state.set_value(SharedString::from(""), window, cx);
                });
                cx.notify();
            }
            Err(error) => self.warn(format!("{error:#}"), cx),
        }
    }

    // ---------------------------------------------------------------- 生成流程

    fn start_generation(&mut self, cx: &mut Context<Self>) {
        if self.busy() {
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
            self.log(cx, format!("配置保存失败：{error}"));
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
        self.progress = 0.0;
        self.log(
            cx,
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
        );

        let started = Instant::now();
        let create_config = config.clone();

        cx.spawn(async move |this, cx| {
            let request_config = create_config.clone();
            let created = cx
                .background_executor()
                .spawn(async move { api::create_task(&request_config, &request) })
                .await;

            let task_id = match created {
                Ok(task_id) => task_id,
                Err(error) => {
                    this.update(cx, |this, cx| this.fail(format!("{error:#}"), cx))
                        .ok();
                    return;
                }
            };

            this.update(cx, |this, cx| {
                this.phase = Phase::Polling {
                    status: SharedString::from("PENDING"),
                    polls: 0,
                };
                this.log(cx, format!("任务已创建：{task_id}"));
                cx.notify();
            })
            .ok();

            for poll in 1..=api::MAX_POLLS {
                cx.background_executor()
                    .timer(Duration::from_secs(api::POLL_SECS))
                    .await;

                let query_config = create_config.clone();
                let query_id = task_id.clone();
                let outcome = cx
                    .background_executor()
                    .spawn(async move { api::query_task(&query_config, &query_id) })
                    .await;

                let is_running = matches!(outcome, Ok(TaskOutcome::Running));

                match outcome {
                    Ok(TaskOutcome::Pending) | Ok(TaskOutcome::Running) => {
                        let status = if is_running { "RUNNING" } else { "PENDING" };
                        let elapsed = started.elapsed().as_secs();
                        this.update(cx, |this, cx| {
                            this.phase = Phase::Polling {
                                status: SharedString::from(status),
                                polls: poll,
                            };
                            if poll % 4 == 0 {
                                this.log(cx, format!("{status} · 已等待 {elapsed} 秒"));
                            }
                            cx.notify();
                        })
                        .ok();
                    }
                    Ok(TaskOutcome::Succeeded { video_url, usage }) => {
                        let elapsed = format!("{:.0}s", started.elapsed().as_secs_f64());
                        let file_name = format!(
                            "wan3-{}.mp4",
                            chrono::Local::now().format("%Y%m%d-%H%M%S")
                        );
                        let destination = PathBuf::from(&create_config.output_dir).join(file_name);

                        this.update(cx, |this, cx| {
                            this.phase = Phase::Downloading {
                                received: 0,
                                total: 0,
                            };
                            this.log(cx, format!("生成完成（耗时 {elapsed}），开始下载…"));
                            if let Some(usage) = usage.as_ref() {
                                this.log(cx, format!("用量：{}", usage.describe()));
                            }
                            cx.notify();
                        })
                        .ok();

                        let received = Arc::new(AtomicU64::new(0));
                        let total = Arc::new(AtomicU64::new(0));
                        let outcome_store: Arc<Mutex<Option<Result<u64, String>>>> =
                            Arc::new(Mutex::new(None));

                        {
                            let url = video_url.clone();
                            let path = destination.clone();
                            let received = received.clone();
                            let total = total.clone();
                            let store = outcome_store.clone();
                            cx.background_executor()
                                .spawn(async move {
                                    let result = api::download_video(&url, &path, &received, &total)
                                        .map_err(|error| format!("{error:#}"));
                                    if let Ok(mut slot) = store.lock() {
                                        *slot = Some(result);
                                    }
                                })
                                .detach();
                        }

                        loop {
                            cx.background_executor()
                                .timer(Duration::from_millis(500))
                                .await;
                            let received_now = received.load(Ordering::Relaxed);
                            let total_now = total.load(Ordering::Relaxed);
                            this.update(cx, |this, cx| {
                                this.phase = Phase::Downloading {
                                    received: received_now,
                                    total: total_now,
                                };
                                cx.notify();
                            })
                            .ok();
                            let finished = outcome_store
                                .lock()
                                .map(|slot| slot.is_some())
                                .unwrap_or(false);
                            if finished {
                                break;
                            }
                        }

                        let downloaded =
                            outcome_store.lock().ok().and_then(|mut slot| slot.take());
                        match downloaded {
                            Some(Ok(bytes)) => {
                                let saved = destination.clone();
                                this.update(cx, |this, cx| {
                                    this.phase = Phase::Done;
                                    this.progress = 100.0;
                                    this.message = Some(SharedString::from(format!(
                                        "已保存到 {}",
                                        saved.display()
                                    )));
                                    this.log(cx, format!(
                                        "已保存 {}（{}）",
                                        saved.display(),
                                        media::human_size(bytes)
                                    ));
                                    this.result = Some(ResultInfo {
                                        video_url: video_url.clone(),
                                        saved: Some(saved),
                                        usage: usage.clone(),
                                        elapsed: elapsed.clone(),
                                    });
                                    cx.notify();
                                })
                                .ok();
                            }
                            Some(Err(error)) => {
                                this.update(cx, |this, cx| {
                                    this.warn(
                                        format!("视频已生成但下载失败：{error}。链接 24 小时内有效，请尽快手动下载"),
                                        cx,
                                    );
                                    this.phase = Phase::Done;
                                    this.result = Some(ResultInfo {
                                        video_url: video_url.clone(),
                                        saved: None,
                                        usage: usage.clone(),
                                        elapsed: elapsed.clone(),
                                    });
                                })
                                .ok();
                            }
                            None => {}
                        }
                        return;
                    }
                    Ok(TaskOutcome::Failed { code, message }) => {
                        this.update(cx, |this, cx| {
                            this.fail(format!("任务失败：{code} {message}"), cx)
                        })
                        .ok();
                        return;
                    }
                    Err(error) => {
                        this.update(cx, |this, cx| this.fail(format!("{error:#}"), cx))
                            .ok();
                        return;
                    }
                }
            }

            this.update(cx, |this, cx| {
                this.fail("任务超时：超过 40 分钟仍未完成，请改用查询接口或稍后重试", cx)
            })
            .ok();
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
                    .child(
                        div()
                            .text_sm()
                            .text_color(p.muted_fg)
                            .child("全能参考：参考图 + 提示词 → 视频"),
                    ),
            )
            .child(
                h_flex()
                    .gap_3()
                    .items_center()
                    .child(
                        div()
                            .text_sm()
                            .text_color(p.muted_fg)
                            .child(self.endpoint_summary(cx)),
                    )
                    .child(
                        Button::new("open-settings")
                            .label("连接设置")
                            .outline()
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.open_settings(window, cx)
                            })),
                    ),
            )
            .into_any_element()
    }

    fn render_prompt(&mut self, p: &Palette, _cx: &mut Context<Self>) -> AnyElement {
        let status = self
            .prompt_status
            .clone()
            .unwrap_or_else(|| SharedString::from("已保存"));
        let body = v_flex()
            .gap_2()
            .w_full()
            .child(Textarea::new(&self.prompt).w_full())
            .child(
                div()
                    .text_sm()
                    .text_color(p.muted_fg)
                    .child("最多 20000 字符；用「图1」「图2」指代参考图的排列顺序。"),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(p.muted_fg)
                    .child(SharedString::from(format!(
                        "提示词修改后 3 秒自动保存到 {} · {status}",
                        Config::prompt_file_path().display()
                    ))),
            )
            .into_any_element();

        section(p, "提示词 prompt", None, body)
    }

    fn render_images(&mut self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let mut list = v_flex().gap_2().w_full();

        if self.images.is_empty() {
            list = list.child(div().text_sm().text_color(p.muted_fg).child(
                "还没有参考图：点击「选择本地图片」，或把图片直接拖进窗口，也可以粘贴公网图片地址。",
            ));
        }

        for index in 0..self.images.len() {
            let image = self.images[index].clone();
            let mut row = h_flex()
                .gap_3()
                .items_center()
                .w_full()
                .p_2()
                .rounded(p.radius)
                .border_1()
                .border_color(p.border);

            row = match image.preview.clone() {
                Some(path) => row.child(
                    img(path)
                        .w(px(72.))
                        .h(px(54.))
                        .rounded(p.radius)
                        .object_fit(ObjectFit::Cover),
                ),
                None => row.child(
                    v_flex()
                        .w(px(72.))
                        .h(px(54.))
                        .items_center()
                        .justify_center()
                        .rounded(p.radius)
                        .bg(p.muted)
                        .child(div().text_sm().text_color(p.muted_fg).child("URL")),
                ),
            };

            let mut meta = v_flex()
                .gap_1()
                .flex_1()
                .child(
                    div()
                        .text_sm()
                        .child(SharedString::from(image.label.clone())),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(p.muted_fg)
                        .child(SharedString::from(image.info.clone())),
                );
            if let Some(note) = image.note.clone() {
                meta = meta.child(
                    div()
                        .text_sm()
                        .text_color(p.muted_fg)
                        .child(SharedString::from(note)),
                );
            }

            row = row.child(meta).child(
                Button::new(ElementId::Name(format!("remove-image:{index}").into()))
                    .label("移除")
                    .outline()
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        if index < this.images.len() {
                            this.images.remove(index);
                        }
                        cx.notify();
                    })),
            );

            list = list.child(row);
        }

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
                        "已选 {}/{} 张 · 本地图片自动转 Base64 上传（单张 ≤ 20MB，超限会自动压缩）",
                        self.images.len(),
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

        section(p, "参考图 reference_image", Some("最多 10 张参考图"), body)
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
            .child(field_row(
                p,
                "清晰度 resolution",
                options_row(
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
            .child(field_row(
                p,
                "比例 ratio",
                options_row(
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
            .child(field_row(
                p,
                "模型 model",
                options_row(
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
                    .child(field_row(
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
                    .child(field_row(
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

        section(
            p,
            "生成参数 parameters",
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
        let elapsed = result.elapsed.clone();
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

        section(p, "生成结果", None, body.into_any_element())
    }

    fn render_footer(&mut self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let busy = self.busy();
        let (label, color) = match &self.phase {
            Phase::Idle => ("待命", p.muted_fg),
            Phase::Submitting => ("提交任务中", p.primary),
            Phase::Polling { .. } => ("生成中", p.primary),
            Phase::Downloading { .. } => ("下载中", p.primary),
            Phase::Done => ("已完成", p.primary),
            Phase::Failed(_) => ("失败", p.danger),
        };

        let detail = match &self.phase {
            Phase::Polling { status, polls } if *polls == 0 => {
                format!("{status} · 等待首次查询（每 15 秒轮询一次）")
            }
            Phase::Polling { status, polls } => {
                format!("{status} · 第 {polls} 次轮询（每 15 秒一次）")
            }
            Phase::Downloading { received, total } if *total > 0 => format!(
                "{} / {}",
                media::human_size(*received),
                media::human_size(*total)
            ),
            Phase::Downloading { received, .. } => media::human_size(*received),
            Phase::Failed(message) => message.to_string(),
            _ => self
                .message
                .as_ref()
                .map(|message| message.to_string())
                .unwrap_or_default(),
        };

        let progress_value = match &self.phase {
            Phase::Downloading { received, total } if *total > 0 => {
                (*received as f32 / *total as f32) * 100.0
            }
            _ => self.progress,
        };

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
                        .value(progress_value)
                        .loading(matches!(self.phase, Phase::Polling { .. })),
                )
            })
            .child(
                StatusBar::new().left(
                    h_flex()
                        .gap_3()
                        .min_w(px(0.0))
                        .child(div().text_sm().font_bold().text_color(color).child(label))
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

        // 没配 API Key 时首帧自动弹出连接设置
        if !self.settings_auto_opened {
            self.settings_auto_opened = true;
            if self.api_key.read(cx).value().trim().is_empty() {
                let entity = cx.entity();
                window.defer(cx, move |window, cx| {
                    entity.update(cx, |this, cx| this.open_settings(window, cx));
                });
            }
        }

        let p = Palette::new(cx);

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
