//! The connect dialog (the Electron app's `ConnectionDialog`), in the Composer's frame:
//! label, host, port, user, how to sign in, and whether to save the host once it connects.

use std::rc::Rc;

use gpui::{
    AnyElement, App, Context, Entity, Focusable, FontWeight, IntoElement, Render, SharedString,
    Window, div, prelude::*, px,
};
use polterware_ui::button::{Button, ButtonKind};
use polterware_ui::patterns::{composer_actions, composer_card};
use polterware_ui::radio_group::{RadioOption, radio_group};
use polterware_ui::switch::{Switch, switch_row};
use polterware_ui::text_input::TextInput;
use polterware_ui::theme::Theme;

use crate::ssh::Target;
use crate::state::AppState;
use crate::store::{AuthMode, Server};

const MODES: [AuthMode; 3] = [AuthMode::Agent, AuthMode::PrivateKey, AuthMode::Password];

pub struct ConnectView {
    state: Entity<AppState>,
    label: Entity<TextInput>,
    host: Entity<TextInput>,
    port: Entity<TextInput>,
    username: Entity<TextInput>,
    key_path: Entity<TextInput>,
    password: Entity<TextInput>,
    passphrase: Entity<TextInput>,
    save: Entity<Switch>,
    auth: AuthMode,
    editing: Option<Server>,
    error: Option<String>,
    /// The dialog's state was loaded for this opening.
    loaded: bool,
}

fn field(cx: &mut Context<ConnectView>, placeholder: &'static str) -> Entity<TextInput> {
    cx.new(|cx| TextInput::new(cx).placeholder(placeholder))
}

impl ConnectView {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let password = cx.new(|cx| TextInput::new(cx).placeholder("Password").password(true));
        let passphrase = cx.new(|cx| {
            TextInput::new(cx)
                .placeholder("Only if the key has one")
                .password(true)
        });
        Self {
            label: field(cx, "Production"),
            host: field(cx, "example.com"),
            port: field(cx, "22"),
            username: field(cx, "root"),
            key_path: field(cx, "~/.ssh/id_ed25519"),
            password,
            passphrase,
            save: cx.new(|cx| Switch::new(true, cx)),
            auth: AuthMode::Agent,
            editing: None,
            error: None,
            loaded: false,
            state,
        }
    }

    /// Fills the fields from the saved host the dialog opens for, or clears them.
    fn load(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let server = self.state.read(cx).connect_server().cloned();
        let text = |server: &Option<Server>, pick: fn(&Server) -> String| {
            server.as_ref().map(pick).unwrap_or_default()
        };
        let values = [
            (self.label.clone(), text(&server, |s| s.label.clone())),
            (self.host.clone(), text(&server, |s| s.host.clone())),
            (
                self.port.clone(),
                server
                    .as_ref()
                    .map(|s| s.port.to_string())
                    .unwrap_or_else(|| "22".into()),
            ),
            (self.username.clone(), text(&server, |s| s.username.clone())),
            (
                self.key_path.clone(),
                text(&server, |s| s.private_key_path.clone().unwrap_or_default()),
            ),
            (self.password.clone(), String::new()),
            (self.passphrase.clone(), String::new()),
        ];
        for (input, value) in values {
            input.update(cx, |input, cx| input.set_text(value, cx));
        }
        self.auth = server.as_ref().map(|s| s.auth_mode).unwrap_or_default();
        self.save.update(cx, |save, _| save.set_on(true));
        self.error = None;
        // A saved host only needs its secret; a new one starts at the host.
        let focus = if server.is_some() {
            self.secret_input()
        } else {
            &self.host
        };
        focus.read(cx).focus_handle(cx).focus(window);
        self.editing = server;
    }

    /// The password field, or the passphrase field for a private key.
    fn secret_input(&self) -> &Entity<TextInput> {
        if self.auth == AuthMode::Password {
            &self.password
        } else {
            &self.passphrase
        }
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        self.loaded = false;
        self.state.update(cx, |state, cx| {
            state.close_connect();
            cx.notify();
        });
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        let read = |input: &Entity<TextInput>, cx: &Context<Self>| {
            input.read(cx).text().trim().to_string()
        };
        let host = read(&self.host, cx);
        let username = read(&self.username, cx);
        let port_text = read(&self.port, cx);
        let port = if port_text.is_empty() {
            Some(22)
        } else {
            port_text.parse::<u16>().ok().filter(|port| *port > 0)
        };
        let key_path = read(&self.key_path, cx);
        let secret = if self.auth == AuthMode::Agent {
            String::new()
        } else {
            self.secret_input().read(cx).text().to_string()
        };
        let problem = if host.is_empty() {
            Some("Host is required")
        } else if username.is_empty() {
            Some("User is required")
        } else if port.is_none() {
            Some("Port must be a number from 1 to 65535")
        } else if self.auth == AuthMode::PrivateKey && key_path.is_empty() {
            Some("Private key path is required")
        } else if self.auth == AuthMode::Password && secret.is_empty() {
            Some("Password is required")
        } else {
            None
        };
        if let Some(problem) = problem {
            self.error = Some(problem.into());
            cx.notify();
            return;
        }
        let port = port.unwrap_or(22);
        let label = {
            let label = read(&self.label, cx);
            if label.is_empty() {
                host.clone()
            } else {
                label
            }
        };
        let private_key_path = (self.auth == AuthMode::PrivateKey).then_some(key_path);
        let target = Target {
            host: host.clone(),
            port,
            username: username.clone(),
            auth_mode: self.auth,
            private_key_path: private_key_path.clone(),
            secret: (!secret.is_empty()).then_some(secret),
        };
        let save = self.save.read(cx).is_on().then(|| Server {
            id: self
                .editing
                .as_ref()
                .map(|server| server.id.clone())
                .unwrap_or_default(),
            label: label.clone(),
            host,
            port,
            username,
            auth_mode: self.auth,
            private_key_path,
            secret_enc: None,
        });
        let server_id = self.editing.as_ref().map(|server| server.id.clone());
        self.loaded = false;
        self.state.update(cx, |state, cx| {
            if let Some(id) = &server_id
                && let Some(open) = state
                    .sessions()
                    .iter()
                    .find(|session| session.server_id.as_deref() == Some(id.as_str()))
                    .map(|session| session.id)
            {
                state.close_session(open);
            }
            state.close_connect();
            state.connect(label, target, server_id, save, cx);
            cx.notify();
        });
    }
}

