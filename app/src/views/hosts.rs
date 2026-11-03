//! Hosts, the start page (the Electron app's `HostHome`): every saved host and open session
//! in one table, with each connected host's live CPU, memory, load, network, and uptime. A
//! row opens the host, connecting it first when it is not open.

use std::rc::Rc;

use gpui::{
    AnyElement, App, Entity, FontWeight, IntoElement, Rgba, SharedString, Window, div, prelude::*,
    px,
};
use polterware_ui::button::{Button, ButtonKind};
use polterware_ui::data_table::{Column, DataTable, RowCx, primary_cell, sans};
use polterware_ui::icon_button::icon_button;
use polterware_ui::icons::{Icon, icon_colored};
use polterware_ui::patterns::{MascotMood, page_empty_with, toolbar};
use polterware_ui::progress_bar::progress_bar_sm;
use polterware_ui::scroll::page_fill;
use polterware_ui::status_dot::status_dot;
use polterware_ui::theme::{Theme, tw};

use crate::format;
use crate::state::{AppState, HostRow, HostTab, Route};
use crate::store::AuthMode;

const TITLES: [&str; 7] = [
    "Host", "Status", "CPU", "Memory", "Load", "Network", "Uptime",
];

pub fn hosts_page(state: &Entity<AppState>, _window: &mut Window, cx: &mut App) -> AnyElement {
    let theme = *Theme::global(cx);
    let rows = state.read(cx).host_rows();
    let open = state.clone();
    let connect = icon_button(
        "hosts-connect",
        Icon::Plus,
        "Connect a host",
        Rc::new(move |_, cx: &mut App| {
            open.update(cx, |state, cx| {
                state.open_connect(None);
                cx.notify();
            });
        }),
        cx,
    );
    let bar = toolbar(div(), connect, cx);
    if rows.is_empty() {
        let open = state.clone();
        let body = div()
            .flex()
            .justify_center()
            .child(
                Button::new("hosts-empty-connect", ButtonKind::Primary)
                    .label("Connect a host")
                    .icon(Icon::Server)
                    .on_click(move |_, cx| {
                        open.update(cx, |state, cx| {
                            state.open_connect(None);
                            cx.notify();
                        });
                    }),
            )
            .into_any_element();
        return div()
            .size_full()
            .flex()
            .flex_col()
            .child(bar)
            .child(page_empty_with(
                Some("No hosts yet"),
                MascotMood::Sleeping,
                Some(body),
                cx,
            ))
            .into_any_element();
    }
    div()
        .size_full()
        .flex()
        .flex_col()
        .child(page_fill(
            bar,
            tw(4.0),
            hosts_table(state, rows, &theme),
            cx,
        ))
        .into_any_element()
}

fn hosts_table(state: &Entity<AppState>, rows: Vec<HostRow>, theme: &Theme) -> DataTable {
    let rows = Rc::new(rows);
    let mut columns: Vec<Column> = TITLES
        .iter()
        .enumerate()
        .map(|(index, title)| {
            let column = Column::new(*title);
            match index {
                0 => column.min_width(200.0),
                2 | 3 => column.width(120.0),
                4 => column.width(64.0),
                _ => column,
            }
        })
        .collect();
    columns.push(Column::new("").width(96.0).fixed().end());
    let cells = rows.clone();
    let cell_state = state.clone();
    let activate_state = state.clone();
    let font = sans(theme, FontWeight::MEDIUM);
    DataTable::new("hosts", columns, rows.len(), move |row, _window, cx| {
        host_cells(&cells[row.index], row, &cell_state, font.clone(), cx)
    })
    .row_height(48.0)
    .empty("No hosts yet")
    .on_activate(move |index, _window, cx| {
        let row = rows[index].clone();
        activate_state.update(cx, |state, cx| {
            open_row(state, &row, cx);
            cx.notify();
        });
    })
}

/// Opens the row's session, or connects its saved host. A password, which this app does not
/// keep, opens the connect dialog instead.
fn open_row(state: &mut AppState, row: &HostRow, cx: &mut gpui::Context<AppState>) {
    if let Some(id) = row.session {
        state.go(Route::Host(id, HostTab::Metrics));
        return;
    }
    let Some(server) = &row.server else {
        return;
    };
    if server.auth_mode == AuthMode::Password {
        state.open_connect(Some(server.clone()));
    } else {
        state.connect_saved(server, None, cx);
    }
}

