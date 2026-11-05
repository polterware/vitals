//! A host's page: the Metrics and Terminal tabs. Terminal is the Electron app's
//! `.vitals-terminal-pane`: the session's shell in polterware-ui's `TerminalView`, under the
//! toolbar. Metrics is the Electron app's
//! `MetricsDashboard`: CPU, memory, network, disk I/O, pressure, disks, temperatures and GPUs,
//! and processes, each a card of whole cells packed on the dashboard grid, with the history
//! charts over the settings' history window. A card moves by dragging its header or grip, or
//! with the arrow keys on its grip, and the layout it leaves is saved (`card-reorder.ts`).
//! A right click on a process opens its menu (copy its PID or command, or kill it). Before
//! the first metrics, the page shows the `EchoRing` while it connects.

use std::rc::Rc;

use gpui::{
    AnyElement, App, ClipboardItem, ElementId, Entity, FontWeight, IntoElement, Rgba, SharedString,
    Window, div, prelude::*, px,
};
use polterware_ui::button::{Button, ButtonKind};
use polterware_ui::chip::chip;
use polterware_ui::data_table::{Column, DataTable, RowCx, SortDirection, next_sort, sans};
use polterware_ui::echo_ring::EchoRing;
use polterware_ui::ellipsis::fade;
use polterware_ui::icon_tile::IconTile;
use polterware_ui::icons::Icon;
use polterware_ui::menu::MenuItem;
use polterware_ui::patterns::{
    AnchorSlot, ContextMenu, DashboardCard, GridCard, Level, Meter, PickRow, Reading, card_width,
    dashboard_grid, dashboard_grip, dashboard_measure, headline, meters, readings, title_card,
    toolbar,
};
use polterware_ui::scroll::{page_content, use_scroll};
use polterware_ui::segmented::SegmentedControl;
use polterware_ui::sort::{SortValue, sort_rows};
use polterware_ui::spark_chart::SparkChart;
use polterware_ui::status_dot::status_dot;
use polterware_ui::theme::{StatusTone, Theme};

use crate::format;
use crate::protocol::{Disk, DiskIo, Process, Snapshot};
use crate::session::Status;
use crate::settings::{NetworkUnit, Settings};
use crate::state::{AppState, CARDS, HostSession, HostTab, Route, card_size, tail};

/// The grid's first guess at its columns, before it has measured its width.
const GUESS_COLUMNS: u16 = 3;
/// `.vitals-section`'s padding on each side.
const CARD_PAD: f32 = 16.0;
/// One line of 12.5px text in the default density.
const TABLE_ROW: f32 = 28.75;
/// `.vitals-metrics`' padding.
const GRID_PAD: f32 = 12.0;
/// `.vitals-terminal-pane`'s padding: top, sides, bottom.
const TERMINAL_PAD: (f32, f32, f32) = (20.0, 16.0, 12.0);
/// Its height is the page's less 48px: the 44px toolbar and 4px under the pane.
const TERMINAL_UNDER: f32 = 4.0;
const TERMINAL_MIN_HEIGHT: f32 = 240.0;

pub fn host_page(
    state: &Entity<AppState>,
    menu: &Entity<ContextMenu>,
    id: u64,
    tab: HostTab,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let theme = *Theme::global(cx);
    let Some(session) = state.read(cx).session(id) else {
        return div().into_any_element();
    };
    let available = available_cards(session.snapshot.as_ref());
    let actions = if tab == HostTab::Metrics {
        let (columns, _) = dashboard_measure(grid_id(id), GUESS_COLUMNS, window, cx);
        layout_menu(state, menu, id, &available, columns, window, cx)
    } else {
        div().into_any_element()
    };
    let bar = toolbar(view_tabs(state, id, tab), actions, cx);
    if tab == HostTab::Terminal {
        let terminal = state.update(cx, |state, cx| state.ensure_terminal(id, window, cx));
        let (top, sides, bottom) = TERMINAL_PAD;
        return div()
            .size_full()
            .flex()
            .flex_col()
            .child(bar)
            .child(
                div()
                    .flex_1()
                    .min_h(px(TERMINAL_MIN_HEIGHT + TERMINAL_UNDER))
                    .pb(px(TERMINAL_UNDER))
                    .child(
                        div()
                            .size_full()
                            .pt(px(top))
                            .px(px(sides))
                            .pb(px(bottom))
                            .bg(theme.color_surface)
                            .children(terminal),
                    ),
            )
            .into_any_element();
    }
    let scroll = use_scroll(scroll_id(id), window, cx)
        .read(cx)
        .handle()
        .clone();
    let session = state.read(cx).session(id).expect("the session is open");
    let Some(snapshot) = session.snapshot.clone() else {
        return div()
            .size_full()
            .flex()
            .flex_col()
            .child(bar)
            .child(waiting(state, session, &theme))
            .into_any_element();
    };
    let settings = *state.read(cx).settings();
    let hidden: Vec<&'static str> = CARDS
        .iter()
        .map(|(card, _)| *card)
        .filter(|card| state.read(cx).card_hidden(card))
        .collect();
    let host = HostView::of(session, settings.history_samples());
    let content = dashboard(
        &Dashboard {
            state,
            menu,
            session: &host,
            snapshot: &snapshot,
            settings,
            theme,
            hidden,
            scroll,
        },
        window,
        cx,
    );
    page_content(scroll_id(id), bar, GRID_PAD, content, window, cx)
}

