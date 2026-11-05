//! The connect dialog (the Electron app's `ConnectionDialog`), field for field: a small
//! `A11yDialog` titled Connect (Edit server for a saved host) with Label, Host, Port, User,
//! Authentication, the secret the method needs, Save server for a new host, and Cancel and
//! Connect in its footer.
//!
//! It stays open while the host connects: the fields are locked, Connect reads
//! "Connecting…", the session's progress shows under the fields, and Cancel ends the attempt
//! but keeps the dialog. A failure shows under the fields until the next edit or submit; the
//! dialog closes, and the new tab takes the page, only once the host connects.

use std::rc::Rc;

use gpui::{
    App, Context, Entity, FocusHandle, Focusable, IntoElement, Render, SharedString, Subscription,
    Window, div, prelude::*, px,
};
use polterware_ui::button::{Button, ButtonKind};
use polterware_ui::menu::MenuItem;
use polterware_ui::patterns::{
    AnchorSlot, ContextMenu, DialogPose, DialogSize, dialog, field, form_stack, settings_row,
};
use polterware_ui::select::select_trigger;
use polterware_ui::switch::Switch;
use polterware_ui::text_input::TextInput;
use polterware_ui::theme::Theme;

use crate::secrets::secret_name;
use crate::ssh::Target;
use crate::state::AppState;
use crate::store::{AuthMode, Server};

/// The Authentication select's options, in the web app's order.
const MODES: [AuthMode; 3] = [AuthMode::Password, AuthMode::PrivateKey, AuthMode::Agent];

pub struct ConnectView {
    state: Entity<AppState>,
    menu: Entity<ContextMenu>,
    auth_anchor: AnchorSlot,
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
    /// What is wrong with a field, by the field it sits under.
    error: Option<(Problem, String)>,
    /// The opening (`AppState::connect_opened`) the fields were loaded for.
    loaded: u64,
    /// Holds the keyboard while the fields are locked.
    focus: FocusHandle,
    /// Each text field's text when last seen, so an edit is told from a redraw.
    seen: Vec<String>,
    /// Where the dialog is in its spring, set by the app as it draws the modal.
    pose: DialogPose,
    _edits: Vec<Subscription>,
    /// What the last frame showed of the connect, for the tests.
    #[cfg(test)]
    drawn: Drawn,
}

/// What a frame showed of the connect: the progress line, the failures, whether the fields
/// were locked, and Connect's label.
#[cfg(test)]
#[derive(Clone, Debug, Default, PartialEq)]
struct Drawn {
    progress: Option<String>,
    errors: Vec<String>,
    locked: bool,
    submit: &'static str,
}

/// The field a problem sits under.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Problem {
    Host,
    Port,
    User,
    Password,
    Key,
}

fn input(cx: &mut Context<ConnectView>, placeholder: &'static str) -> Entity<TextInput> {
    cx.new(|cx| TextInput::new(cx).placeholder(placeholder))
}

impl ConnectView {
    pub fn new(state: Entity<AppState>, menu: Entity<ContextMenu>, cx: &mut Context<Self>) -> Self {
        let password = cx.new(|cx| TextInput::new(cx).password(true));
        let passphrase = cx.new(|cx| TextInput::new(cx).password(true));
        let mut view = Self {
            label: input(cx, "prod-1"),
            host: input(cx, "192.168.1.10"),
            port: input(cx, ""),
            username: input(cx, ""),
            key_path: input(cx, "/Users/you/.ssh/id_ed25519"),
            password,
            passphrase,
            save: cx.new(|cx| Switch::new(true, cx)),
            auth: AuthMode::Password,
            editing: None,
            error: None,
            loaded: 0,
            focus: cx.focus_handle(),
            seen: Vec::new(),
            pose: DialogPose::SHOWN,
            _edits: Vec::new(),
            #[cfg(test)]
            drawn: Drawn::default(),
            state,
            menu,
            auth_anchor: AnchorSlot::default(),
        };
        view._edits = view
            .text_fields()
            .into_iter()
            .enumerate()
            .map(|(index, (input, _))| {
                cx.observe(&input, move |this, _, cx| this.edited(index, cx))
            })
            .collect();
        view
    }