fn text(value: impl Into<SharedString>, ink: Rgba) -> AnyElement {
    div()
        .text_size(px(12.5))
        .text_color(ink)
        .whitespace_nowrap()
        .child(value.into())
        .into_any_element()
}

fn meter(fraction: f64, label: String, color: Rgba, ink: Rgba, cx: &App) -> AnyElement {
    div()
        .w_full()
        .flex()
        .items_center()
        .gap(px(8.0))
        .child(
            div()
                .flex_1()
                .child(progress_bar_sm(fraction as f32, color, cx)),
        )
        .child(
            div()
                .w(px(36.0))
                .flex()
                .justify_end()
                .child(text(label, ink)),
        )
        .into_any_element()
}

fn host_cells(
    row: &HostRow,
    cell: &RowCx,
    state: &Entity<AppState>,
    font: gpui::Font,
    cx: &mut App,
) -> Vec<AnyElement> {
    let theme = *Theme::global(cx);
    let ink = cell.ink;
    let muted = theme.color_text_muted;
    let app = state.read(cx);
    let session = row.session.and_then(|id| app.session(id));
    let status = session.map(|session| session.status);
    let snapshot = session.and_then(|session| session.snapshot.as_ref());
    let dash = || text("—", muted);

    let host = div()
        .w_full()
        .min_w(px(0.0))
        .flex()
        .flex_col()
        .child(primary_cell(
            icon_colored(Icon::Server, 14.0, theme.color_text_muted),
            SharedString::from(row.label.clone()),
            font,
            None,
            cx,
        ))
        .child(
            div()
                .pl(px(24.0))
                .text_size(px(11.5))
                .text_color(muted)
                .whitespace_nowrap()
                .overflow_hidden()
                .child(SharedString::from(row.address.clone())),
        )
        .into_any_element();
    let status_cell = match status {
        Some(status) => {
            let label = session
                .and_then(|session| session.message.clone())
                .filter(|_| status != crate::session::Status::Connected)
                .unwrap_or_else(|| status.label().to_string());
            div()
                .flex()
                .items_center()
                .gap(px(8.0))
                .min_w(px(0.0))
                .child(status_dot(status.tone_key()))
                .child(text(label, ink))
                .into_any_element()
        }
        None => text("Not connected", muted),
    };
    let mut cells = vec![host, status_cell];
    match snapshot {
        Some(snapshot) => {
            let memory = snapshot.memory_fraction();
            let (rx, tx) = snapshot.network_totals();
            cells.push(meter(
                snapshot.cpu_total_percent / 100.0,
                format::percent(snapshot.cpu_total_percent),
                theme.color_info,
                ink,
                cx,
            ));
            cells.push(meter(
                memory,
                format::percent(memory * 100.0),
                theme.color_success,
                ink,
                cx,
            ));
            cells.push(text(format!("{:.2}", snapshot.load[0]), ink));
            cells.push(text(
                format!("↓ {}  ↑ {}", format::rate(rx), format::rate(tx)),
                ink,
            ));
            cells.push(text(format::uptime(snapshot.uptime_seconds), ink));
        }
        None => cells.extend((0..5).map(|_| dash())),
    }
    cells.push(row_actions(row, cell.index, state, cx));
    cells
}

fn row_actions(row: &HostRow, index: usize, state: &Entity<AppState>, cx: &App) -> AnyElement {
    let mut actions = div().flex().items_center().justify_end().gap(px(4.0));
    if let Some(server) = row.server.clone() {
        let edit = state.clone();
        actions = actions.child(icon_button(
            ("host-edit", index),
            Icon::Pencil,
            "Edit",
            Rc::new(move |_, cx: &mut App| {
                cx.stop_propagation();
                edit.update(cx, |state, cx| {
                    state.open_connect(Some(server.clone()));
                    cx.notify();
                });
            }),
            cx,
        ));
    }
    if let Some(id) = row.session {
        let close = state.clone();
        actions = actions.child(icon_button(
            ("host-close", index),
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
        ));
    } else if let Some(server) = row.server.clone() {
        let remove = state.clone();
        actions = actions.child(icon_button(
            ("host-remove", index),
            Icon::Delete,
            "Remove",
            Rc::new(move |_, cx: &mut App| {
                cx.stop_propagation();
                remove.update(cx, |state, cx| {
                    state.remove_server(&server.id);
                    cx.notify();
                });
            }),
            cx,
        ));
    }
    actions.into_any_element()
}
