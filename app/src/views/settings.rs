//! Settings (the Electron app's `SettingsDialog`), not ported yet: the page names what it
//! will hold.

use gpui::{AnyElement, App, IntoElement, div, prelude::*, px};
use polterware_ui::patterns::{MascotMood, page_empty_with, toolbar};
use polterware_ui::theme::Theme;

const PENDING: [&str; 6] = [
    "Network unit (bytes or bits) and the history window",
    "Confirm before stopping a process or closing a session",
    "The default kill signal",
    "Reconnect automatically, and after how long",
    "Reduce transparency",
    "Known hosts, recent hosts, and the card layout",
];

pub fn settings_page(cx: &App) -> AnyElement {
    let theme = *Theme::global(cx);
    let list = div()
        .flex()
        .flex_col()
        .items_center()
        .gap(px(theme.space_1))
        .text_size(px(theme.font_size_sm))
        .text_color(theme.color_text_muted)
        .children(PENDING.iter().map(|item| div().child(*item)));
    div()
        .size_full()
        .flex()
        .flex_col()
        .child(toolbar(div(), div(), cx))
        .child(page_empty_with(
            Some("Settings are not ported yet"),
            MascotMood::Sleeping,
            Some(list.into_any_element()),
            cx,
        ))
        .into_any_element()
}
