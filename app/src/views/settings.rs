//! Settings (the Electron app's `SettingsDialog`), section for section and row for row:
//! General (Appearance: reduce transparency; Units: the network unit), Monitoring (Charts: the
//! history window; Processes: the kill confirmation and its signal), Connections (Sessions:
//! the close confirmation; Reconnect: reconnecting and its delay; Known hosts), and Advanced
//! (Layout: reset the cards; History: clear the recent hosts). Each change saves at once. The
//! rail's search finds a setting by its words (`SEARCH_ENTRIES`) and opens its section.

use std::rc::Rc;

use gpui::{
    AnyElement, App, Context, Entity, FontWeight, IntoElement, Render, SharedString, Window, div,
    prelude::*, px,
};
use polterware_ui::button::{Button, ButtonKind};
use polterware_ui::data_table::{Column, DataTable, sans, table_height};
use polterware_ui::ellipsis::fade;
use polterware_ui::icons::Icon;
use polterware_ui::menu::MenuItem;
use polterware_ui::patterns::{
    AnchorSlot, ContextMenu, DialogPose, SettingsNavItem, SettingsSearchEntry, settings_dialog,
    settings_group, settings_group_gap, settings_row,
};
use polterware_ui::select::select_trigger;
use polterware_ui::switch::Switch;
use polterware_ui::text_input::{FieldSize, TextInput};
use polterware_ui::theme::{Theme, rem};

use crate::protocol::Signal;
use crate::settings::{HISTORY_WINDOWS, NetworkUnit, RECONNECT_DELAY, Settings};
use crate::state::{AppState, SettingsSection};

/// `w-48`.
fn select_width() -> f32 {
    12.0 * rem()
}

/// The delay field's width as the web app renders it: its `w-24` gives way to the row.
const DELAY_WIDTH: f32 = 43.0;

/// A known host's row: the 32px Remove in the cell's 5px padding.
const HOST_ROW: f32 = 42.0;

/// What the search finds besides the sections' names (`SEARCH_ENTRIES`): each setting, the
/// group it sits in, and the words it answers to.
fn search_entries() -> Vec<SettingsSearchEntry> {
    let entry = |item: &str, section: &str, label: &str| {
        SettingsSearchEntry::new(item.to_string(), label.to_string()).section(section.to_string())
    };
    vec![
        entry("general", "Appearance", "Reduce transparency")
            .description("Solid window chrome instead of the frosted glass.")
            .keywords("glass vibrancy blur translucent opaque"),
        entry("general", "Units", "Network unit")
            .description("Bytes or bits per second.")
            .keywords("bandwidth rate bits bytes"),
        entry("monitoring", "Charts", "History window")
            .description("How much history the charts keep.")
            .keywords("samples minutes graph"),
        entry("monitoring", "Processes", "Confirm before kill").keywords("process signal prompt"),
        entry("monitoring", "Processes", "Default kill signal")
            .description("TERM or KILL.")
            .keywords("sigterm sigkill process"),
        entry(
            "connections",
            "Sessions",
            "Confirm before closing a session",
        )
        .keywords("tab close disconnect prompt"),
        entry("connections", "Reconnect", "Auto-reconnect").keywords("ssh drop retry"),
        entry("connections", "Reconnect", "Reconnect delay").keywords("seconds retry wait"),
        entry("connections", "Known hosts", "Known hosts")
            .description("Trusted host key fingerprints.")
            .keywords("ssh fingerprint key trust remove"),
        entry("advanced", "Layout", "Reset card layout").keywords("cards order dashboard default"),
        entry("advanced", "History", "Clear recent hosts").keywords("recents history"),
    ]
}

pub struct SettingsView {
    state: Entity<AppState>,
    menu: Entity<ContextMenu>,
    reduce_transparency: Entity<Switch>,
    confirm_kill: Entity<Switch>,
    confirm_close: Entity<Switch>,
    auto_reconnect: Entity<Switch>,
    delay: Entity<TextInput>,
    search: Entity<TextInput>,
    anchors: [AnchorSlot; 3],
    /// Where the dialog is in its spring, set by the app as it draws the modal.
    pose: DialogPose,
    /// The section last open, which the dialog keeps while it leaves.
    section: SettingsSection,
}

fn delay_text(settings: &Settings) -> String {
    let delay = settings.reconnect_delay_seconds;
    if delay.fract() == 0.0 {
        format!("{delay:.0}")
    } else {
        format!("{delay}")
    }
}