/// The dashboard grid of session `id`.
fn grid_id(id: u64) -> ElementId {
    ElementId::from(("host-dashboard", id as usize))
}

/// The page scroll of session `id`.
fn scroll_id(id: u64) -> ElementId {
    ElementId::from(("host-scroll", id as usize))
}

/// The Metrics and Terminal tabs (`PageToolbar.Tabs`).
fn view_tabs(state: &Entity<AppState>, id: u64, tab: HostTab) -> impl IntoElement + use<> {
    let go = state.clone();
    SegmentedControl::new(SharedString::from(format!("host-view-{id}")))
        .options(["Metrics", "Terminal"])
        .selected(match tab {
            HostTab::Metrics => 0,
            HostTab::Terminal => 1,
        })
        .on_select(move |index, window, cx| {
            let target = if index == 0 {
                HostTab::Metrics
            } else {
                HostTab::Terminal
            };
            go.update(cx, |state, cx| {
                state.go(Route::Host(id, target));
                if target == HostTab::Terminal
                    && let Some(terminal) = state.ensure_terminal(id, window, cx)
                {
                    terminal.read(cx).focus(window);
                }
                cx.notify();
            });
        })
}

/// The cards this host has data for (`available`).
fn available_cards(snapshot: Option<&Snapshot>) -> Vec<&'static str> {
    let Some(snapshot) = snapshot else {
        return Vec::new();
    };
    CARDS
        .iter()
        .map(|(card, _)| *card)
        .filter(|card| match *card {
            "network" => !snapshot.network.is_empty(),
            "diskio" => !snapshot.disk_io.is_empty(),
            "pressure" => snapshot.pressure.is_some(),
            "thermal" => !snapshot.thermals.is_empty() || !snapshot.gpus.is_empty(),
            _ => true,
        })
        .collect()
}

