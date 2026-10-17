//! 参考图列表：虚拟化 `List` 的 delegate，以及拖动重排。
//!
//! 顺序有语义——它就是提示词里「图1」「图2」的引用顺序，所以重排之后必须立刻写回主视图。

use gpui_kit::component::{
    ActiveTheme as _, IndexPath,
    button::Button,
    h_flex,
    list::{ListDelegate, ListItem, ListState},
    v_flex,
};
use gpui_kit::prelude::*;
use gpui_kit::{
    App, ClickEvent, Context, ElementId, ObjectFit, Render, SharedString, WeakEntity, Window, div,
    img, px,
};

use crate::app::WanApp;
use crate::media::RefImage;
use crate::ui::{self, Palette};

/// 把第 `from` 项挪到第 `to` 位；越界或原地不动返回 `false`。
fn reorder<T>(items: &mut Vec<T>, from: usize, to: usize) -> bool {
    if from == to || from >= items.len() || to >= items.len() {
        return false;
    }
    let item = items.remove(from);
    items.insert(to, item);
    true
}

/// 拖动参考图时跟随光标的预览。
#[derive(Clone)]
struct DraggedImage {
    row: usize,
    label: SharedString,
}

impl Render for DraggedImage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_3()
            .py_1()
            .rounded(cx.theme().radius)
            .bg(cx.theme().accent)
            .text_color(cx.theme().accent_foreground)
            .child(self.label.clone())
    }
}

/// 参考图列表的 delegate。
///
/// `List` 是虚拟化渲染，行数据必须由 delegate 自己持有：`render_item` 期间去读主视图
/// 会和主视图自己的 `Render` 借冲突。所以这里持有一份副本，增删改后整份写回。
pub struct RefImagesDelegate {
    owner: WeakEntity<WanApp>,
    items: Vec<RefImage>,
    selected: Option<IndexPath>,
}

impl RefImagesDelegate {
    pub fn new(owner: WeakEntity<WanApp>) -> Self {
        Self {
            owner,
            items: Vec::new(),
            selected: None,
        }
    }

    /// 用主视图的数据整体刷新（新增/添加链接/清空之后调用）。
    pub fn reset(&mut self, items: Vec<RefImage>) {
        self.items = items;
    }

    /// 把当前顺序整份写回主视图。返回是否真的移动了。
    fn move_item(&mut self, from: usize, to: usize) -> bool {
        reorder(&mut self.items, from, to)
    }

    fn remove_at(&mut self, row: usize) -> bool {
        if row >= self.items.len() {
            return false;
        }
        self.items.remove(row);
        true
    }

    fn push_to_owner(&self, cx: &mut Context<ListState<Self>>) {
        let Some(owner) = self.owner.upgrade() else {
            return;
        };
        let items = self.items.clone();
        owner.update(cx, |app, cx| app.apply_images(items, cx));
    }
}

impl ListDelegate for RefImagesDelegate {
    type Item = ListItem;

    fn items_count(&self, _section: usize, _cx: &App) -> usize {
        self.items.len()
    }

    fn render_item(
        &mut self,
        ix: IndexPath,
        _window: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) -> Option<Self::Item> {
        let row = ix.row;
        let image = self.items.get(row)?.clone();
        let p = Palette::new(cx);

        let mut tile = h_flex().gap_3().items_center().w_full().py_1();
        tile = match image.preview.clone() {
            Some(path) => tile.child(
                img(path)
                    .w(px(ui::THUMB_WIDTH))
                    .h(px(ui::THUMB_HEIGHT))
                    .rounded(p.radius)
                    .object_fit(ObjectFit::Cover),
            ),
            None => tile.child(
                v_flex()
                    .w(px(ui::THUMB_WIDTH))
                    .h(px(ui::THUMB_HEIGHT))
                    .items_center()
                    .justify_center()
                    .rounded(p.radius)
                    .bg(p.muted)
                    .child(div().text_sm().text_color(p.muted_fg).child("URL")),
            ),
        };

        // 列表要求每行等高：信息固定压成两行并截断，行高由缩略图决定
        let mut info = image.info.clone();
        if let Some(note) = image.note.as_ref() {
            info.push_str(" · ");
            info.push_str(note);
        }

        let meta = v_flex()
            .gap_1()
            .flex_1()
            .overflow_hidden()
            .child(
                div()
                    .text_sm()
                    .truncate()
                    .child(SharedString::from(image.label.clone())),
            )
            .child(
                div()
                    .text_sm()
                    .truncate()
                    .text_color(p.muted_fg)
                    .child(SharedString::from(info)),
            );

        tile = tile.child(meta).child(
            Button::new(ElementId::Name(format!("remove-image:{row}").into()))
                .label("移除")
                .outline()
                .on_click(cx.listener(move |state, _: &ClickEvent, _, cx| {
                    if state.delegate_mut().remove_at(row) {
                        state.delegate().push_to_owner(cx);
                    }
                    cx.notify();
                })),
        );

        Some(
            ListItem::new(ix)
                .child(tile)
                .selected(Some(ix) == self.selected)
                .on_drag(
                    DraggedImage {
                        row,
                        label: SharedString::from(image.label.clone()),
                    },
                    |drag, _, _, cx| cx.new(|_| drag.clone()),
                )
                .drag_over::<DraggedImage>(|style, _, _, cx| style.bg(cx.theme().drop_target))
                .on_drop(cx.listener(move |state, drag: &DraggedImage, _, cx| {
                    if state.delegate_mut().move_item(drag.row, row) {
                        state.delegate().push_to_owner(cx);
                    }
                    cx.notify();
                })),
        )
    }

    fn set_selected_index(
        &mut self,
        ix: Option<IndexPath>,
        _window: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) {
        self.selected = ix;
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::reorder;

    /// 重排结果直接决定「图1」「图2」的引用顺序，必须准确。
    #[test]
    fn reorder_moves_item_to_target_position() {
        let mut items = vec!["A", "B", "C"];

        // A 拖到末尾
        assert!(reorder(&mut items, 0, 2));
        assert_eq!(items, ["B", "C", "A"]);

        // A 再拖回首位
        assert!(reorder(&mut items, 2, 0));
        assert_eq!(items, ["A", "B", "C"]);

        // 相邻交换
        assert!(reorder(&mut items, 0, 1));
        assert_eq!(items, ["B", "A", "C"]);
    }

    #[test]
    fn reorder_rejects_noop_and_out_of_range() {
        let mut items = vec!["A", "B", "C"];

        assert!(!reorder(&mut items, 1, 1));
        assert_eq!(items, ["A", "B", "C"]);

        assert!(!reorder(&mut items, 0, 3));
        assert!(!reorder(&mut items, 3, 0));
        assert_eq!(items, ["A", "B", "C"]);

        let mut empty: Vec<&str> = Vec::new();
        assert!(!reorder(&mut empty, 0, 0));
    }
}
