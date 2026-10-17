//! 展示层公共件：主题取色、卡片、表单行、分段选择器。
//!
//! 这里只放与业务无关的东西，任何视图都可以直接复用；业务逻辑一律不放在这里。

use gpui_kit::component::{
    ActiveTheme as _, button::{Button, ButtonVariants as _}, h_flex, input::InputState,
    label::Label, v_flex,
};
use gpui_kit::prelude::*;
use gpui_kit::{
    AnyElement, App, ClickEvent, Context, ElementId, Entity, Hsla, Pixels, SharedString, Window,
    div,
};

/// 参考图缩略图尺寸（行高由它决定，见 `ref_images` 的等高要求）。
pub const THUMB_WIDTH: f32 = 72.;
pub const THUMB_HEIGHT: f32 = 54.;

/// 参考图列表的最大高度，超出后由列表内部滚动。
pub const IMAGE_LIST_MAX_HEIGHT: f32 = 300.;

/// 主题中常用取值的快照。
///
/// `Hsla` 与 `Pixels` 都是 `Copy`，所以可以在各个自由函数之间随手传递，
/// 不必把 `cx` 一路透传下去。
#[derive(Clone, Copy)]
pub struct Palette {
    pub bg: Hsla,
    pub fg: Hsla,
    pub primary: Hsla,
    pub muted: Hsla,
    pub muted_fg: Hsla,
    pub border: Hsla,
    pub danger: Hsla,
    pub radius: Pixels,
    pub radius_lg: Pixels,
}

impl Palette {
    pub fn new(cx: &App) -> Self {
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

/// 次要说明文字。
pub fn hint(p: &Palette, text: impl Into<SharedString>) -> AnyElement {
    div()
        .text_sm()
        .text_color(p.muted_fg)
        .child(text.into())
        .into_any_element()
}

/// 卡片：标题 + 可选副标题 + 内容。
pub fn section(
    p: &Palette,
    title: &'static str,
    subtitle: Option<&'static str>,
    body: AnyElement,
) -> AnyElement {
    let mut head = v_flex().gap_1().child(Label::new(title));
    if let Some(subtitle) = subtitle {
        head = head.child(hint(p, subtitle));
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

/// 表单一行：上行标签，下行控件。
pub fn field_row(p: &Palette, label: &str, control: AnyElement) -> AnyElement {
    v_flex()
        .gap_2()
        .w_full()
        .child(hint(p, label.to_string()))
        .child(control)
        .into_any_element()
}

/// 分段选择器：一排按钮，选中项使用 primary 样式。
///
/// `on_pick` 只负责改状态，重绘由本函数统一触发。
pub fn segmented<T, F>(
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
    let chips: Vec<AnyElement> = options
        .iter()
        .map(|(label, value)| {
            let value: &'static str = value;
            let on_pick = on_pick.clone();

            let button = Button::new(ElementId::Name(format!("{prefix}:{value}").into()))
                .label(SharedString::from(*label));
            let button = if value == selected {
                button.primary()
            } else {
                button.outline()
            };

            button
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    on_pick(this, value, cx);
                    cx.notify();
                }))
                .into_any_element()
        })
        .collect();

    h_flex().flex_wrap().gap_2().children(chips).into_any_element()
}

/// 单行输入框，构造时一并给定占位符与初始值。
pub fn text_input(
    window: &mut Window,
    cx: &mut App,
    placeholder: &str,
    value: impl Into<SharedString>,
) -> Entity<InputState> {
    let placeholder = SharedString::from(placeholder);
    let value = value.into();

    cx.new(|cx| {
        InputState::new(window, cx)
            .placeholder(placeholder)
            .default_value(value)
    })
}

/// 把滚轮滚动限制在内部：内层滚动到顶/底之后，滚轮滚动不再传给外层容器。
///
/// 等价于 CSS 的 `overscroll-behavior: contain`。
///
/// gpui 派发滚轮事件时按「内层 → 外层」倒序遍历监听器，但内层滚动完毕后并不拦截事件，
/// 于是内外层会一起滚动。这里套一个只挂滚轮监听的包装元素：它排在内外层之间，
/// 内层先滚动完，冒泡到这里就把事件截断，外层接收不到。
pub fn scroll_contained(child: impl IntoElement) -> AnyElement {
    div()
        .w_full()
        .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
        .child(child)
        .into_any_element()
}

/// 限定取值范围的数字输入框。
pub fn number_input(
    window: &mut Window,
    cx: &mut App,
    value: impl Into<SharedString>,
    range: std::ops::RangeInclusive<f64>,
) -> Entity<InputState> {
    let value = value.into();
    let (min, max) = (*range.start(), *range.end());

    cx.new(|cx| {
        InputState::new(window, cx)
            .min(min)
            .max(max)
            .default_value(value)
    })
}
