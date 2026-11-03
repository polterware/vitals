//! A host's page: the Metrics and Terminal tabs. Metrics is the Electron app's
//! `MetricsDashboard` in its first form, a grid of cards over the latest snapshot.

use std::rc::Rc;

use gpui::{
    AnyElement, App, Entity, FontWeight, IntoElement, Rgba, SharedString, Window, div, prelude::*,
    px, relative,
};
use polterware_ui::button::{Button, ButtonKind};
use polterware_ui::card::card;
use polterware_ui::icon_button::icon_button;
use polterware_ui::icons::{Icon, icon_colored};
use polterware_ui::patterns::{MascotMood, page_empty_with, toolbar};
use polterware_ui::progress_bar::progress_bar_sm;
use polterware_ui::scroll::page_content;
use polterware_ui::status_dot::status_dot;
use polterware_ui::theme::{Theme, tw};

use crate::format;
use crate::protocol::Snapshot;
use crate::session::Status;
use crate::state::{AppState, HISTORY, HostSession, HostTab, Route};

const CARD_WIDTH: f32 = 320.0;
const PROCESS_ROWS: usize = 12;

pub fn host_page(
    state: &Entity<AppState>,
    id: u64,
    tab: HostTab,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let theme = *Theme::global(cx);
    let Some(session) = state.read(cx).session(id) else {
        return div().into_any_element();
    };
    let bar = toolbar(
        tabs(state, id, tab, &theme),
        session_actions(state, id, cx),
        cx,
    );
    if tab == HostTab::Terminal {
        return div()
            .size_full()
            .flex()
            .flex_col()
            .child(bar)
            .child(page_empty_with(
                Some("The terminal is not ported yet"),
                MascotMood::Sleeping,
                Some(
                    muted_line("The Electron app opens an xterm shell here.", &theme)
                        .into_any_element(),
                ),
                cx,
            ))
            .into_any_element();
    }
    let Some(snapshot) = session.snapshot.clone() else {
        return div()
            .size_full()
            .flex()
            .flex_col()
            .child(bar)
            .child(waiting(state, session, &theme, cx))
            .into_any_element();
    };
    let content = dashboard(session, &snapshot, &theme, cx);
    page_content(
        ("host-scroll", id as usize),
        bar,
        tw(4.0),
        content,
        window,
        cx,
    )
}

fn tabs(
    state: &Entity<AppState>,
    id: u64,
    tab: HostTab,
    theme: &Theme,
) -> impl IntoElement + use<> {
    let tab_button = |label: &'static str, target: HostTab| {
        let go = state.clone();
        Button::new(("host-tab", target as usize), ButtonKind::Ghost)
            .label(label)
            .selected(tab == target)
            .on_click(move |_, cx| {
                go.update(cx, |state, cx| {
                    state.go(Route::Host(id, target));
                    cx.notify();
                });
            })
    };
    div()
        .flex()
        .items_center()
        .gap(px(theme.space_1))
        .child(tab_button("Metrics", HostTab::Metrics))
        .child(tab_button("Terminal", HostTab::Terminal))
}

fn session_actions(state: &Entity<AppState>, id: u64, cx: &App) -> impl IntoElement + use<> {
    let close = state.clone();
    icon_button(
        "host-close-session",
        Icon::X,
        "Close session",
        Rc::new(move |_, cx: &mut App| {
            close.update(cx, |state, cx| {
                state.close_session(id);
                cx.notify();
            });
        }),
        cx,
    )
}

fn muted_line(text: impl Into<SharedString>, theme: &Theme) -> gpui::Div {
    div()
        .text_size(px(theme.font_size_sm))
        .text_color(theme.color_text_muted)
        .child(text.into())
}

/// Before the first metrics: the connection's progress, or what went wrong with a retry.
fn waiting(state: &Entity<AppState>, session: &HostSession, theme: &Theme, cx: &App) -> AnyElement {
    let failed = matches!(session.status, Status::Error | Status::Disconnected);
    let title = match session.status {
        Status::Error => "Connection failed",
        Status::Disconnected => "Disconnected",
        _ => "Connecting",
    };
    let message = session
        .message
        .clone()
        .unwrap_or_else(|| format!("{}@{}", session.target.username, session.target.host));
    let mut body = div()
        .flex()
        .flex_col()
        .items_center()
        .gap(px(theme.space_3))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(8.0))
                .child(status_dot(session.status.tone_key()))
                .child(muted_line(message, theme)),
        );
    if failed {
        let retry = state.clone();
        let id = session.id;
        let (label, target, server_id) = (
            session.label.clone(),
            session.target.clone(),
            session.server_id.clone(),
        );
        body = body.child(
            Button::new("host-retry", ButtonKind::Secondary)
                .label("Try again")
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
        );
    }
    page_empty_with(
        Some(title),
        if failed {
            MascotMood::Sad
        } else {
            MascotMood::Lost
        },
        Some(body.into_any_element()),
        cx,
    )
    .into_any_element()
}