/// `CardLayoutMenu`: the 30px Layout pill, and its menu of which cards show, Auto-layout (the
/// shown cards packed with no holes on the grid's `columns`), and Reset layout.
fn layout_menu(
    state: &Entity<AppState>,
    menu: &Entity<ContextMenu>,
    id: u64,
    available: &[&'static str],
    columns: u16,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let anchor = window
        .use_keyed_state(("layout-anchor", id as usize), cx, |_, _| {
            AnchorSlot::default()
        })
        .read(cx)
        .clone();
    let (state, menu, available) = (state.clone(), menu.clone(), available.to_vec());
    let slot = anchor.clone();
    let trigger = title_card(
        ("layout-trigger", id as usize),
        Icon::LayoutDashboard,
        false,
        Rc::new(move |window, cx: &mut App| {
            let Some(bounds) = slot.get() else {
                return;
            };
            let app = state.read(cx);
            let mut items: Vec<MenuItem> = CARDS
                .iter()
                .filter(|(card, _)| available.contains(card))
                .map(|(card, title)| {
                    let (toggle, available) = (state.clone(), available.clone());
                    MenuItem::check(
                        *title,
                        !app.card_hidden(card),
                        Rc::new(move |_, cx: &mut App| {
                            toggle.update(cx, |state, cx| {
                                state.toggle_card(card, &available);
                                cx.notify();
                            });
                        }),
                    )
                })
                .collect();
            let (pack, shown) = (
                state.clone(),
                available
                    .iter()
                    .copied()
                    .filter(|card| !app.card_hidden(card))
                    .collect::<Vec<_>>(),
            );
            let reset = state.clone();
            items.push(MenuItem::Separator);
            items.push(MenuItem::action(
                "Auto-layout",
                Some(Icon::Layers),
                Rc::new(move |_, cx: &mut App| {
                    pack.update(cx, |state, cx| {
                        state.auto_layout_cards(&shown, columns);
                        cx.notify();
                    });
                }),
            ));
            items.push(MenuItem::action(
                "Reset layout",
                Some(Icon::Undo2),
                Rc::new(move |_, cx: &mut App| {
                    reset.update(cx, |state, cx| {
                        state.reset_cards();
                        cx.notify();
                    });
                }),
            ));
            menu.update(cx, |menu, cx| {
                menu.set_width(260.0);
                menu.open_under(bounds, items, window, cx);
            });
        }),
        window,
        cx,
    );
    div()
        .relative()
        .child(anchor.probe())
        .child(trigger)
        .into_any_element()
}

fn muted_line(text: impl Into<SharedString>, theme: &Theme) -> gpui::Div {
    div()
        .text_size(px(12.0))
        .text_color(theme.color_text_muted)
        .child(text.into())
}

/// Before the first metrics: `.vitals-connecting` while it connects, and the page with
/// Close and Retry once it failed.
fn waiting(state: &Entity<AppState>, session: &HostSession, theme: &Theme) -> AnyElement {
    let failed = matches!(session.status, Status::Error | Status::Disconnected);
    if !failed {
        let title = if session.status == Status::Reconnecting {
            format!("Reconnecting to {}", session.label)
        } else {
            format!("Connecting to {}", session.label)
        };
        let message = session
            .message
            .clone()
            .unwrap_or_else(|| "Waiting for the first metrics...".into());
        return div()
            .flex_1()
            .min_h(px(0.0))
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(6.0))
            .p(px(32.0))
            // `.vitals-connecting-echo`: 3px dots, a muted peak, at rest on the selected fill.
            .child(
                div().mb(px(12.0)).child(
                    EchoRing::new(("connecting-echo", session.id as usize))
                        .dot(3.0)
                        .peak(theme.color_text_muted)
                        .rest(theme.color_surface_selected),
                ),
            )
            .child(
                div()
                    .text_size(px(13.0))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme.color_text_secondary)
                    .child(title),
            )
            .child(muted_line(message, theme))
            .into_any_element();
    }
    let title = if session.status == Status::Disconnected {
        "Disconnected"
    } else {
        "Connection failed"
    };
    let id = session.id;
    let close = state.clone();
    let retry = state.clone();
    let (label, target, server_id) = (
        session.label.clone(),
        session.target.clone(),
        session.server_id.clone(),
    );
    div()
        .flex_1()
        .min_h(px(0.0))
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(12.0))
        .child(
            div()
                .py(px(14.0))
                .mb(px(12.0))
                .child(IconTile::new(Icon::Server).size(56.0, 32.0)),
        )
        .child(
            div()
                .text_size(px(15.0))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme.color_text_primary)
                .child(title),
        )
        .children(
            session
                .message
                .clone()
                .map(|message| muted_line(message, theme)),
        )
        .child(
            div()
                .mt(px(12.0))
                .flex()
                .items_center()
                .gap(px(8.0))
                .child(
                    Button::new("host-close", ButtonKind::Secondary)
                        .label("Close")
                        .on_click(move |_, cx| {
                            close.update(cx, |state, cx| {
                                state.close_session(id);
                                cx.notify();
                            });
                        }),
                )
                .child(
                    Button::new("host-retry", ButtonKind::Brand)
                        .label("Retry")
                        .icon(Icon::RefreshCw)
                        .on_click(move |_, cx| {
                            let (label, target, server_id) =
                                (label.clone(), target.clone(), server_id.clone());
                            retry.update(cx, |state, cx| {
                                state.close_session(id);
                                state.connect(label, target, server_id, None, cx);
                                cx.notify();
                            });
                        }),
                ),
        )
        .into_any_element()
}