    /// Draws the dialog at `pose` of its spring.
    pub fn set_pose(&mut self, pose: DialogPose) {
        self.pose = pose;
    }

    /// The text fields, each with the problem that sits under it.
    fn text_fields(&self) -> [(Entity<TextInput>, Option<Problem>); 7] {
        [
            (self.label.clone(), None),
            (self.host.clone(), Some(Problem::Host)),
            (self.port.clone(), Some(Problem::Port)),
            (self.username.clone(), Some(Problem::User)),
            (self.key_path.clone(), Some(Problem::Key)),
            (self.password.clone(), Some(Problem::Password)),
            (self.passphrase.clone(), None),
        ]
    }

    /// Text field `index` may have changed: an edit clears the connection's failure and the
    /// problem under that field (`edit` in `ConnectionDialog.tsx`).
    fn edited(&mut self, index: usize, cx: &mut Context<Self>) {
        let (input, problem) = self.text_fields()[index].clone();
        let text = input.read(cx).text().to_string();
        if self.seen.get(index) == Some(&text) {
            return;
        }
        self.seen.resize(self.text_fields().len(), String::new());
        self.seen[index] = text;
        if problem.is_some() && self.error.as_ref().map(|(at, _)| Some(*at)) == Some(problem) {
            self.error = None;
            cx.notify();
        }
        if !self.state.read(cx).attempt().errors.is_empty() {
            self.state.update(cx, |state, cx| {
                state.clear_attempt_errors();
                cx.notify();
            });
        }
    }