fn dashboard(session: &HostSession, snapshot: &Snapshot, theme: &Theme, cx: &App) -> AnyElement {
    let mut grid = div().w_full().flex().flex_wrap().gap(px(tw(3.0)));
    grid = grid
        .child(cpu_card(session, snapshot, theme, cx))
        .child(memory_card(snapshot, theme, cx))
        .child(network_card(snapshot, theme));
    if !snapshot.disks.is_empty() {
        grid = grid.child(disks_card(snapshot, theme, cx));
    }
    if !snapshot.disk_io.is_empty() {
        grid = grid.child(disk_io_card(snapshot, theme));
    }
    if let Some(pressure) = &snapshot.pressure {
        let rows = [
            ("CPU", pressure.cpu),
            ("Memory", pressure.memory),
            ("I/O", pressure.io),
        ];
        grid = grid.child(metric_card(
            "Pressure",
            Icon::Gauge,
            theme,
            rows.iter()
                .map(|(label, value)| {
                    bar_row(
                        label,
                        *value / 100.0,
                        format::percent(*value),
                        theme.color_warning,
                        theme,
                        cx,
                    )
                })
                .collect(),
        ));
    }
    if !snapshot.thermals.is_empty() {
        grid = grid.child(metric_card(
            "Thermal",
            Icon::Zap,
            theme,
            snapshot
                .thermals
                .iter()
                .map(|zone| pair_row(&zone.id, format!("{:.0} °C", zone.celsius), theme))
                .collect(),
        ));
    }
    for gpu in &snapshot.gpus {
        grid = grid.child(metric_card(
            "GPU",
            Icon::Monitor,
            theme,
            vec![
                pair_row("Name", gpu.name.clone(), theme),
                bar_row(
                    "Usage",
                    gpu.utilization_percent / 100.0,
                    format::percent(gpu.utilization_percent),
                    theme.color_info,
                    theme,
                    cx,
                ),
                pair_row(
                    "Memory",
                    format!(
                        "{} of {}",
                        format::bytes(gpu.memory_used_mb * 1024.0 * 1024.0),
                        format::bytes(gpu.memory_total_mb * 1024.0 * 1024.0)
                    ),
                    theme,
                ),
                pair_row("Temperature", format!("{:.0} °C", gpu.temperature_c), theme),
            ],
        ));
    }
    if !snapshot.processes.is_empty() {
        grid = grid.child(processes_card(snapshot, theme));
    }
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap(px(tw(3.0)))
        .child(host_line(session, snapshot, theme))
        .child(grid)
        .into_any_element()
}

fn host_line(session: &HostSession, snapshot: &Snapshot, theme: &Theme) -> impl IntoElement {
    let mut parts = vec![format!(
        "{}@{}",
        session.target.username, session.target.host
    )];
    if let Some(hello) = &session.hello {
        parts.push(hello.hostname.clone());
        parts.push(format!("{} {}", hello.kernel, hello.arch));
        parts.push(format!("agent {}", hello.agent_version));
    }
    parts.push(format!("up {}", format::uptime(snapshot.uptime_seconds)));
    div()
        .flex()
        .items_center()
        .gap(px(8.0))
        .child(status_dot(session.status.tone_key()))
        .child(
            div()
                .text_size(px(theme.font_size_lg))
                .font_weight(FontWeight::SEMIBOLD)
                .child(SharedString::from(session.label.clone())),
        )
        .child(muted_line(parts.join(" · "), theme))
}

fn metric_card(
    title: &'static str,
    icon: Icon,
    theme: &Theme,
    rows: Vec<AnyElement>,
) -> AnyElement {
    metric_card_sized(title, icon, CARD_WIDTH, theme, rows)
}