impl SettingsView {
    pub fn new(state: Entity<AppState>, menu: Entity<ContextMenu>, cx: &mut Context<Self>) -> Self {
        let settings = *state.read(cx).settings();
        let switch = |on: bool, change: fn(&mut Settings, bool), cx: &mut Context<Self>| {
            let state = state.clone();
            cx.new(move |cx| {
                Switch::new(on, cx).on_change(move |on, _, cx| {
                    state.update(cx, |state, cx| {
                        state.update_settings(|settings| change(settings, on));
                        cx.notify();
                    });
                })
            })
        };
        let reduce_transparency = switch(
            settings.reduce_transparency,
            |settings, on| settings.reduce_transparency = on,
            cx,
        );
        let confirm_kill = switch(
            settings.confirm_before_kill,
            |settings, on| settings.confirm_before_kill = on,
            cx,
        );
        let confirm_close = switch(
            settings.confirm_before_close_session,
            |settings, on| settings.confirm_before_close_session = on,
            cx,
        );
        let auto_reconnect = switch(
            settings.auto_reconnect,
            |settings, on| settings.auto_reconnect = on,
            cx,
        );
        let submit = state.clone();
        let delay = cx.new(|cx| {
            let mut input = TextInput::new(cx)
                .size(FieldSize::Md)
                .placeholder("3")
                .on_submit(move |text, _, cx| {
                    let value = text.trim().parse::<f64>().ok();
                    submit.update(cx, |state, cx| {
                        match value.filter(|value| RECONNECT_DELAY.contains(value)) {
                            Some(delay) => state.update_settings(|settings| {
                                settings.reconnect_delay_seconds = delay;
                            }),
                            None => state.toast("The delay is 1 to 300 seconds.", true),
                        }
                        cx.notify();
                    });
                });
            input.set_text(delay_text(&settings), cx);
            input
        });
        let close = state.clone();
        let search = cx.new(move |cx: &mut Context<TextInput>| {
            // Escape clears a query first, then closes the dialog; the field is mid-update
            // while it hears the key, so the change waits for the update to end.
            let field = cx.entity().downgrade();
            TextInput::new(cx)
                .size(FieldSize::Sm)
                .placeholder("Search settings")
                .on_cancel(move |_, cx| {
                    let (field, close) = (field.clone(), close.clone());
                    cx.defer(move |cx| {
                        let cleared = field
                            .update(cx, |input, cx| {
                                let had = !input.text().is_empty();
                                input.set_text("", cx);
                                had
                            })
                            .unwrap_or(false);
                        if !cleared {
                            close.update(cx, |state, cx| {
                                state.close_settings();
                                cx.notify();
                            });
                        }
                    });
                })
        });
        cx.observe(&search, |_, _, cx| cx.notify()).detach();
        Self {
            state,
            menu,
            reduce_transparency,
            confirm_kill,
            confirm_close,
            auto_reconnect,
            delay,
            search,
            anchors: Default::default(),
            pose: DialogPose::SHOWN,
            section: SettingsSection::General,
        }
    }

    /// Draws the dialog at `pose` of its spring.
    pub fn set_pose(&mut self, pose: DialogPose) {
        self.pose = pose;
    }

    /// Brings the controls back to the saved settings, as the dialog opens.
    pub fn sync(&mut self, cx: &mut Context<Self>) {
        let settings = *self.state.read(cx).settings();
        for (switch, on) in [
            (&self.reduce_transparency, settings.reduce_transparency),
            (&self.confirm_kill, settings.confirm_before_kill),
            (&self.confirm_close, settings.confirm_before_close_session),
            (&self.auto_reconnect, settings.auto_reconnect),
        ] {
            switch.update(cx, |switch, cx| {
                if switch.is_on() != on {
                    switch.set_on(on);
                    cx.notify();
                }
            });
        }
        self.delay
            .update(cx, |input, cx| input.set_text(delay_text(&settings), cx));
        self.search.update(cx, |input, cx| input.set_text("", cx));
    }

    /// A `Select` (`size="md" class="w-48"`): the picked label on the field, its options in a
    /// menu under it.
    fn select(
        &self,
        slot: usize,
        id: &'static str,
        options: Vec<(String, bool)>,
        pick: fn(&mut Settings, usize),
        cx: &App,
    ) -> AnyElement {
        let anchor = self.anchors[slot].clone();
        let picked = options
            .iter()
            .find(|(_, picked)| *picked)
            .map(|(label, _)| label.clone())
            .unwrap_or_default();
        let (state, menu, probe) = (self.state.clone(), self.menu.clone(), anchor.clone());
        let width = select_width();
        div()
            .relative()
            .w(px(width))
            .child(anchor.probe())
            .child(
                select_trigger(
                    id,
                    picked,
                    None,
                    Rc::new(move |window, cx: &mut App| {
                        let Some(bounds) = probe.get() else {
                            return;
                        };
                        let items = options
                            .iter()
                            .enumerate()
                            .map(|(index, (label, picked))| {
                                let state = state.clone();
                                MenuItem::radio(
                                    label.clone(),
                                    *picked,
                                    Rc::new(move |_, cx: &mut App| {
                                        state.update(cx, |state, cx| {
                                            state.update_settings(|settings| pick(settings, index));
                                            cx.notify();
                                        });
                                    }),
                                )
                            })
                            .collect();
                        menu.update(cx, |menu, cx| {
                            menu.set_width(width);
                            menu.open_under(bounds, items, window, cx);
                        });
                    }),
                    cx,
                )
                .w_full(),
            )
            .into_any_element()
    }

