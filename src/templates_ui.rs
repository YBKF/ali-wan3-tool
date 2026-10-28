//! 提示词模版管理弹窗，以及它内部的「新建提示词模版」弹窗。
//!
//! 模版正文不在应用里编辑：编辑按钮直接用系统默认文本编辑器打开文件。
//! 这样既能舒舒服服改长提示词，也不会和主界面提示词的「停止输入 3 秒自动保存」互相覆盖。

use std::cell::Cell;
use std::path::PathBuf;

use gpui_kit::assets::IconName;
use gpui_kit::component::{
    Icon, Sizable, WindowExt as _,
    button::{Button, ButtonVariant, ButtonVariants as _},
    dialog::DialogFooter,
    h_flex,
    input::{Input, InputEvent, InputState},
    v_flex,
};
use gpui_kit::prelude::*;
use gpui_kit::{
    AnyElement, ClickEvent, Context, ElementId, Entity, Focusable as _, Render, SharedString,
    Subscription, WeakEntity, Window, div, px,
};

use crate::app::WanApp;
use crate::templates::{self, PromptTemplate};
use crate::ui::{self, Palette};

/// 列表最大高度，超出后列表内部滚动，不把弹窗撑破。
const LIST_MAX_HEIGHT: f32 = 340.;

// ------------------------------------------------------------------ 模版管理

/// 「提示词模版管理」弹窗的内容。
pub struct PromptTemplatesDialog {
    owner: WeakEntity<WanApp>,
    dir: PathBuf,
    templates: Vec<PromptTemplate>,
    /// 正在改标题的那一行；`None` 表示当前没有在改名
    editing: Option<TitleEdit>,
    /// 一次性的操作结果提示（删除失败、模版为空等）
    notice: Option<SharedString>,
}

/// 正在改名的标题：标题就是文件名，回车即落盘。
struct TitleEdit {
    /// 改名前的文件路径，用来判断改的是哪一行、以及要移动哪个文件
    path: PathBuf,
    input: Entity<InputState>,
    /// 改名失败的原因（标题不合法 / 与别的模版重名），显示在输入框下面
    error: Option<SharedString>,
    /// 输入框事件订阅，随编辑状态一起丢弃
    _subscription: Subscription,
}

impl PromptTemplatesDialog {
    pub fn new(owner: WeakEntity<WanApp>) -> Self {
        let dir = templates::dir();
        Self {
            owner,
            templates: templates::list_in(&dir),
            dir,
            editing: None,
            notice: None,
        }
    }

    /// 重新读一遍目录（新建、删除、改名之后调用）。
    fn reload(&mut self, cx: &mut Context<Self>) {
        self.templates = templates::list_in(&self.dir);
        // 正在改名的那一行可能已经被删掉，编辑状态也就没意义了
        let editing_gone = self
            .editing
            .as_ref()
            .is_some_and(|editing| !self.templates.iter().any(|item| item.path == editing.path));
        if editing_gone {
            self.editing = None;
        }
        cx.notify();
    }

    /// 新建模版：先弹窗让用户填标题，确认后才落盘。
    fn open_new_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dir = self.dir.clone();
        let default_title = templates::next_title_in(&dir);
        let panel = cx.weak_entity();
        let title_input = ui::text_input(window, cx, "例如：人物转身特写", default_title);
        let form = cx.new(|_| NewTemplateForm::new(panel, dir, title_input.clone()));