    /// Remembers every field's text, so loading the fields is not taken for an edit.
    fn see_all(&mut self, cx: &App) {
        self.seen = self
            .text_fields()
            .iter()
            .map(|(input, _)| input.read(cx).text().to_string())
            .collect();
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
            (
                self.username.clone(),
                server
                    .as_ref()
                    .map(|s| s.username.clone())
                    .unwrap_or_else(|| "root".into()),
            ),
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
        self.see_all(cx);
        self.auth = server
            .as_ref()
            .map(|s| s.auth_mode)
            .unwrap_or(AuthMode::Password);
        self.save.update(cx, |save, _| save.set_on(true));
        self.error = None;
        // A saved host that signs in with a secret only needs it; otherwise the first field.
        let focus = if server.is_some() && self.auth != AuthMode::Agent {
            self.secret_input()
        } else {
            &self.label
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

    /// Cancel ends a connect in progress and keeps the dialog; otherwise it closes it.
    pub fn cancel(&mut self, cx: &mut Context<Self>) {
        self.state.update(cx, |state, cx| {
            if state.connecting() {
                state.cancel_attempt();
            } else {
                state.close_connect();
            }
            cx.notify();
        });
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.state.read(cx).connecting() {
            return;
        }
        self.state
            .update(cx, |state, _| state.clear_attempt_errors());
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
            Some((Problem::Host, "Host is required"))
        } else if username.is_empty() {
            Some((Problem::User, "User is required"))
        } else if port.is_none() {
            Some((Problem::Port, "Port must be between 1 and 65535"))
        } else if self.auth == AuthMode::Password && secret.is_empty() {
            Some((Problem::Password, "Password is required"))
        } else if self.auth == AuthMode::PrivateKey && key_path.is_empty() {
            Some((Problem::Key, "Private key path is required"))
        } else {
            None
        };
        if let Some((field, problem)) = problem {
            self.error = Some((field, problem.into()));
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
        // A saved host is always saved again; a new one when Save server is on.
        let keep = self.editing.is_some() || self.save.read(cx).is_on();
        let save = keep.then(|| Server {
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
        // The fields lock: the keyboard leaves them for the dialog.
        self.focus.focus(window);
        self.state.update(cx, |state, cx| {
            state.begin_attempt(label, target, server_id, save, cx);
            cx.notify();
        });
    }
}

impl ConnectView {
    /// The problem under `field`, if the last submit found one there.
    fn problem(&self, field: Problem) -> Option<SharedString> {
        self.error
            .as_ref()
            .filter(|(at, _)| *at == field)
            .map(|(_, text)| SharedString::from(text.clone()))
    }

    /// The Authentication `Select`: the picked method on the field, the methods in a menu
    /// under it.
    fn auth_select(&self, connecting: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.entity();
        let (menu, probe) = (self.menu.clone(), self.auth_anchor.clone());
        let picked = self.auth;
        div()
            .relative()
            .w_full()
            .child(self.auth_anchor.probe())
            .child(select_trigger(
                "connect-auth",
                picked.label(),
                None,
                Rc::new(move |window, cx: &mut App| {
                    let Some(bounds) = probe.get().filter(|_| !connecting) else {
                        return;
                    };
                    let items = MODES
                        .iter()
                        .map(|mode| {
                            let (view, mode) = (view.clone(), *mode);
                            MenuItem::radio(
                                mode.label(),
                                mode == picked,
                                Rc::new(move |_, cx: &mut App| {
                                    view.update(cx, |this, cx| {
                                        this.auth = mode;
                                        this.error = None;
                                        this.state.update(cx, |state, cx| {
                                            state.clear_attempt_errors();
                                            cx.notify();
                                        });
                                        cx.notify();
                                    });
                                }),
                            )
                        })
                        .collect();
                    let width = f32::from(bounds.size.width);
                    menu.update(cx, |menu, cx| {
                        menu.set_width(width);
                        menu.open_under(bounds, items, window, cx);
                    });
                }),
                cx,
            ))
    }
}

impl Render for ConnectView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let opened = self.state.read(cx).connect_opened();
        if self.loaded != opened {
            self.loaded = opened;
            self.load(window, cx);
        }
        let theme = *Theme::global(cx);
        let view = cx.entity();
        let (connecting, progress, failures) = {
            let state = self.state.read(cx);
            let attempt = state.attempt();
            (
                state.connecting(),
                attempt.progress.clone(),
                attempt.errors.clone(),
            )
        };
        let title = if self.editing.is_some() {
            "Edit server"
        } else {
            "Connect"
        };
        let mut fields = vec![
            field("Label", self.label.clone()).into_any_element(),
            field("Host", self.host.clone())
                .error(self.problem(Problem::Host))
                .into_any_element(),
            field("Port", self.port.clone())
                .error(self.problem(Problem::Port))
                .into_any_element(),
            field("User", self.username.clone())
                .error(self.problem(Problem::User))
                .into_any_element(),
            field("Authentication", self.auth_select(connecting, cx)).into_any_element(),
        ];
        match self.auth {
            AuthMode::Password => fields.push(
                field("Password", self.password.clone())
                    .error(self.problem(Problem::Password))
                    .into_any_element(),
            ),
            AuthMode::PrivateKey => {
                fields.push(
                    field("Private key path", self.key_path.clone())
                        .error(self.problem(Problem::Key))
                        .into_any_element(),
                );
                fields.push(
                    field("Passphrase (optional)", self.passphrase.clone()).into_any_element(),
                );
            }
            AuthMode::Agent => {}
        }
        if self.auth != AuthMode::Agent {
            // Where a saved server's secret goes, said where it is typed.
            fields.push(
                div()
                    .text_size(px(11.0))
                    .line_height(px(11.0 * 1.35))
                    .text_color(theme.color_text_muted)
                    .child(format!(
                        "A saved server keeps its {} in your macOS Keychain.",
                        secret_name(self.auth)
                    ))
                    .into_any_element(),
            );
        }
        // `.pw-field__hint` while it connects, then `.pw-field__error`s if it failed.
        #[cfg(test)]
        {
            self.drawn = Drawn {
                progress: progress.clone().filter(|_| connecting),
                errors: failures.clone(),
                locked: connecting,
                submit: submit_label(connecting),
            };
        }
        let note = |text: String, color| {
            div()
                .text_size(px(11.0))
                .line_height(px(11.0 * 1.35))
                .text_color(color)
                .child(text)
                .into_any_element()
        };
        if connecting && let Some(progress) = progress {
            fields.push(note(progress, theme.color_text_muted));
        }
        if !failures.is_empty() {
            fields.push(
                div()
                    .flex()
                    .flex_col()
                    .children(
                        failures
                            .into_iter()
                            .map(|failure| note(failure, theme.color_danger)),
                    )
                    .into_any_element(),
            );
        }
        if self.editing.is_none() {
            fields.push(
                settings_row("Save server")
                    .child(self.save.clone())
                    .into_any_element(),
            );
        }
        let (close, cancel, connect) = (view.clone(), view.clone(), view);
        let form = div()
            .relative()
            .track_focus(&self.focus)
            .child(form_stack(fields))
            // Locked while it connects: the fields take no pointer.
            .when(connecting, |form| {
                form.child(
                    div()
                        .id("connect-locked")
                        .absolute()
                        .inset_0()
                        .occlude()
                        .cursor_not_allowed(),
                )
            });
        dialog(title, 1.0)
            .size(DialogSize::Sm)
            .pose(self.pose)
            .on_close(Rc::new(move |_, cx: &mut App| {
                close.update(cx, |this, cx| this.cancel(cx));
            }))
            .footer([
                Button::new("connect-cancel", ButtonKind::Danger)
                    .label("Cancel")
                    .on_click(move |_, cx| cancel.update(cx, |this, cx| this.cancel(cx)))
                    .into_any_element(),
                Button::new("connect-submit", ButtonKind::Inverse)
                    .label(submit_label(connecting))
                    .disabled(connecting)
                    .on_click(move |window, cx| {
                        connect.update(cx, |this, cx| this.submit(window, cx))
                    })
                    .into_any_element(),
            ])
            .child(form)
    }
}

/// Connect's label: "Connecting…" while it waits on the host.
fn submit_label(connecting: bool) -> &'static str {
    if connecting {
        "Connecting…"
    } else {
        "Connect"
    }
}

#[cfg(test)]
mod tests {
    use gpui::{
        AppContext as _, Bounds, TestAppContext, VisualTestContext, WindowBounds, WindowOptions,
        point, px, size,
    };