/// What the dashboard draws of a session, taken out of the app state so the page can build
/// with the window: its status, whether the script fallback reads it, and the history window
/// of its charts.
struct HostView {
    id: u64,
    status: Status,
    message: Option<String>,
    fallback: bool,
    cpu: Vec<f32>,
    cpu_display: Option<f64>,
    interfaces: std::collections::BTreeMap<String, (Vec<f32>, Vec<f32>)>,
}

impl HostView {
    fn of(session: &HostSession, samples: usize) -> Self {
        Self {
            id: session.id,
            status: session.status,
            message: session.message.clone(),
            fallback: session.fallback.is_some(),
            cpu: tail(&session.history.cpu, samples),
            cpu_display: session.history.cpu_display,
            interfaces: session
                .history
                .interfaces
                .iter()
                .map(|(name, (rx, tx))| (name.clone(), (tail(rx, samples), tail(tx, samples))))
                .collect(),
        }
    }
}

struct Dashboard<'a> {
    state: &'a Entity<AppState>,
    menu: &'a Entity<ContextMenu>,
    session: &'a HostView,
    snapshot: &'a Snapshot,
    settings: Settings,
    theme: Theme,
    /// The cards the person hid.
    hidden: Vec<&'static str>,
    /// The page's scroll, which a held card scrolls near its edges.
    scroll: gpui::ScrollHandle,
}

/// The cards, in the Electron app's order (`CARD_IDS`), each with its size in cells. A card
/// shows only when the host reports its data.
fn dashboard(page: &Dashboard, window: &mut Window, cx: &mut App) -> AnyElement {
    let id = page.session.id;
    let (columns, measured) = dashboard_measure(grid_id(id), GUESS_COLUMNS, window, cx);
    let width = measured
        .unwrap_or_else(|| f32::from(window.viewport_size().width) - 2.0 * (8.0 + GRID_PAD));
    let inner = |span: u16| card_width(span, columns, width) - 2.0 * CARD_PAD;
    let snapshot = page.snapshot;

    let mut cards: Vec<GridCard> = Vec::new();
    for card in available_cards(Some(snapshot)) {
        if page.hidden.contains(&card) {
            continue;
        }
        let built = match card {
            "cpu" => cpu_card(page, inner(1), cx),
            "memory" => memory_card(page, cx),
            "network" => network_card(page, window, cx),
            "diskio" => disk_io_card(page, window, cx),
            "pressure" => pressure_card(page, cx),
            "disk" => disks_card(page, window, cx),
            "thermal" => thermal_card(page, inner(1), cx),
            _ => processes_card(page, window, cx),
        };
        let grip = dashboard_grip(grid_id(id), card, window, cx);
        cards.push(GridCard::new(card, card_size(card), built.grip(grip)));
    }
    let layout = page.state.read(cx).card_layout();
    let save = page.state.clone();
    let toolbar_height = page.theme.page_header_height;

    div()
        .w_full()
        .flex()
        .flex_col()
        .gap(px(GRID_PAD))
        .children(status_head(page.session, &page.theme, cx))
        .child(
            dashboard_grid(grid_id(id))
                .guess(GUESS_COLUMNS)
                .layout(&layout)
                .cards(cards)
                .scroll(page.scroll.clone(), toolbar_height)
                .on_change(Rc::new(move |layout, _, cx: &mut App| {
                    save.update(cx, |state, cx| {
                        state.set_card_layout(&layout);
                        cx.notify();
                    });
                }))
                .build(window, cx),
        )
        .into_any_element()
}

/// `.vitals-status-head`: the "Script fallback" chip while the script reads the host, and,
/// while the session is not connected, its status and message over the last metrics.
fn status_head(session: &HostView, theme: &Theme, cx: &App) -> Option<AnyElement> {
    let message = session
        .message
        .clone()
        .filter(|_| session.status != Status::Connected);
    if !session.fallback && message.is_none() {
        return None;
    }
    Some(
        div()
            .pt(px(GRID_PAD))
            .flex()
            .items_center()
            .gap(px(8.0))
            .when(session.fallback, |head| {
                head.child(chip("Script fallback", StatusTone::Warn, cx))
            })
            .when_some(message, |head, message| {
                head.child(status_dot(session.status.tone_key()))
                    .child(
                        div()
                            .text_size(px(theme.font_size_sm))
                            .text_color(theme.color_text_secondary)
                            .child(session.status.label()),
                    )
                    .child(muted_line(message, theme))
            })
            .into_any_element(),
    )
}