fn metric_card_sized(
    title: &'static str,
    icon: Icon,
    width: f32,
    theme: &Theme,
    rows: Vec<AnyElement>,
) -> AnyElement {
    let header = div()
        .flex()
        .items_center()
        .gap(px(8.0))
        .child(icon_colored(icon, 14.0, theme.color_text_muted))
        .child(
            div()
                .text_size(px(theme.font_size_sm))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme.color_text_secondary)
                .child(title),
        )
        .into_any_element();
    MetricCard {
        width,
        header,
        rows,
    }
    .into_any_element()
}

/// A dashboard card: `Card` with its header and rows, `width` wide.
#[derive(IntoElement)]
struct MetricCard {
    width: f32,
    header: AnyElement,
    rows: Vec<AnyElement>,
}

impl RenderOnce for MetricCard {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = *Theme::global(cx);
        div().w(px(self.width)).flex_none().child(
            card(cx).child(
                div()
                    .p(px(tw(4.0)))
                    .flex()
                    .flex_col()
                    .gap(px(theme.space_3))
                    .child(self.header)
                    .children(self.rows),
            ),
        )
    }
}

fn pair_row(label: &str, value: impl Into<SharedString>, theme: &Theme) -> AnyElement {
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap(px(8.0))
        .text_size(px(12.5))
        .child(
            div()
                .min_w(px(0.0))
                .text_color(theme.color_text_secondary)
                .child(SharedString::from(label.to_string())),
        )
        .child(
            div()
                .flex_none()
                .text_color(theme.color_text_primary)
                .child(value.into()),
        )
        .into_any_element()
}

fn bar_row(
    label: &str,
    fraction: f64,
    value: String,
    color: Rgba,
    theme: &Theme,
    cx: &App,
) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap(px(4.0))
        .child(pair_row(label, value, theme))
        .child(progress_bar_sm(fraction as f32, color, cx))
        .into_any_element()
}

fn big_number(value: String, caption: String, theme: &Theme) -> AnyElement {
    div()
        .flex()
        .items_end()
        .gap(px(8.0))
        .child(
            div()
                .text_size(px(28.0))
                .line_height(px(30.0))
                .font_weight(FontWeight::SEMIBOLD)
                .child(value),
        )
        .child(muted_line(caption, theme).pb(px(3.0)))
        .into_any_element()
}

/// The last two minutes of total CPU, one bar a second, the newest on the right.
fn sparkline(history: &[f32], color: Rgba) -> AnyElement {
    let mut bars = div().h(px(40.0)).w_full().flex().items_end().gap(px(1.0));
    let empty = HISTORY.saturating_sub(history.len());
    for slot in 0..HISTORY {
        let value = slot.checked_sub(empty).map_or(0.0, |index| history[index]);
        bars = bars.child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .h(relative((value / 100.0).clamp(0.02, 1.0)))
                .rounded(px(1.0))
                .bg(if slot < empty {
                    Rgba { a: 0.0, ..color }
                } else {
                    color
                }),
        );
    }
    bars.into_any_element()
}

fn cpu_card(session: &HostSession, snapshot: &Snapshot, theme: &Theme, cx: &App) -> AnyElement {
    let history: Vec<f32> = session.cpu_history.iter().copied().collect();
    let mut rows = vec![
        big_number(
            format::percent(snapshot.cpu_total_percent),
            format!("load {}", format::load(snapshot.load)),
            theme,
        ),
        sparkline(&history, theme.color_info),
    ];
    let mut cores = div().flex().flex_wrap().gap(px(6.0));
    for core in &snapshot.cpu_cores {
        cores = cores.child(
            div()
                .w(px(56.0))
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(muted_line(
                    format!(
                        "{} {}",
                        core.name.trim_start_matches("cpu"),
                        format::percent(core.usage_percent)
                    ),
                    theme,
                ))
                .child(progress_bar_sm(
                    (core.usage_percent / 100.0) as f32,
                    theme.color_info,
                    cx,
                )),
        );
    }
    rows.push(cores.into_any_element());
    metric_card("CPU", Icon::Cpu, theme, rows)
}

fn memory_card(snapshot: &Snapshot, theme: &Theme, cx: &App) -> AnyElement {
    let memory = &snapshot.memory;
    let fraction = snapshot.memory_fraction();
    let mut rows = vec![
        big_number(
            format::percent(fraction * 100.0),
            format!(
                "{} of {}",
                format::kilobytes(memory.used_kb),
                format::kilobytes(memory.total_kb)
            ),
            theme,
        ),
        progress_bar_sm(fraction as f32, theme.color_success, cx).into_any_element(),
        pair_row("Available", format::kilobytes(memory.available_kb), theme),
    ];
    if memory.swap_total_kb > 0.0 {
        let swap = (memory.swap_used_kb / memory.swap_total_kb).clamp(0.0, 1.0);
        rows.push(bar_row(
            "Swap",
            swap,
            format!(
                "{} of {}",
                format::kilobytes(memory.swap_used_kb),
                format::kilobytes(memory.swap_total_kb)
            ),
            theme.color_warning,
            theme,
            cx,
        ));
    }
    metric_card("Memory", Icon::MemoryStick, theme, rows)
}

