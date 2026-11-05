//! The host key prompt (the Electron app's `HostKeyModal`): a small `A11yDialog` titled
//! Unknown host key, over the connect dialog when a host this computer has not seen asks to
//! be trusted by its fingerprint before the session signs in. Closing it rejects the key.

use std::rc::Rc;

use gpui::{AnyElement, App, Entity, IntoElement, SharedString, div, prelude::*, px};
use polterware_ui::Handler;
use polterware_ui::button::{Button, ButtonKind};
use polterware_ui::patterns::{Dialog, DialogPose, DialogSize, dialog};
use polterware_ui::theme::Theme;

use crate::state::AppState;

/// What the prompt shows: the session it asks for, its address and its key's fingerprint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostKeyPrompt {
    pub session: u64,
    pub address: String,
    pub fingerprint: String,
}

impl HostKeyPrompt {
    /// The first session waiting for its host key to be approved.
    pub fn current(state: &AppState) -> Option<Self> {
        let session = state.host_key_prompt()?;
        Some(Self {
            session: session.id,
            address: format!("{}:{}", session.target.host, session.target.port),
            fingerprint: session.host_key.clone().unwrap_or_default(),
        })
    }
}

/// Rejects the key of the prompt's session.
pub fn reject(state: &Entity<AppState>, id: u64) -> Handler {
    let state = state.clone();
    Rc::new(move |_, cx: &mut App| {
        state.update(cx, |state, cx| {
            state.reject_host_key(id);
            cx.notify();
        });
    })
}

pub fn host_key_dialog(
    state: &Entity<AppState>,
    prompt: &HostKeyPrompt,
    pose: DialogPose,
    cx: &App,
) -> Dialog {
    let theme = *Theme::global(cx);
    let id = prompt.session;
    let (on_reject, accept) = (reject(state, id), state.clone());
    dialog("Unknown host key", 1.0)
        .size(DialogSize::Sm)
        .pose(pose)
        .description(format!(
            "First connection to {}. Verify the fingerprint before trusting this server.",
            prompt.address
        ))
        .on_close(on_reject.clone())
        .footer([
            Button::new("host-key-reject", ButtonKind::Secondary)
                .label("Reject")
                .on_click_rc(on_reject)
                .into_any_element(),
            Button::new("host-key-accept", ButtonKind::Brand)
                .label("Trust and connect")
                .on_click(move |_, cx| {
                    accept.update(cx, |state, cx| {
                        state.approve_host_key(id);
                        cx.notify();
                    });
                })
                .into_any_element(),
        ])
        // `.vitals-fingerprint`: the key in 12px mono on the active fill, broken anywhere.
        .child(fingerprint(&prompt.fingerprint, &theme))
}

fn fingerprint(text: &str, theme: &Theme) -> AnyElement {
    div()
        .p(px(8.0))
        .rounded(px(theme.radius_control))
        .bg(theme.color_surface_active)
        .text_color(theme.color_text_primary)
        .font_family(theme.font_mono)
        .text_size(px(12.0))
        .child(SharedString::from(text.to_string()))
        .into_any_element()
}