        // 只在该弹窗第一次构建时抢焦点，之后重建不能把焦点从别处抢回来
        let initial_focus = Cell::new(true);
        window.open_dialog(cx, move |dialog, window, cx| {
            if initial_focus.replace(false) {
                title_input.focus_handle(cx).focus(window, cx);
            }

            let form = form.clone();
            let footer_form = form.clone();
            dialog
                .title("新建提示词模版")
                .w(px(480.))
                .child(form)
                .footer(
                    DialogFooter::new()
                        .child(
                            Button::new("new-template-cancel")
                                .label("取消")
                                .outline()
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("new-template-create")
                                .label("创建")
                                .primary()
                                .on_click(move |_, window, cx| {
                                    // 标题不合法时保持弹窗打开，错误显示在表单里
                                    if footer_form.update(cx, |form, cx| form.create(cx)) {
                                        window.close_dialog(cx);
                                    }
                                }),
                        ),
                )
        });
    }

    /// 打开模版保存文件夹（不存在就先建出来）。
    fn open_dir(&mut self, cx: &mut Context<Self>) {
        if let Err(error) = templates::ensure_dir(&self.dir) {
            self.notice = Some(SharedString::from(format!("{error:#}")));
            cx.notify();
            return;
        }
        cx.open_with_system(&self.dir);
    }

    /// 编辑：交给系统默认编辑器。
    fn edit(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if !path.exists() {
            self.notice = Some(SharedString::from("文件已不存在，列表已刷新"));
            self.reload(cx);
            return;
        }
        cx.open_with_system(&path);
    }

    /// 点标题进入改名：输入框预填当前标题并全选，点输入框里的对号保存。
    fn start_rename(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        template: PromptTemplate,
    ) {
        let input = ui::text_input(window, cx, "模版标题", template.title.clone());
        // 保存走输入框里的对号按钮，回车不提交；失焦算放弃
        let subscription = cx.subscribe(&input, |this, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Blur) {
                this.cancel_rename(cx);
            }
        });

        input.focus_handle(cx).focus(window, cx);
        input.update(cx, |input, cx| input.select_all(window, cx));

        self.editing = Some(TitleEdit {
            path: template.path,
            input,
            error: None,
            _subscription: subscription,
        });
        self.notice = None;
        cx.notify();
    }

    /// 点对号：校验 + 改文件名，成功后重新列目录，界面上的标题跟着文件名走。
    fn commit_rename(&mut self, cx: &mut Context<Self>) {
        let Some(editing) = self.editing.as_ref() else {
            return;
        };
        let title = editing.input.read(cx).value().trim().to_string();
        let path = editing.path.clone();

        if let Err(error) = templates::rename(&path, &title) {
            // 标题不合法或重名：留在编辑态，错误挂在输入框下面
            if let Some(editing) = self.editing.as_mut() {
                editing.error = Some(SharedString::from(format!("{error:#}")));
            }
            cx.notify();
            return;
        }

        self.editing = None;
        self.notice = None;
        self.reload(cx);
    }

    /// 放弃这次改名（点到别处时触发）：文件保持原样。
    fn cancel_rename(&mut self, cx: &mut Context<Self>) {
        if self.editing.take().is_some() {
            cx.notify();
        }
    }

    /// 删除前确认一次，避免误点把写好的提示词弄丢。
    fn confirm_delete(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        title: SharedString,
        path: PathBuf,
    ) {
        let panel = cx.weak_entity();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let panel = panel.clone();
            let path = path.clone();
            alert
                .confirm()
                .title(SharedString::from(format!("删除提示词模版「{title}」")))
                .description("删除后文件不会进入回收站，此操作无法撤销。")
                .ok_text("删除")
                .ok_variant(ButtonVariant::Danger)
                .cancel_text("取消")
                .on_ok(move |_, _, cx| {
                    let Some(panel) = panel.upgrade() else {
                        return true;
                    };
                    panel.update(cx, |panel, cx| {
                        panel.notice = match templates::delete(&path) {
                            Ok(()) => None,
                            Err(error) => Some(SharedString::from(format!("{error:#}"))),
                        };
                        panel.reload(cx);
                    });
                    true
                })
        });
    }

    /// 把模版正文替换到主界面提示词输入框，然后关掉管理弹窗。
    fn replace_to_prompt(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        template: PromptTemplate,
    ) {
        let text = match template.read() {
            Ok(text) => text,
            Err(error) => {
                self.notice = Some(SharedString::from(format!("{error:#}")));
                cx.notify();
                return;
            }
        };

        if text.trim().is_empty() {
            self.notice = Some(SharedString::from(
                "这个模版还是空的：点「编辑」按钮用编辑器写入提示词后再使用",
            ));
            cx.notify();
            return;
        }

        let Some(owner) = self.owner.upgrade() else {
            return;
        };
        owner.update(cx, |app, cx| app.replace_prompt(&text, window, cx));
        window.close_dialog(cx);
    }
}