fn network_card(snapshot: &Snapshot, theme: &Theme) -> AnyElement {
    let (rx, tx) = snapshot.network_totals();
    let mut rows = vec![big_number(
        format!("↓ {}", format::rate(rx)),
        format!("↑ {}", format::rate(tx)),
        theme,
    )];
    let mut interfaces: Vec<_> = snapshot.network.iter().collect();
    interfaces.sort_by(|a, b| {
        (b.rx_bytes_per_sec + b.tx_bytes_per_sec)
            .total_cmp(&(a.rx_bytes_per_sec + a.tx_bytes_per_sec))
    });
    for interface in interfaces.into_iter().take(6) {
        rows.push(pair_row(
            &interface.name,
            format!(
                "↓ {}  ↑ {}",
                format::rate(interface.rx_bytes_per_sec),
                format::rate(interface.tx_bytes_per_sec)
            ),
            theme,
        ));
    }
    metric_card("Network", Icon::ArrowDataTransferVertical, theme, rows)
}

fn disks_card(snapshot: &Snapshot, theme: &Theme, cx: &App) -> AnyElement {
    let rows = snapshot
        .disks
        .iter()
        .map(|disk| {
            bar_row(
                &disk.mount_point,
                disk.use_percent / 100.0,
                format!(
                    "{} of {} · {}",
                    format::kilobytes(disk.used_kb),
                    format::kilobytes(disk.size_kb),
                    format::percent(disk.use_percent)
                ),
                theme.color_primary,
                theme,
                cx,
            )
        })
        .collect();
    metric_card("Disks", Icon::HardDrive, theme, rows)
}

fn disk_io_card(snapshot: &Snapshot, theme: &Theme) -> AnyElement {
    let rows = snapshot
        .disk_io
        .iter()
        .map(|io| {
            pair_row(
                &io.device,
                format!(
                    "R {}  W {} · {}",
                    format::rate(io.read_bytes_per_sec),
                    format::rate(io.write_bytes_per_sec),
                    format::percent(io.util_percent)
                ),
                theme,
            )
        })
        .collect();
    metric_card("Disk I/O", Icon::Activity, theme, rows)
}

fn processes_card(snapshot: &Snapshot, theme: &Theme) -> AnyElement {
    let mut processes: Vec<_> = snapshot.processes.iter().collect();
    processes.sort_by(|a, b| b.cpu_percent.total_cmp(&a.cpu_percent));
    let header = process_row(
        ["PID", "User", "CPU", "Mem", "Command"].map(String::from),
        theme.color_text_muted,
        theme,
    );
    let mut rows = vec![header];
    for process in processes.into_iter().take(PROCESS_ROWS) {
        rows.push(process_row(
            [
                process.pid.to_string(),
                process.user.clone(),
                format!("{:.1}", process.cpu_percent),
                format!("{:.1}", process.mem_percent),
                process.command.clone(),
            ],
            theme.color_text_primary,
            theme,
        ));
    }
    metric_card_sized(
        "Processes",
        Icon::ListOrdered,
        CARD_WIDTH * 2.0 + tw(3.0),
        theme,
        rows,
    )
}

fn process_row(cells: [String; 5], ink: Rgba, theme: &Theme) -> AnyElement {
    let [pid, user, cpu, mem, command] = cells;
    let fixed = |value: String, width: f32| {
        div()
            .w(px(width))
            .flex_none()
            .overflow_hidden()
            .whitespace_nowrap()
            .child(value)
    };
    div()
        .flex()
        .items_center()
        .gap(px(8.0))
        .text_size(px(12.0))
        .text_color(ink)
        .font_family(theme.font_mono)
        .child(fixed(pid, 56.0))
        .child(fixed(user, 88.0))
        .child(fixed(cpu, 44.0))
        .child(fixed(mem, 44.0))
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .overflow_hidden()
                .whitespace_nowrap()
                .child(command),
        )
        .into_any_element()
}
