//! The root view, laid out as the Electron app's shell: the title bar is the strip of tabs
//! (the open sessions, "+" for the start page, and Settings at its end) over the main surface,
//! which shows the start page or the open session; then the dialogs (connect, a host key over
//! it, a confirmation, Settings), each opening on the slow spring and leaving on the quicker
//! tween (`DialogPresence`), the menus, and toasts, each toast dismissed once its lifetime
//! passes.

use std::collections::HashSet;
use std::rc::Rc;

use gpui::{
    AnyElement, App, Context, Entity, FocusHandle, IntoElement, KeyBinding, Render, Rgba, Window,
    actions, deferred, div, prelude::*, px,
};
use polterware_ui::icons::Icon;
use polterware_ui::patterns::{
    ConfirmDialog, ContextMenu, DialogPose, DialogPresence, TitleTab, main_surface, modal_layer,
    title_add, title_card, title_divider, title_spacer, title_strip,
};
use polterware_ui::theme::Theme;
use polterware_ui::toast::{toast_lifetime, toast_stack};

use crate::settings::signal_name;
use crate::state::{AppState, Confirm, Route, SettingsSection};
use crate::views::connect::ConnectView;
use crate::views::host_key::{self, HostKeyPrompt};
use crate::views::settings::SettingsView;
use crate::views::{host_page, hosts};

actions!(vitals_app, [Quit, CloseWindow, ConnectHost, OpenSettings]);

/// The frame's gutter around the main surface (`.vitals-main-frame`), on every side but the
/// title bar's.
const FRAME_PAD: f32 = 8.0;

pub fn register_key_bindings(cx: &mut App) {
    cx.on_action(|_: &Quit, cx| cx.quit());
    cx.bind_keys([
        KeyBinding::new("cmd-q", Quit, None),
        KeyBinding::new("cmd-w", CloseWindow, None),
        KeyBinding::new("cmd-n", ConnectHost, None),
        KeyBinding::new("cmd-,", OpenSettings, None),
    ]);
}

pub struct AppView {
    focus_handle: FocusHandle,
    confirm_focus: FocusHandle,
    state: Entity<AppState>,
    connect: Entity<ConnectView>,
    settings: Entity<SettingsView>,
    menu: Entity<ContextMenu>,
    /// Toasts with a dismissal on its way.
    timed: HashSet<u64>,
    confirming: Option<Confirm>,
    settings_shown: bool,
    /// Each dialog's spring.
    modals: Modals,
    /// The last confirmation and host key asked, drawn while their dialogs leave.
    last_confirm: Option<Confirm>,
    last_host_key: Option<HostKeyPrompt>,
}

/// Where each dialog is in its spring.
#[derive(Default)]
struct Modals {
    connect: DialogPresence,
    host_key: DialogPresence,
    confirm: DialogPresence,
    settings: DialogPresence,
    /// Captures draw each dialog at rest.
    still: bool,
}

impl Modals {
    fn set(presence: &mut DialogPresence, open: bool, still: bool) {
        if still {
            *presence = if open {
                DialogPresence::shown()
            } else {
                DialogPresence::new()
            };
        } else {
            presence.set_open(open);
        }
    }
}