/// `loadLevel`: cyan under 60%, amber under 85%, red above.
fn load_color(percent: f64, theme: &Theme) -> Rgba {
    Level::of(percent, 60.0, 85.0).color(theme)
}

/// `tempLevel`: cyan under 65 °C, amber under 80, red above.
fn temp_color(celsius: f64, theme: &Theme) -> Rgba {
    Level::of(celsius, 65.0, 80.0).color(theme)
}

fn cpu_card(page: &Dashboard, inner: f32, cx: &App) -> DashboardCard {
    let (snapshot, theme, id) = (page.snapshot, &page.theme, page.session.id as usize);
    let samples = page.settings.history_samples();
    let [load1, load5, load15] = snapshot.load;
    let mut card = DashboardCard::new(("card-cpu", id), "CPU")
        .icon(Icon::Cpu)
        .child(headline(
            format::percent(
                page.session
                    .cpu_display
                    .unwrap_or(snapshot.cpu_total_percent),
                1,
            ),
            format!("Load {load1:.2} / {load5:.2} / {load15:.2}"),
            None,
            cx,
        ));
    if !snapshot.cpu_cores.is_empty() {
        card = card.child(meters(
            snapshot
                .cpu_cores
                .iter()
                .map(|core| {
                    Meter::new(
                        core.name.clone(),
                        format::percent(core.usage_percent, 0),
                        (core.usage_percent / 100.0) as f32,
                        load_color(core.usage_percent, theme),
                    )
                })
                .collect(),
            inner,
            cx,
        ));
    }
    card.child(
        SparkChart::new(("cpu-chart", id), page.session.cpu.clone())
            .slots(samples)
            .label("CPU %"),
    )
}

fn memory_card(page: &Dashboard, cx: &App) -> DashboardCard {
    let (memory, theme) = (&page.snapshot.memory, &page.theme);
    let used = page.snapshot.memory_fraction() * 100.0;
    let swap = if memory.swap_total_kb > 0.0 {
        (memory.swap_used_kb / memory.swap_total_kb * 100.0).clamp(0.0, 100.0)
    } else {
        0.0
    };
    DashboardCard::new(("card-memory", page.session.id as usize), "Memory")
        .icon(Icon::MemoryStick)
        .child(headline(
            format::percent(used, 1),
            format!(
                "{} / {}",
                format::kilobytes(memory.used_kb),
                format::kilobytes(memory.total_kb)
            ),
            Some(((used / 100.0) as f32, load_color(used, theme))),
            cx,
        ))
        .child(readings(
            vec![
                Reading::split(
                    &format::kilobytes(memory.swap_used_kb),
                    format!("Swap of {}", format::kilobytes(memory.swap_total_kb)),
                )
                .bar((swap / 100.0) as f32, load_color(swap, theme)),
                Reading::new(format::uptime(page.snapshot.uptime_seconds), "Uptime"),
            ],
            Some(2),
            0.0,
            cx,
        ))
}