    use super::*;
    use crate::session::{SessionEvent, Status};

    struct Open {
        cx: VisualTestContext,
        state: Entity<AppState>,
        view: Entity<ConnectView>,
    }

    fn open(cx: &mut TestAppContext) -> Open {
        let state = cx.new(|_| AppState::new());
        let root = state.clone();
        let window = cx.update(|cx| {
            polterware_ui::init(cx);
            let bounds = Bounds::new(point(px(0.0), px(0.0)), size(px(900.0), px(800.0)));
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                move |_, cx| {
                    let menu = cx.new(ContextMenu::new);
                    cx.new(|cx| ConnectView::new(root, menu, cx))
                },
            )
            .expect("the test window opens")
        });
        let view = window
            .update(cx, |_, _, cx| cx.entity())
            .expect("the window has its view");
        let mut cx = VisualTestContext::from_window(window.into(), cx);
        cx.update(|window, _| window.activate_window());
        state.update(&mut cx, |state, cx| {
            state.open_connect(None);
            cx.notify();
        });
        let mut open = Open { cx, state, view };
        open.draw();
        open
    }

    impl Open {
        fn draw(&mut self) {
            self.cx.update(|window, cx| {
                window.refresh();
                window.draw(cx).clear();
            });
            self.cx.run_until_parked();
        }

        fn drawn(&self) -> Drawn {
            self.view.read_with(&self.cx, |view, _| view.drawn.clone())
        }

        fn feed(&mut self, id: u64, status: Status, message: Option<&str>) {
            let event = SessionEvent::Status(status, message.map(str::to_string));
            self.state.update(&mut self.cx, |state, cx| {
                state.feed(id, event, cx);
                cx.notify();
            });
            self.draw();
        }