impl AppView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        let confirm_focus = cx.focus_handle();
        let state = cx.new(|_| AppState::new());
        cx.observe_in(&state, window, |this, _, window, cx| {
            this.state_changed(window, cx);
            cx.notify();
        })
        .detach();
        let menu = cx.new(ContextMenu::new);
        let connect = cx.new(|cx| ConnectView::new(state.clone(), menu.clone(), cx));
        let settings = cx.new(|cx| SettingsView::new(state.clone(), menu.clone(), cx));
        focus_handle.focus(window);
        if let Some(dir) = crate::shots::shots_dir() {
            crate::shots::start_tour(dir, state.clone(), window, cx);
        } else if let Some(seconds) = crate::shots::hold_seconds() {
            crate::shots::start_hold(seconds, state.clone(), window, cx);
        }
        let still = crate::shots::shots_dir().is_some();
        Self {
            focus_handle,
            confirm_focus,
            state,
            connect,
            settings,
            menu,
            timed: HashSet::new(),
            confirming: None,
            settings_shown: false,
            modals: Modals {
                still,
                ..Modals::default()
            },
            last_confirm: None,
            last_host_key: None,
        }
    }

    /// Dismisses each new toast after its lifetime, focuses a confirmation as it opens (so
    /// Enter and Escape answer it), brings Settings' controls up to date as it opens, and
    /// opens or closes each dialog's spring.
    fn state_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let state = self.state.read(cx);
        let still = self.modals.still;
        Modals::set(&mut self.modals.connect, state.connect_open(), still);
        Modals::set(&mut self.modals.confirm, state.confirm().is_some(), still);
        Modals::set(
            &mut self.modals.settings,
            state.settings_open().is_some(),
            still,
        );
        // A new prompt springs in again, even over one leaving.
        let prompt = HostKeyPrompt::current(state);
        if prompt.is_some() && prompt != self.last_host_key {
            self.modals.host_key = DialogPresence::new();
        }
        Modals::set(&mut self.modals.host_key, prompt.is_some(), still);
        if prompt.is_some() {
            self.last_host_key = prompt;
        }
        if let Some(confirm) = state.confirm() {
            self.last_confirm = Some(confirm);
        }
        let toasts: Vec<_> = state
            .toasts()
            .iter()
            .filter(|toast| !self.timed.contains(&toast.id))
            .map(|toast| (toast.id, toast_lifetime(toast.tone)))
            .collect();
        let confirm = state.confirm();
        let settings_open = state.settings_open().is_some();
        let live: HashSet<u64> = state.toasts().iter().map(|toast| toast.id).collect();
        self.timed.retain(|id| live.contains(id));
        for (id, lifetime) in toasts {
            self.timed.insert(id);
            let state = self.state.clone();
            cx.spawn(async move |_, cx| {
                cx.background_executor().timer(lifetime).await;
                let _ = state.update(cx, |state, cx| {
                    state.dismiss_toast(id);
                    cx.notify();
                });
            })
            .detach();
        }
        if confirm != self.confirming {
            self.confirming = confirm;
            if confirm.is_some() {
                self.menu.update(cx, |menu, cx| menu.close(cx));
                self.confirm_focus.focus(window);
            } else {
                self.focus_handle.focus(window);
            }
        }
        if settings_open != self.settings_shown {
            self.settings_shown = settings_open;
            if settings_open {
                self.settings.update(cx, |settings, cx| settings.sync(cx));
            }
        }
    }

    fn page(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        match self.state.read(cx).route() {
            Route::Hosts => hosts::hosts_page(&self.state, &self.menu, window, cx),
            Route::Host(id, tab) => {
                host_page::host_page(&self.state, &self.menu, id, tab, window, cx)
            }
        }
    }

    /// The title bar: the divider past the traffic lights, a tab per session, "+", and
    /// Settings at the end.
    fn title_bar(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let app = self.state.read(cx);
        let route = app.route();
        let settings_open = app.settings_open().is_some();
        // A connect the dialog waits on has no tab until it connects.
        let tabs: Vec<(u64, String)> = app
            .tabs()
            .map(|session| (session.id, session.label.clone()))
            .collect();
        let mut children = vec![title_divider(cx)];
        for (id, label) in tabs {
            let (select, close) = (self.state.clone(), self.state.clone());
            children.push(
                TitleTab::new(("title-tab", id as usize), label)
                    .active(matches!(route, Route::Host(open, _) if open == id))
                    .on_select(Rc::new(move |_, cx: &mut App| {
                        select.update(cx, |state, cx| {
                            let tab = match state.route() {
                                Route::Host(open, tab) if open == id => tab,
                                _ => crate::state::HostTab::Metrics,
                            };
                            state.go(Route::Host(id, tab));
                            cx.notify();
                        });
                    }))
                    .on_close(Rc::new(move |_, cx: &mut App| {
                        close.update(cx, |state, cx| {
                            state.request_close(id);
                            cx.notify();
                        });
                    }))
                    .into_any_element(),
            );
        }
        let home = self.state.clone();
        children.push(title_add(
            "title-new",
            route == Route::Hosts,
            Rc::new(move |_, cx: &mut App| {
                home.update(cx, |state, cx| {
                    state.go(Route::Hosts);
                    cx.notify();
                });
            }),
            window,
            cx,
        ));
        children.push(title_spacer());
        let settings = self.state.clone();
        children.push(title_card(
            "title-settings",
            Icon::Settings,
            settings_open,
            Rc::new(move |_, cx: &mut App| {
                settings.update(cx, |state, cx| {
                    state.open_settings(SettingsSection::General);
                    cx.notify();
                });
            }),
            window,
            cx,
        ));
        title_strip("title-bar", FRAME_PAD, children, cx).into_any_element()
    }

    /// The dialogs still drawn, bottom first: the connect dialog with the host key prompt over
    /// it, then a confirmation, then Settings. Each fades its scrim with its own spring, and
    /// takes the pointer only while it is open.
    fn modals(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let theme = *Theme::global(cx);
        let mut layers = Vec::new();
        let presence = &self.modals.connect;
        if presence.is_present() {
            let pose = presence.frame(window);
            let open = presence.is_open();
            self.connect.update(cx, |view, _| view.set_pose(pose));
            let cancel = self.connect.clone();
            layers.push(modal_layer(
                pose.opacity,
                open,
                Rc::new(move |_, cx: &mut App| cancel.update(cx, |view, cx| view.cancel(cx))),
                self.connect.clone(),
                &theme,
            ));
        }
        let presence = &self.modals.host_key;
        if presence.is_present()
            && let Some(prompt) = &self.last_host_key
        {
            let pose = presence.frame(window);
            let open = presence.is_open();
            layers.push(modal_layer(
                pose.opacity,
                open,
                host_key::reject(&self.state, prompt.session),
                host_key::host_key_dialog(&self.state, prompt, pose, cx),
                &theme,
            ));
        }
        let presence = &self.modals.confirm;
        if presence.is_present()
            && let Some(confirm) = self.last_confirm
        {
            let pose = presence.frame(window);
            let open = presence.is_open();
            let cancel = self.state.clone();
            layers.push(modal_layer(
                pose.opacity,
                open,
                Rc::new(move |_, cx: &mut App| {
                    cancel.update(cx, |state, cx| {
                        state.cancel_confirm();
                        cx.notify();
                    });
                }),
                self.confirm_card(confirm, pose, cx),
                &theme,
            ));
        }
        let presence = &self.modals.settings;
        if presence.is_present() {
            let pose = presence.frame(window);
            let open = presence.is_open();
            self.settings.update(cx, |view, _| view.set_pose(pose));
            let close = self.state.clone();
            layers.push(modal_layer(
                pose.opacity,
                open,
                Rc::new(move |_, cx: &mut App| {
                    close.update(cx, |state, cx| {
                        state.close_settings();
                        cx.notify();
                    });
                }),
                self.settings.clone(),
                &theme,
            ));
        }
        layers
    }

    fn confirm_card(&self, confirm: Confirm, pose: DialogPose, cx: &App) -> AnyElement {
        let state = self.state.read(cx);
        let cancel = self.state.clone();
        let accept = self.state.clone();
        let dialog = match confirm {
            Confirm::Kill { pid, .. } => ConfirmDialog::new(
                "confirm-kill",
                format!("Kill process {pid}?"),
                format!(
                    "This sends SIG{} on the remote host.",
                    signal_name(state.settings().default_kill_signal)
                ),
            )
            .recovery("The process stops only if the remote user is allowed to signal it.")
            .confirm_label("Kill")
            .danger(),
            Confirm::Close { session } => {
                let label = state
                    .session(session)
                    .map(|open| open.label.clone())
                    .unwrap_or_else(|| "this session".into());
                ConfirmDialog::new(
                    "confirm-close",
                    format!("Close {label}?"),
                    "This ends the SSH session and stops the metrics agent on the remote host.",
                )
                .recovery("Metric history and any open terminal for this tab are discarded.")
                .confirm_label("Close session")
                .danger()
            }
        };
        dialog
            .pose(pose)
            .focus(&self.confirm_focus)
            .on_cancel(Rc::new(move |_, cx: &mut App| {
                cancel.update(cx, |state, cx| {
                    state.cancel_confirm();
                    cx.notify();
                });
            }))
            .on_confirm(Rc::new(move |_, cx: &mut App| {
                accept.update(cx, |state, cx| {
                    state.accept_confirm();
                    cx.notify();
                });
            }))
            .into_any_element()
    }
}