impl Render for PromptTemplatesDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::new(cx);
        let templates = self.templates.clone();
        let dir = self.dir.clone();

        // 上方一行：左侧新建，右侧打开文件夹
        let controls = h_flex()
            .w_full()
            .justify_between()
            .items_center()
            .gap_3()
            .child(
                Button::new("new-prompt-template")
                    .label("新建提示词模版")
                    .primary()
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.open_new_dialog(window, cx)
                    })),
            )
            .child(
                Button::new("open-prompt-template-dir")
                    .label("打开提示词模版保存文件夹")
                    .outline()
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.open_dir(cx))),
            );

        let list: AnyElement = if templates.is_empty() {
            ui::hint(&p, "还没有提示词模版：点击「新建提示词模版」新建一个。")
        } else {
            let rows: Vec<AnyElement> = templates
                .iter()
                .map(|template| {
                    let title = template.title.clone();
                    let edit_path = template.path.clone();
                    let delete_path = template.path.clone();
                    let delete_title = SharedString::from(title.clone());
                    let added = template.clone();
                    let rename_target = template.clone();

                    // 这一行正在改名：标题位置换成输入框，错误提示挂在下面
                    let editing = self
                        .editing
                        .as_ref()
                        .filter(|edit| edit.path == template.path);
                    let title_cell: AnyElement = match editing {
                        Some(edit) => {
                            let save = Button::new(ElementId::Name(
                                format!("prompt-template-rename-save:{title}").into(),
                            ))
                            .icon(Icon::new(IconName::Check))
                            .ghost()
                            .xsmall()
                            .tooltip("保存修改")
                            .on_click(
                                cx.listener(|this, _: &ClickEvent, _, cx| this.commit_rename(cx)),
                            );

                            let mut cell = v_flex()
                                .flex_1()
                                .min_w(px(0.))
                                .gap_1()
                                .child(Input::new(&edit.input).small().suffix(save));
                            if let Some(error) = edit.error.clone() {
                                cell =
                                    cell.child(div().text_sm().text_color(p.danger).child(error));
                            }
                            cell.into_any_element()
                        }
                        None => div()
                            .id(ElementId::Name(
                                format!("prompt-template-title:{title}").into(),
                            ))
                            .flex_1()
                            .min_w(px(0.))
                            .text_sm()
                            .overflow_hidden()
                            .text_ellipsis()
                            .cursor_pointer()
                            .hover(|style| style.bg(p.muted))
                            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                this.start_rename(window, cx, rename_target.clone())
                            }))
                            .child(SharedString::from(title.clone()))
                            .into_any_element(),
                    };

                    h_flex()
                        .w_full()
                        .gap_3()
                        .items_center()
                        .px_3()
                        .py_2()
                        .rounded(p.radius)
                        .border_b_1()
                        .border_color(p.border)
                        .child(title_cell)
                        .child(
                            Button::new(ElementId::Name(
                                format!("prompt-template-edit:{title}").into(),
                            ))
                            .icon(Icon::new(IconName::Pencil))
                            .ghost()
                            .small()
                            .tooltip("编辑内容")
                            .on_click(cx.listener(
                                move |this, _: &ClickEvent, _, cx| this.edit(edit_path.clone(), cx),
                            )),
                        )
                        .child(
                            Button::new(ElementId::Name(
                                format!("prompt-template-delete:{title}").into(),
                            ))
                            .icon(Icon::new(IconName::Trash))
                            .ghost()
                            .small()
                            .tooltip("删除")
                            .on_click(cx.listener(
                                move |this, _: &ClickEvent, window, cx| {
                                    this.confirm_delete(
                                        window,
                                        cx,
                                        delete_title.clone(),
                                        delete_path.clone(),
                                    )
                                },
                            )),
                        )
                        .child(
                            Button::new(ElementId::Name(
                                format!("prompt-template-add:{title}").into(),
                            ))
                            .label("使用此模版")
                            .outline()
                            .text_color(p.primary)
                            .on_click(cx.listener(
                                move |this, _: &ClickEvent, window, cx| {
                                    this.replace_to_prompt(window, cx, added.clone())
                                },
                            )),
                        )
                        .into_any_element()
                })
                .collect();

            div()
                .id("prompt-template-list")
                .p_4()
                .w_full()
                .rounded(p.radius_lg)
                .border_1()
                .border_color(p.border)
                .max_h(px(LIST_MAX_HEIGHT))
                .overflow_y_scroll()
                .child(v_flex().w_full().children(rows))
                .into_any_element()
        };

        let mut body = v_flex()
            .w_full()
            .gap_3()
            .child(controls)
            .child(list)
            .child(ui::hint(
                &p,
                format!(
                    "点击模版标题即可修改标题，改完后点击输入框里的对号保存。模版目录：{}（每个模版是一个 .prompt.txt 文件）",
                    dir.display()
                ),
            ));

        if let Some(notice) = self.notice.clone() {
            body = body.child(div().text_sm().text_color(p.danger).child(notice));
        }

        body
    }
}

// ------------------------------------------------------------------ 新建模版

/// 「新建提示词模版」弹窗的表单：标题输入框 + 校验错误。
///
/// 做成独立视图，校验失败时才能原地重绘错误提示而不关窗。
struct NewTemplateForm {
    panel: WeakEntity<PromptTemplatesDialog>,
    dir: PathBuf,
    title: Entity<InputState>,
    error: Option<SharedString>,
}

impl NewTemplateForm {
    fn new(
        panel: WeakEntity<PromptTemplatesDialog>,
        dir: PathBuf,
        title: Entity<InputState>,
    ) -> Self {
        Self {
            panel,
            dir,
            title,
            error: None,
        }
    }

    /// 校验标题 → 建文件 → 用系统编辑器打开；返回是否可以关闭弹窗。
    fn create(&mut self, cx: &mut Context<Self>) -> bool {
        let title = self.title.read(cx).value().trim().to_string();

        match templates::create_in(&self.dir, &title) {
            Ok(path) => {
                if let Some(panel) = self.panel.upgrade() {
                    panel.update(cx, |panel, cx| panel.reload(cx));
                }
                cx.open_with_system(&path);
                self.error = None;
                true
            }
            Err(error) => {
                self.error = Some(SharedString::from(format!("{error:#}")));
                cx.notify();
                false
            }
        }
    }
}

impl Render for NewTemplateForm {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::new(cx);

        let mut body = v_flex()
            .w_full()
            .gap_2()
            .child(ui::hint(
                &p,
                "请输入模版标题：标题就是文件名（不用带 .prompt.txt 后缀）。确认后会创建文件并用系统默认文本编辑器打开。",
            ))
            .child(ui::field_row(
                &p,
                "模版标题",
                Input::new(&self.title).w_full().into_any_element(),
            ));

        if let Some(error) = self.error.clone() {
            body = body.child(div().text_sm().text_color(p.danger).child(error));
        }

        body.child(ui::hint(&p, format!("保存位置：{}", self.dir.display())))
    }
}
