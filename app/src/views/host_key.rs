//! The host key prompt (the Electron app's `HostKeyModal`): a host this computer has not
//! seen asks to be trusted by its fingerprint before the session signs in.

use gpui::{AnyElement, App, Entity, FontWeight, IntoElement, SharedString, div, prelude::*, px};
use polterware_ui::button::{Button, ButtonKind};
use polterware_ui::patterns::{composer_actions, composer_card};
use polterware_ui::theme::Theme;

use crate::state::AppState;

pub fn host_key_card(state: &Entity<AppState>, cx: &App) -> Option<AnyElement> {
    let theme = *Theme::global(cx);
    let session = state.read(cx).host_key_prompt()?;
    let id = session.id;
    let fingerprint = session.host_key.clone().unwrap_or_default();
    let address = format!("{}:{}", session.target.host, session.target.port);
    let reject = state.clone();
    let accept = state.clone();
    Some(
        composer_card("Trust this host?", 1.0, 0.0, &theme)
            .child(
                div()
                    .text_size(px(theme.font_size_sm))
                    .text_color(theme.color_text_secondary)
                    .child(SharedString::from(format!(
                        "{address} is not a known host. Check that its key fingerprint matches the server's before you connect."
                    ))),
            )
            .child(
                div()
                    .p(px(theme.space_3))
                    .rounded(px(theme.radius_md))
                    .bg(theme.color_surface)
                    .font_family(theme.font_mono)
                    .font_weight(FontWeight::MEDIUM)
                    .text_size(px(theme.font_size_sm))
                    .child(SharedString::from(fingerprint)),
            )
            .child(composer_actions(
                [
                    Button::new("host-key-reject", ButtonKind::Secondary)
                        .label("Reject")
                        .on_click(move |_, cx| {
                            reject.update(cx, |state, cx| {
                                state.reject_host_key(id);
                                cx.notify();
                            });
                        })
                        .into_any_element(),
                    Button::new("host-key-accept", ButtonKind::Primary)
                        .label("Trust and connect")
                        .on_click(move |_, cx| {
                            accept.update(cx, |state, cx| {
                                state.approve_host_key(id);
                                cx.notify();
                            });
                        })
                        .into_any_element(),
                ],
                &theme,
            ))
            .into_any_element(),
    )
}