        fn begin(&mut self) -> u64 {
            let target = Target {
                host: "10.0.0.5".into(),
                port: 22,
                username: "root".into(),
                auth_mode: AuthMode::Agent,
                private_key_path: None,
                secret: None,
            };
            let id = self.state.update(&mut self.cx, |state, cx| {
                cx.notify();
                state.begin_test_attempt("box", target)
            });
            self.draw();
            id
        }
    }

    #[gpui::test]
    fn the_dialog_shows_its_connect_and_stays_open(cx: &mut TestAppContext) {
        let mut open = open(cx);
        let idle = Drawn {
            submit: "Connect",
            ..Drawn::default()
        };
        assert_eq!(open.drawn(), idle);
        let id = open.begin();
        assert_eq!(
            open.drawn(),
            Drawn {
                locked: true,
                submit: "Connecting…",
                ..Drawn::default()
            },
            "the fields lock before the first step"
        );
        open.feed(id, Status::Connecting, Some("Authenticating as root…"));
        assert_eq!(
            open.drawn(),
            Drawn {
                progress: Some("Authenticating as root…".into()),
                errors: Vec::new(),
                locked: true,
                submit: "Connecting…",
            }
        );
        // Cancel ends the connect and keeps the dialog, unlocked.
        let view = open.view.clone();
        view.update(&mut open.cx, |view, cx| view.cancel(cx));
        open.draw();
        assert!(
            open.state
                .read_with(&open.cx, |state, _| state.connect_open())
        );
        assert_eq!(open.drawn(), idle);
        // Cancel again closes it.
        view.update(&mut open.cx, |view, cx| view.cancel(cx));
        assert!(
            !open
                .state
                .read_with(&open.cx, |state, _| state.connect_open())
        );
    }

    #[gpui::test]
    fn a_failure_shows_until_the_next_edit(cx: &mut TestAppContext) {
        let mut open = open(cx);
        let id = open.begin();
        open.feed(id, Status::Error, Some("Permission denied (publickey)"));
        assert_eq!(
            open.drawn(),
            Drawn {
                progress: None,
                errors: vec!["Permission denied (publickey)".into()],
                locked: false,
                submit: "Connect",
            }
        );
        let host = open.view.read_with(&open.cx, |view, _| view.host.clone());
        host.update(&mut open.cx, |input, cx| input.set_text("10.0.0.6", cx));
        open.draw();
        assert!(open.drawn().errors.is_empty(), "an edit clears the failure");
        assert!(
            open.state
                .read_with(&open.cx, |state, _| state.attempt().errors.is_empty())
        );
    }

    #[gpui::test]
    fn a_missing_field_is_named_before_any_connect(cx: &mut TestAppContext) {
        let mut open = open(cx);
        let view = open.view.clone();
        // The new-host dialog opens with no host: Connect names it and starts nothing.
        open.cx
            .update(|window, cx| view.update(cx, |view, cx| view.submit(window, cx)));
        open.draw();
        let problem = view.read_with(&open.cx, |view, _| view.problem(Problem::Host));
        assert_eq!(problem, Some("Host is required".into()));
        assert!(
            !open
                .state
                .read_with(&open.cx, |state, _| state.connecting())
        );
        assert_eq!(
            open.state
                .read_with(&open.cx, |state, _| state.sessions().len()),
            0
        );
        // Typing a host clears its problem.
        let host = view.read_with(&open.cx, |view, _| view.host.clone());
        host.update(&mut open.cx, |input, cx| input.set_text("10.0.0.5", cx));
        open.draw();
        assert!(view.read_with(&open.cx, |view, _| view.problem(Problem::Host).is_none()));
    }

    #[gpui::test]
    fn a_connect_closes_the_dialog(cx: &mut TestAppContext) {
        let mut open = open(cx);
        let id = open.begin();
        open.feed(id, Status::Connected, None);
        assert!(
            !open
                .state
                .read_with(&open.cx, |state, _| state.connect_open())
        );
        assert_eq!(
            open.state
                .read_with(&open.cx, |state, _| state.tabs().count()),
            1
        );
    }
}