    fn general(&self, settings: &Settings, cx: &App) -> AnyElement {
        let unit = self.select(
            0,
            "settings-network-unit",
            vec![
                (
                    "Bytes per second".into(),
                    settings.network_unit == NetworkUnit::Bytes,
                ),
                (
                    "Bits per second".into(),
                    settings.network_unit == NetworkUnit::Bits,
                ),
            ],
            |settings, index| {
                settings.network_unit = if index == 0 {
                    NetworkUnit::Bytes
                } else {
                    NetworkUnit::Bits
                };
            },
            cx,
        );
        groups(vec![
            settings_group("Appearance")
                .child(
                    settings_row("Reduce transparency")
                        .description("Use solid window chrome instead of the frosted glass.")
                        .child(self.reduce_transparency.clone()),
                )
                .into_any_element(),
            settings_group("Units")
                .child(
                    settings_row("Network unit")
                        .description("How network throughput is shown.")
                        .child(unit),
                )
                .into_any_element(),
        ])
    }

    fn monitoring(&self, settings: &Settings, cx: &App) -> AnyElement {
        let history = self.select(
            1,
            "settings-history",
            HISTORY_WINDOWS
                .iter()
                .map(|minutes| {
                    (
                        format!("{minutes} minutes"),
                        *minutes == settings.history_window_minutes,
                    )
                })
                .collect(),
            |settings, index| settings.history_window_minutes = HISTORY_WINDOWS[index.min(2)],
            cx,
        );
        let signal = self.select(
            2,
            "settings-signal",
            vec![
                ("TERM".into(), settings.default_kill_signal == Signal::Term),
                ("KILL".into(), settings.default_kill_signal == Signal::Kill),
            ],
            |settings, index| {
                settings.default_kill_signal = if index == 0 {
                    Signal::Term
                } else {
                    Signal::Kill
                };
            },
            cx,
        );
        groups(vec![
            settings_group("Charts")
                .child(
                    settings_row("History window")
                        .description("About one sample per second.")
                        .child(history),
                )
                .into_any_element(),
            settings_group("Processes")
                .child(
                    settings_row("Confirm before kill")
                        .description("Ask before sending a signal to a process.")
                        .child(self.confirm_kill.clone()),
                )
                .child(
                    settings_row("Default kill signal")
                        .description("TERM asks politely; KILL stops immediately.")
                        .child(signal),
                )
                .into_any_element(),
        ])
    }

    /// The known hosts as the Electron app's table: host, fingerprint, and Remove.
    fn known_hosts(&self, cx: &App) -> AnyElement {
        let theme = *Theme::global(cx);
        let hosts = Rc::new(self.state.read(cx).known_hosts().to_vec());
        let rows = hosts.len();
        let state = self.state.clone();
        let font = sans(&theme, FontWeight::NORMAL);
        let table = DataTable::new(
            "settings-known-hosts",
            vec![
                Column::new("Host"),
                Column::new("Fingerprint").shrinks(),
                // `<span class="sr-only">Actions</span>`: a header with no label.
                Column::new("").width(102.0).resizable(false),
            ],
            rows,
            move |row, _, _| {
                let (key, fingerprint) = hosts[row.index].clone();
                let remove = state.clone();
                vec![
                    div()
                        .font(font.clone())
                        .text_size(px(12.5))
                        .text_color(row.ink)
                        .child(fade(SharedString::from(key.clone())))
                        .into_any_element(),
                    div()
                        .min_w(px(0.0))
                        .font_family(theme.font_mono)
                        .text_size(px(11.5))
                        .text_color(theme.color_text_secondary)
                        .child(fade(SharedString::from(fingerprint)))
                        .into_any_element(),
                    Button::new(("settings-known-remove", row.index), ButtonKind::Danger)
                        .label("Remove")
                        .on_click(move |_, cx| {
                            remove.update(cx, |state, cx| {
                                state.remove_known_host(&key);
                                cx.notify();
                            });
                        })
                        .into_any_element(),
                ]
            },
        )
        .row_height(HOST_ROW)
        .gutters(12.0)
        .empty("No known hosts yet.");
        let height = if rows == 0 {
            table_height(0, HOST_ROW) + 240.0
        } else {
            table_height(rows, HOST_ROW)
        };
        div().w_full().h(px(height)).child(table).into_any_element()
    }