fn labeled(label: &'static str, input: AnyElement, theme: &Theme) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap(px(theme.space_1))
        .child(
            div()
                .text_size(px(theme.font_size_sm))
                .font_weight(FontWeight::MEDIUM)
                .text_color(theme.color_text_secondary)
                .child(label),
        )
        .child(input)
}

impl Render for ConnectView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.loaded {
            self.loaded = true;
            self.load(window, cx);
        }
        let theme = *Theme::global(cx);
        let view = cx.entity();
        let title: SharedString = match &self.editing {
            Some(server) => format!("Connect {}", server.display_label()).into(),
            None => "Connect a host".into(),
        };
        let options: Vec<RadioOption> = MODES
            .iter()
            .map(|mode| RadioOption {
                label: mode.label().into(),
            })
            .collect();
        let selected = MODES
            .iter()
            .position(|mode| *mode == self.auth)
            .unwrap_or(0);
        let pick = view.clone();
        let on_select = Rc::new(move |index: usize, _: &mut Window, cx: &mut App| {
            pick.update(cx, |this, cx| {
                this.auth = MODES[index];
                cx.notify();
            });
        });

        let mut card = composer_card(title, 1.0, 0.0, &theme)
            .child(labeled(
                "Label",
                self.label.clone().into_any_element(),
                &theme,
            ))
            .child(
                div()
                    .flex()
                    .gap(px(theme.space_2))
                    .child(div().flex_1().child(labeled(
                        "Host",
                        self.host.clone().into_any_element(),
                        &theme,
                    )))
                    .child(div().w(px(96.0)).child(labeled(
                        "Port",
                        self.port.clone().into_any_element(),
                        &theme,
                    ))),
            )
            .child(labeled(
                "User",
                self.username.clone().into_any_element(),
                &theme,
            ))
            .child(labeled(
                "Sign in with",
                radio_group("connect-auth", &options, selected, on_select, cx).into_any_element(),
                &theme,
            ));
        if self.auth == AuthMode::PrivateKey {
            card = card.child(labeled(
                "Private key",
                self.key_path.clone().into_any_element(),
                &theme,
            ));
        }
        if self.auth != AuthMode::Agent {
            let label = if self.auth == AuthMode::Password {
                "Password"
            } else {
                "Passphrase"
            };
            card = card.child(labeled(
                label,
                self.secret_input().clone().into_any_element(),
                &theme,
            ));
        }
        card = card.child(switch_row(
            "connect-save",
            "Save this host once it connects",
            &self.save,
            window,
            cx,
        ));
        if self.auth != AuthMode::Agent {
            card = card.child(
                div()
                    .text_size(px(theme.font_size_sm))
                    .text_color(theme.color_text_muted)
                    .child("Passwords and passphrases are not saved yet; you type them again."),
            );
        }
        if let Some(error) = &self.error {
            card = card.child(
                div()
                    .text_size(px(theme.font_size_sm))
                    .text_color(theme.color_danger)
                    .child(SharedString::from(error.clone())),
            );
        }
        let cancel = view.clone();
        let connect = view.clone();
        card.child(composer_actions(
            [
                Button::new("connect-cancel", ButtonKind::Secondary)
                    .label("Cancel")
                    .on_click(move |_, cx| cancel.update(cx, |this, cx| this.cancel(cx)))
                    .into_any_element(),
                Button::new("connect-submit", ButtonKind::Primary)
                    .label("Connect")
                    .on_click(move |_, cx| connect.update(cx, |this, cx| this.submit(cx)))
                    .into_any_element(),
            ],
            &theme,
        ))
    }
}