impl Render for AppView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *Theme::global(cx);
        let page = self.page(window, cx);
        let title_bar = self.title_bar(window, cx);
        let app = self.state.read(cx);
        let toasts = app.toasts().to_vec();
        // Reduce transparency: the shell's glass, opaque.
        let shell = if app.settings().reduce_transparency {
            Rgba {
                a: 1.0,
                ..theme.color_shell_glass
            }
        } else {
            theme.color_shell_glass
        };
        let dismiss_state = self.state.clone();
        let dismiss = Rc::new(move |id, _: &mut Window, cx: &mut App| {
            dismiss_state.update(cx, |state, cx| {
                state.dismiss_toast(id);
                cx.notify();
            });
        });

        let mut root = div()
            .id("vitals-root")
            .track_focus(&self.focus_handle)
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(shell)
            .text_color(theme.color_text_primary)
            .font_family(theme.font_sans)
            .text_size(px(theme.font_size_md))
            .on_action(cx.listener(|_, _: &CloseWindow, window, _| window.remove_window()))
            .on_action(cx.listener(|this, _: &ConnectHost, _, cx| {
                this.state.update(cx, |state, cx| {
                    state.open_connect(None);
                    cx.notify();
                });
            }))
            .on_action(cx.listener(|this, _: &OpenSettings, _, cx| {
                this.state.update(cx, |state, cx| {
                    state.open_settings(SettingsSection::General);
                    cx.notify();
                });
            }))
            // `.vitals-main-frame`: the title bar, then the surface 8px in from the window.
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.0))
                    .w_full()
                    .flex()
                    .flex_col()
                    .px(px(FRAME_PAD))
                    .pb(px(FRAME_PAD))
                    .child(title_bar)
                    .child(
                        div()
                            .flex_1()
                            .min_h(px(0.0))
                            .child(main_surface(page, &theme)),
                    ),
            );
        for modal in self.modals(window, cx) {
            root = root.child(modal);
        }
        root.child(toast_stack(&toasts, dismiss, cx))
            // Last, so it draws over the page and its cards.
            .child(deferred(self.menu.clone()).with_priority(1))
    }
}
