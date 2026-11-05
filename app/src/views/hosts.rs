//! The start page (the Electron app's `HostHome`): Connect a host, then the hosts connected
//! last (Recents) and the other saved hosts (Saved). A row opens its host, connecting it
//! first when it is not open; its "…" edits or removes it.

use std::rc::Rc;

use gpui::{AnyElement, App, Entity, IntoElement, SharedString, Window};
use polterware_ui::button::{Button, ButtonKind};
use polterware_ui::icons::Icon;
use polterware_ui::menu::MenuItem;
use polterware_ui::patterns::{ContextMenu, MascotMood, StartRow, start_page, start_section};

use crate::state::{AppState, RECENT_LIMIT};
use crate::store::Server;

pub fn hosts_page(
    state: &Entity<AppState>,
    menu: &Entity<ContextMenu>,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let app = state.read(cx);
    let servers = app.servers().to_vec();
    let recents: Vec<Server> = app
        .recents()
        .iter()
        .filter_map(|key| servers.iter().find(|server| &server.key() == key))
        .take(RECENT_LIMIT)
        .cloned()
        .collect();
    let others: Vec<Server> = servers
        .iter()
        .filter(|server| !recents.iter().any(|recent| recent.id == server.id))
        .cloned()
        .collect();

    let target = window.use_keyed_state("home-menu-target", cx, |_, _| None::<String>);
    if target.read(cx).is_some() && !menu.read(cx).is_open(cx) {
        target.update(cx, |target, _| *target = None);
    }
    let open_for = target.read(cx).clone();

    let row = |server: &Server, section: &'static str| -> AnyElement {
        let (open, more_state, more_menu, more_target) =
            (state.clone(), state.clone(), menu.clone(), target.clone());
        let (opened, edited) = (server.clone(), server.clone());
        let id = server.id.clone();
        StartRow::new(
            SharedString::from(format!("home-{section}-{}", server.id)),
            Icon::Server,
            server.display_label().to_string(),
        )
        .detail(format!("{}@{}", server.username, server.host))
        .menu_open(open_for.as_deref() == Some(server.id.as_str()))
        .on_open(Rc::new(move |_, cx: &mut App| {
            let server = opened.clone();
            open.update(cx, |state, cx| {
                // Its session, or a connection with its saved secret; with none, the dialog.
                state.open_saved(&server, cx);
                cx.notify();
            });
        }))
        .on_more(Rc::new(move |bounds, window, cx: &mut App| {
            more_target.update(cx, |target, _| *target = Some(id.clone()));
            let (edit_state, remove_state) = (more_state.clone(), more_state.clone());
            let (server, remove_id) = (edited.clone(), id.clone());
            let items = vec![
                MenuItem::action(
                    "Edit",
                    Some(Icon::Pencil),
                    Rc::new(move |_, cx: &mut App| {
                        let server = server.clone();
                        edit_state.update(cx, |state, cx| {
                            state.open_connect(Some(server));
                            cx.notify();
                        });
                    }),
                ),
                MenuItem::danger(
                    "Remove",
                    Some(Icon::Delete),
                    Rc::new(move |_, cx: &mut App| {
                        let id = remove_id.clone();
                        remove_state.update(cx, |state, cx| {
                            state.remove_server(&id, cx);
                            cx.notify();
                        });
                    }),
                ),
            ];
            more_menu.update(cx, |menu, cx| {
                menu.set_width(160.0);
                menu.open_under(bounds, items, window, cx);
            });
        }))
        .into_any_element()
    };

    let connect = state.clone();
    let body = vec![
        Button::new("home-connect", ButtonKind::Secondary)
            .label("Connect a host")
            .icon(Icon::Server)
            .icon_size(16.0)
            .full_width()
            .on_click(move |_, cx| {
                connect.update(cx, |state, cx| {
                    state.open_connect(None);
                    cx.notify();
                });
            })
            .into_any_element(),
        start_section(
            "Recents",
            recents.iter().map(|server| row(server, "recent")).collect(),
            cx,
        ),
        start_section(
            "Saved",
            others.iter().map(|server| row(server, "saved")).collect(),
            cx,
        ),
    ];
    start_page(MascotMood::Sleeping, body, cx)
}
