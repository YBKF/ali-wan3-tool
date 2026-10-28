//! 连接设置弹窗：API Key、业务空间 ID、地域、输出目录。
//!
//! 内容做成独立视图，而不是写在弹窗 builder 的闭包里：builder 不保证每次状态变化都重跑，
//! 做成 `Render` 实体后，点地域才能立刻重绘选中态。

use gpui_kit::component::{
    button::Button,
    h_flex,
    input::{Input, InputState},
    v_flex,
};
use gpui_kit::prelude::*;
use gpui_kit::{
    ClickEvent, Context, Entity, PathPromptOptions, Render, SharedString, WeakEntity, Window, div,
};

use crate::app::WanApp;
use crate::config::{Config, REGIONS};
use crate::ui::{self, Palette};

pub struct SettingsForm {
    owner: WeakEntity<WanApp>,
    api_key: Entity<InputState>,
    workspace_id: Entity<InputState>,
    output_dir: Entity<InputState>,
    /// 草稿值：点「保存」才回写主视图
    region_index: usize,
    saved_hint: Option<SharedString>,
}

impl SettingsForm {
    pub fn new(
        owner: WeakEntity<WanApp>,
        api_key: Entity<InputState>,
        workspace_id: Entity<InputState>,
        output_dir: Entity<InputState>,
        region_index: usize,
    ) -> Self {
        Self {
            owner,
            api_key,
            workspace_id,
            output_dir,
            region_index,
            saved_hint: None,
        }
    }

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

    /// 回写主视图并落盘。
    pub(crate) fn save(&mut self, cx: &mut Context<Self>) {
        let region_index = self.region_index;

        let outcome = self
            .owner
            .update(cx, |app, cx| {
                app.apply_connection_settings(region_index, cx)
            })
            .unwrap_or(Ok(()));

        self.saved_hint = Some(match outcome {
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

        let mut body = v_flex()
            .gap_3()
            .w_full()
            .child(ui::field_row(
                &p,
                "API Key（作为 Authorization: Bearer … 发送）",
                Input::new(&self.api_key).w_full().into_any_element(),
            ))
            .child(ui::field_row(
                &p,
                "业务空间 ID（留空则回退经典 DashScope 域名）",
                Input::new(&self.workspace_id).w_full().into_any_element(),
            ))
            .child(ui::field_row(
                &p,
                "地域（必须与 API Key、模型同地域）",
                ui::segmented(
                    "region",
                    &region_options,
                    REGIONS[region_index].1,
                    cx,
                    |this: &mut Self, value, _cx| {
                        if let Some(index) = REGIONS.iter().position(|(_, v)| *v == value) {
                            this.region_index = index;
                        }
                    },
                ),
            ))
            .child(ui::field_row(
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
            .child(ui::hint(
                &p,
                format!("配置文件：{}", Config::file_path().display()),
            ));

        if let Some(saved_hint) = self.saved_hint.clone() {
            body = body.child(div().text_sm().text_color(p.primary).child(saved_hint));
        }

        body.into_any_element()
    }
}