/// The interfaces, which pick the one the charts draw (the first until one is picked), and
/// its receive and send history.
fn network_card(page: &Dashboard, window: &mut Window, cx: &mut App) -> DashboardCard {
    let (snapshot, theme, id) = (page.snapshot, page.theme, page.session.id as usize);
    let unit = page.settings.network_unit;
    let picked = window.use_keyed_state(("network-pick", id), cx, |_, _| None::<String>);
    let selected = picked
        .read(cx)
        .clone()
        .filter(|name| snapshot.network.iter().any(|iface| &iface.name == name))
        .or_else(|| snapshot.network.first().map(|iface| iface.name.clone()));
    let rows =
        div()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .children(snapshot.network.iter().enumerate().map(|(index, iface)| {
                let name = iface.name.clone();
                let pick = picked.clone();
                PickRow::new(("network-iface", index))
                    .cells([
                        iface.name.clone(),
                        format!(
                            "Down {}",
                            format::network_rate(iface.rx_bytes_per_sec, unit)
                        ),
                        format!("Up {}", format::network_rate(iface.tx_bytes_per_sec, unit)),
                    ])
                    .selected(selected.as_deref() == Some(iface.name.as_str()))
                    .on_click(Rc::new(move |_, cx: &mut App| {
                        let name = name.clone();
                        pick.update(cx, |picked, cx| {
                            *picked = Some(name);
                            cx.notify();
                        });
                    }))
            }));
    let mut card = DashboardCard::new(("card-network", id), "Network")
        .icon(Icon::ArrowDataTransferVertical)
        .child(rows);
    if let Some(name) = selected {
        let samples = page.settings.history_samples();
        let scale = match unit {
            NetworkUnit::Bytes => 1.0,
            NetworkUnit::Bits => 8.0,
        };
        let series = page.session.interfaces.get(&name);
        let chart = |key: &'static str, caption: &str, color: Rgba, rx: bool| {
            let values: Vec<f32> = series
                .map(|(down, up)| if rx { down } else { up })
                .into_iter()
                .flatten()
                .map(|value| value * scale)
                .collect();
            div()
                .flex()
                .flex_col()
                .gap(px(4.0))
                .child(muted_line(
                    format!("{caption} {name} ({})", format::rate_unit(unit)),
                    &theme,
                ))
                .child(
                    SparkChart::new((key, id), values)
                        .slots(samples)
                        .height(80.0)
                        .color(color)
                        .label(caption.to_string()),
                )
        };
        card = card
            .child(chart("network-down", "Down", theme.chart_cyan, true))
            .child(chart("network-up", "Up", theme.chart_blue, false));
    }
    card
}

/// A table whose header sorts it: `default` until the person picks a column, then the
/// order they picked, kept in the window under `key`.
struct Sorted<T> {
    rows: Rc<Vec<T>>,
    sort: Option<(usize, SortDirection)>,
    state: Entity<Option<(usize, SortDirection)>>,
}

fn sorted<T: Clone + 'static>(
    key: ElementId,
    rows: &[T],
    default: Option<(usize, SortDirection)>,
    by: impl Fn(&T, usize) -> SortValue,
    window: &mut Window,
    cx: &mut App,
) -> Sorted<T> {
    let state = window.use_keyed_state(key, cx, move |_, _| default);
    let sort = *state.read(cx);
    let rows = match sort {
        Some((column, direction)) => sort_rows(rows, direction, |row| by(row, column)),
        None => rows.to_vec(),
    };
    Sorted {
        rows: Rc::new(rows),
        sort,
        state,
    }
}

fn table<T: 'static>(
    id: ElementId,
    columns: Vec<Column>,
    sorted: &Sorted<T>,
    cells: impl Fn(&T) -> Vec<String> + 'static,
    cx: &App,
) -> DataTable {
    let theme = Theme::global(cx);
    let font = sans(theme, FontWeight::NORMAL);
    let rows = sorted.rows.clone();
    let state = sorted.state.clone();
    DataTable::new(id, columns, rows.len(), move |row: &RowCx, _, _| {
        cells(&rows[row.index])
            .into_iter()
            .map(|text| {
                div()
                    .min_w(px(0.0))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .font(font.clone())
                    .text_size(px(12.5))
                    .text_color(row.ink)
                    .child(fade(SharedString::from(text)))
                    .into_any_element()
            })
            .collect()
    })
    .row_height(TABLE_ROW)
    .default_density()
    // The web table's gutters, inside the card's padding.
    .gutters(12.0)
    // `.vitals-card-body.pw-scroll-fade`: the rows past the card fade into it.
    .fades(theme.color_surface_subtle)
    .sort(sorted.sort, move |column, _, cx| {
        state.update(cx, |sort, cx| {
            *sort = next_sort(*sort, column);
            cx.notify();
        });
    })
}

fn number(value: f64) -> SortValue {
    SortValue::Number(value)
}

fn text(value: &str) -> SortValue {
    SortValue::Text(value.to_string())
}

/// A card whose body is one table filling it.
fn table_card(id: ElementId, title: &'static str, icon: Icon, table: DataTable) -> DashboardCard {
    DashboardCard::new(id, title)
        .icon(icon)
        .packed()
        .child(div().flex_1().min_h(px(0.0)).child(table))
}