    fn connections(&self, settings: &Settings, cx: &App) -> AnyElement {
        // `disabled={!autoReconnect}`: faded, and it takes no clicks.
        let delay = div()
            .relative()
            .w(px(DELAY_WIDTH))
            .child(self.delay.clone())
            .when(!settings.auto_reconnect, |field| {
                field.opacity(0.5).child(
                    div()
                        .id("settings-delay-off")
                        .absolute()
                        .inset_0()
                        .occlude(),
                )
            });
        groups(vec![
            settings_group("Sessions")
                .child(
                    settings_row("Confirm before closing a session")
                        .description("Ask before closing a connected tab.")
                        .child(self.confirm_close.clone()),
                )
                .into_any_element(),
            settings_group("Reconnect")
                .child(
                    settings_row("Auto-reconnect")
                        .description("Retry when an SSH session drops.")
                        .child(self.auto_reconnect.clone()),
                )
                .child(
                    settings_row("Reconnect delay")
                        .description("Seconds to wait, between 1 and 300.")
                        .child(delay),
                )
                .into_any_element(),
            settings_group("Known hosts")
                .description("Removing a host asks you to verify its key on the next connection.")
                .child(self.known_hosts(cx))
                .into_any_element(),
        ])
    }

    fn advanced(&self) -> AnyElement {
        let (reset, clear) = (self.state.clone(), self.state.clone());
        groups(vec![
            settings_group("Layout")
                .child(
                    settings_row("Reset card layout")
                        .description("Restore the default order and positions of dashboard cards.")
                        .child(
                            Button::new("settings-reset-layout", ButtonKind::Secondary)
                                .label("Reset")
                                .on_click(move |_, cx| {
                                    reset.update(cx, |state, cx| {
                                        state.reset_cards();
                                        state.toast("Card layout reset.", false);
                                        cx.notify();
                                    });
                                }),
                        ),
                )
                .into_any_element(),
            settings_group("History")
                .child(
                    settings_row("Clear recent hosts")
                        .description("Saved servers are kept.")
                        .child(
                            Button::new("settings-clear-recents", ButtonKind::Secondary)
                                .label("Clear")
                                .on_click(move |_, cx| {
                                    clear.update(cx, |state, cx| {
                                        state.clear_recents();
                                        state.toast("Recent hosts cleared.", false);
                                        cx.notify();
                                    });
                                }),
                        ),
                )
                .into_any_element(),
        ])
    }
}

fn groups(groups: Vec<AnyElement>) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap(px(settings_group_gap()))
        .children(groups)
        .into_any_element()
}

impl Render for SettingsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let app = self.state.read(cx);
        let section = app.settings_open().unwrap_or(self.section);
        self.section = section;
        let settings = *app.settings();
        let items = SettingsSection::ALL
            .iter()
            .map(|section| {
                let icon = match section {
                    SettingsSection::General => Icon::Cog,
                    SettingsSection::Monitoring => Icon::Activity,
                    SettingsSection::Connections => Icon::Plug,
                    SettingsSection::Advanced => Icon::Hammer,
                };
                SettingsNavItem::new(section.id(), section.label()).icon(icon)
            })
            .collect();
        let content = match section {
            SettingsSection::General => self.general(&settings, cx),
            SettingsSection::Monitoring => self.monitoring(&settings, cx),
            SettingsSection::Connections => self.connections(&settings, cx),
            SettingsSection::Advanced => self.advanced(),
        };
        let (state, search) = (self.state.clone(), self.search.clone());
        // A pick in the rail or the search opens its section and clears the query.
        let open = Rc::new(move |id: &str, cx: &mut App| {
            search.update(cx, |input, cx| input.set_text("", cx));
            let picked = SettingsSection::ALL
                .into_iter()
                .find(|section| section.id() == id);
            if let Some(picked) = picked {
                state.update(cx, |state, cx| {
                    state.open_settings(picked);
                    cx.notify();
                });
            }
        });
        let (select, found) = (open.clone(), open);
        let close = self.state.clone();
        let query = self.search.read(cx).text().to_string();
        settings_dialog(1.0)
            .pose(self.pose)
            .items(items)
            .active(section.id())
            .heading(section.label())
            .search(
                self.search.clone(),
                query,
                search_entries(),
                Rc::new(move |entry, _, cx| found(entry.item.as_ref(), cx)),
            )
            .on_select(Rc::new(move |id, _, cx| select(id.as_ref(), cx)))
            .on_close(Rc::new(move |_, cx| {
                close.update(cx, |state, cx| {
                    state.close_settings();
                    cx.notify();
                });
            }))
            .child(content)
    }
}
