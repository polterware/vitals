//! The root view: the sidebar, the routed page on the main surface, the dialogs, and toasts.

use std::rc::Rc;

use gpui::{
    AnyElement, App, Context, Entity, FocusHandle, IntoElement, KeyBinding, Render, Window,
    actions, div, prelude::*, px,
};
use polterware_ui::patterns::{main_surface, modal_layer};
use polterware_ui::theme::Theme;
use polterware_ui::toast::toast_stack;

use crate::state::{AppState, Route};
use crate::views::connect::ConnectView;
use crate::views::{host_key, host_page, hosts, settings, sidebar};

actions!(vitals_app, [Quit, CloseWindow, ConnectHost]);

pub fn register_key_bindings(cx: &mut App) {
    cx.on_action(|_: &Quit, cx| cx.quit());
    cx.bind_keys([
        KeyBinding::new("cmd-q", Quit, None),
        KeyBinding::new("cmd-w", CloseWindow, None),
        KeyBinding::new("cmd-n", ConnectHost, None),
    ]);
}

pub struct AppView {
    focus_handle: FocusHandle,
    state: Entity<AppState>,
    connect: Entity<ConnectView>,
}

impl AppView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        let state = cx.new(|_| AppState::new());
        cx.observe(&state, |_, _, cx| cx.notify()).detach();
        let connect = cx.new(|cx| ConnectView::new(state.clone(), cx));
        focus_handle.focus(window);
        if let Some(dir) = crate::shots::shots_dir() {
            crate::shots::start_tour(dir, state.clone(), window, cx);
        }
        Self {
            focus_handle,
            state,
            connect,
        }
    }

    fn page(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        match self.state.read(cx).route() {
            Route::Hosts => hosts::hosts_page(&self.state, window, cx),
            Route::Host(id, tab) => host_page::host_page(&self.state, id, tab, window, cx),
            Route::Settings => settings::settings_page(cx),
        }
    }
}

impl Render for AppView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *Theme::global(cx);
        let page = self.page(window, cx);
        let toasts = self.state.read(cx).toasts().to_vec();
        let connect_open = self.state.read(cx).connect_open();
        let host_key = host_key::host_key_card(&self.state, cx);
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
            .flex_row()
            .bg(theme.color_shell_glass)
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
            .child(sidebar::sidebar(&self.state, cx))
            // `.pw-shell-main-frame`: padding 8px 8px 8px 0.
            .child(
                div()
                    .flex_1()
                    .h_full()
                    .min_w(px(0.0))
                    .pt(px(theme.space_2))
                    .pr(px(theme.space_2))
                    .pb(px(theme.space_2))
                    .child(main_surface(page, &theme)),
            );
        if connect_open {
            let close = self.state.clone();
            root = root.child(modal_layer(
                1.0,
                true,
                Rc::new(move |_, cx: &mut App| {
                    close.update(cx, |state, cx| {
                        state.close_connect();
                        cx.notify();
                    });
                }),
                self.connect.clone(),
                &theme,
            ));
        } else if let Some(card) = host_key {
            root = root.child(modal_layer(1.0, true, Rc::new(|_, _| {}), card, &theme));
        }
        root.child(toast_stack(&toasts, dismiss, cx))
    }
}