fn disk_io_card(page: &Dashboard, window: &mut Window, cx: &mut App) -> DashboardCard {
    let id = page.session.id as usize;
    let rows = sorted(
        ("disk-io-sort", id).into(),
        &page.snapshot.disk_io,
        None,
        |io: &DiskIo, column| match column {
            0 => text(&io.device),
            1 => number(io.read_bytes_per_sec),
            2 => number(io.write_bytes_per_sec),
            _ => number(io.util_percent),
        },
        window,
        cx,
    );
    let columns = ["Device", "Read", "Write", "Util"]
        .map(|title| Column::new(title).sortable())
        .to_vec();
    let table = table(
        ("disk-io", id).into(),
        columns,
        &rows,
        |io: &DiskIo| {
            vec![
                io.device.clone(),
                format::rate(io.read_bytes_per_sec),
                format::rate(io.write_bytes_per_sec),
                format::percent(io.util_percent, 0),
            ]
        },
        cx,
    );
    table_card(
        ("card-disk-io", id).into(),
        "Disk I/O",
        Icon::Activity,
        table,
    )
}

fn disks_card(page: &Dashboard, window: &mut Window, cx: &mut App) -> DashboardCard {
    let id = page.session.id as usize;
    let rows = sorted(
        ("disks-sort", id).into(),
        &page.snapshot.disks,
        None,
        |disk: &Disk, column| match column {
            0 => text(&disk.mount_point),
            1 => number(disk.used_kb),
            2 => number(disk.size_kb),
            _ => number(disk.use_percent),
        },
        window,
        cx,
    );
    let columns = ["Mount", "Used", "Size", "Use %"]
        .map(|title| Column::new(title).sortable())
        .to_vec();
    let table = table(
        ("disks", id).into(),
        columns,
        &rows,
        |disk: &Disk| {
            vec![
                disk.mount_point.clone(),
                format::kilobytes(disk.used_kb),
                format::kilobytes(disk.size_kb),
                format::percent(disk.use_percent, 0),
            ]
        },
        cx,
    );
    table_card(("card-disks", id).into(), "Disk", Icon::HardDrive, table)
}

/// `pressureLevel`: cyan under 10%, amber under 50%, red above.
fn pressure_color(value: f64, theme: &Theme) -> Rgba {
    Level::of(value, 10.0, 50.0).color(theme)
}

fn pressure_card(page: &Dashboard, cx: &App) -> DashboardCard {
    let theme = &page.theme;
    let card = DashboardCard::new(("card-pressure", page.session.id as usize), "Pressure")
        .icon(Icon::Gauge);
    let Some(pressure) = &page.snapshot.pressure else {
        return card.child(muted_line("No pressure data reported.", theme));
    };
    let tiles = [
        ("CPU", pressure.cpu),
        ("Memory", pressure.memory),
        ("I/O", pressure.io),
    ];
    let (peak_label, peak) =
        tiles.iter().copied().fold(
            tiles[0],
            |top, tile| if tile.1 > top.1 { tile } else { top },
        );
    card.child(headline(
        format::percent(peak, 1),
        format!("Highest stall: {peak_label}"),
        None,
        cx,
    ))
    .child(readings(
        tiles
            .iter()
            .map(|(label, value)| {
                Reading::new(format!("{value:.1}"), *label)
                    .unit("%")
                    .bar((value / 100.0) as f32, pressure_color(*value, theme))
            })
            .collect(),
        Some(3),
        0.0,
        cx,
    ))
}

/// Temperature zones, then each GPU's temperature, utilization, and VRAM (`sensorTiles`).
fn thermal_card(page: &Dashboard, inner: f32, cx: &App) -> DashboardCard {
    let (snapshot, theme) = (page.snapshot, &page.theme);
    let mut tiles: Vec<Reading> = snapshot
        .thermals
        .iter()
        .map(|zone| {
            Reading::new(format!("{:.1}", zone.celsius), zone.id.clone())
                .unit("°C")
                .bar(
                    (zone.celsius / 100.0) as f32,
                    temp_color(zone.celsius, theme),
                )
        })
        .collect();
    for gpu in &snapshot.gpus {
        let vram = if gpu.memory_total_mb > 0.0 {
            gpu.memory_used_mb / gpu.memory_total_mb * 100.0
        } else {
            0.0
        };
        tiles.push(
            Reading::new(
                format::number(gpu.temperature_c),
                format!("GPU {} temp", gpu.index),
            )
            .unit("°C")
            .bar(
                (gpu.temperature_c / 100.0) as f32,
                temp_color(gpu.temperature_c, theme),
            ),
        );
        tiles.push(
            Reading::new(
                format::number(gpu.utilization_percent),
                format!("GPU {} util", gpu.index),
            )
            .unit("%")
            .bar(
                (gpu.utilization_percent / 100.0) as f32,
                load_color(gpu.utilization_percent, theme),
            ),
        );
        tiles.push(
            Reading::new(format!("{vram:.0}"), format!("GPU {} VRAM", gpu.index))
                .unit("%")
                .bar((vram / 100.0) as f32, load_color(vram, theme)),
        );
    }
    DashboardCard::new(
        ("card-thermal", page.session.id as usize),
        "Temperature / GPU",
    )
    .icon(Icon::Zap)
    .child(readings(tiles, None, inner, cx))
}

