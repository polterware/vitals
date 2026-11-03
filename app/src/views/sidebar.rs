//! The sidebar: Hosts and Connect a host on top, the open sessions under them (the Electron
//! app's title bar tabs), and Settings in the footer. The whole sidebar drags the window.

use std::rc::Rc;

use gpui::{App, Entity, IntoElement, SharedString, div, prelude::*, px};
use polterware_ui::icon_button::icon_button;
use polterware_ui::icons::Icon;
use polterware_ui::patterns::{SidebarItem, SidebarSectionLabel};
use polterware_ui::status_dot::status_dot;
use polterware_ui::theme::Theme;
use polterware_ui::window::window_drag::WindowDragRegion;

use crate::state::{AppState, HostTab, Route};

pub fn sidebar(state: &Entity<AppState>, cx: &App) -> impl IntoElement {
    let theme = *Theme::global(cx);
    let app = state.read(cx);
    let route = app.route();

    let go = |route: Route| {
        let state = state.clone();
        move |_: &mut gpui::Window, cx: &mut App| {
            state.update(cx, |state, cx| {
                state.go(route);
                cx.notify();
            });
        }
    };
    let connect = state.clone();
    let nav = div()
        .flex_none()
        .flex()
        .flex_col()
        .gap(px(theme.sidebar_row_gap))
        .child(
            SidebarItem::new("sidebar-hosts", "Hosts")
                .icon(Icon::Server)
                .active(route == Route::Hosts)
                .on_click(go(Route::Hosts)),
        )
        .child(
            SidebarItem::new("sidebar-connect", "Connect a host")
                .icon(Icon::Plus)
                .on_click(move |_, cx| {
                    connect.update(cx, |state, cx| {
                        state.open_connect(None);
                        cx.notify();
                    });
                }),
        );

    let mut sessions = div()
        .id("sidebar-sessions")
        .mt(px(theme.space_2))
        .flex_1()
        .min_h(px(0.0))
        .overflow_y_scroll()
        .flex()
        .flex_col()
        .gap(px(theme.sidebar_row_gap));
    if !app.sessions().is_empty() {
        sessions = sessions.child(SidebarSectionLabel::new(
            "sidebar-sessions-label",
            "Sessions",
            false,
        ));
    }
    for session in app.sessions() {
        let id = session.id;
        let active = matches!(route, Route::Host(open, _) if open == id);
        let close = state.clone();
        sessions = sessions.child(
            SidebarItem::new(
                ("sidebar-session", id as usize),
                SharedString::from(session.label.clone()),
            )
            .leading(status_dot(session.status.tone_key()), 14.0)
            .active(active)
            .trailing(icon_button(
                ("sidebar-session-close", id as usize),
                Icon::X,
                "Close session",
                Rc::new(move |_, cx: &mut App| {
                    cx.stop_propagation();
                    close.update(cx, |state, cx| {
                        state.close_session(id);
                        cx.notify();
                    });
                }),
                cx,
            ))
            .on_click(go(Route::Host(id, HostTab::Metrics))),
        );
    }

    let footer = div()
        .flex_none()
        .flex()
        .items_center()
        .pt(px(theme.space_2))
        .child(
            SidebarItem::new("sidebar-settings", "Settings")
                .icon(Icon::Settings)
                .footer()
                .active(route == Route::Settings)
                .on_click(go(Route::Settings)),
        );

    div()
        .id("sidebar")
        .w(px(theme.sidebar_width))
        .h_full()
        .flex_none()
        .overflow_hidden()
        .window_drag_region()
        .pt(px(theme.titlebar_height + theme.space_3))
        .px(px(theme.space_2))
        .pb(px(theme.space_2))
        .flex()
        .flex_col()
        .gap(px(theme.space_2))
        .child(nav)
        .child(sessions)
        .child(footer)
}