/// The top processes, by CPU until the person sorts another column; a right click opens the
/// row's menu, and the row keeps the hover's fill while it is open.
fn processes_card(page: &Dashboard, window: &mut Window, cx: &mut App) -> DashboardCard {
    let session = page.session.id;
    let id = session as usize;
    let rows = sorted(
        ("processes-sort", id).into(),
        &page.snapshot.processes,
        Some((2, SortDirection::Descending)),
        |process: &Process, column| match column {
            0 => number(f64::from(process.pid)),
            1 => text(&process.user),
            2 => number(process.cpu_percent),
            3 => number(process.mem_percent),
            _ => text(&process.command),
        },
        window,
        cx,
    );
    let columns = vec![
        Column::new("PID").width(72.0).sortable(),
        Column::new("User").sortable(),
        Column::new("CPU %").width(88.0).sortable(),
        Column::new("MEM %").width(88.0).sortable(),
        Column::new("Command").sortable().shrinks(),
    ];
    let target = window.use_keyed_state(("process-menu-target", id), cx, |_, _| None::<u32>);
    if target.read(cx).is_some() && !page.menu.read(cx).is_open(cx) {
        target.update(cx, |target, _| *target = None);
    }
    let marked = target
        .read(cx)
        .and_then(|pid| rows.rows.iter().position(|process| process.pid == pid));
    let menu_rows = rows.rows.clone();
    let (menu, state) = (page.menu.clone(), page.state.clone());
    let table = table(
        ("processes", id).into(),
        columns,
        &rows,
        |process: &Process| {
            vec![
                process.pid.to_string(),
                process.user.clone(),
                format!("{:.1}", process.cpu_percent),
                format!("{:.1}", process.mem_percent),
                process.command.clone(),
            ]
        },
        cx,
    )
    .empty("No processes reported.")
    .marked(marked)
    .on_context(move |index, at, window, cx| {
        let Some(process) = menu_rows.get(index).cloned() else {
            return;
        };
        target.update(cx, |target, _| *target = Some(process.pid));
        let items = process_menu(&state, session, &process);
        menu.update(cx, |menu, cx| menu.open(at, items, window, cx));
    });
    table_card(
        ("card-processes", id).into(),
        "Processes",
        Icon::ListOrdered,
        table,
    )
}

/// The process menu (`ProcessTable`'s `ContextMenu`): copy its PID, copy its command, kill it.
fn process_menu(state: &Entity<AppState>, session: u64, process: &Process) -> Vec<MenuItem> {
    let copy = |text: String, label: &'static str| {
        let state = state.clone();
        Rc::new(move |_: &mut Window, cx: &mut App| {
            cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
            state.update(cx, |state, cx| {
                state.toast(format!("{label} copied"), false);
                cx.notify();
            });
        }) as polterware_ui::Handler
    };
    let pid = process.pid;
    let kill = state.clone();
    vec![
        MenuItem::action("Copy PID", Some(Icon::Copy), copy(pid.to_string(), "PID")),
        MenuItem::action(
            "Copy command",
            Some(Icon::SquareTerminal),
            copy(process.command.clone(), "Command"),
        ),
        MenuItem::Separator,
        MenuItem::danger(
            "Kill process",
            Some(Icon::Stop),
            Rc::new(move |_, cx: &mut App| {
                kill.update(cx, |state, cx| {
                    state.request_kill(session, pid);
                    cx.notify();
                });
            }),
        ),
    ]
}
