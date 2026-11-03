//! Vitals in GPUI: live metrics of the hosts you reach over SSH, read from the vitals-agent.

pub mod app;
pub mod format;
pub mod installer;
pub mod protocol;
pub mod session;
pub mod shots;
pub mod ssh;
pub mod state;
pub mod store;
pub mod views;

use app::{Quit, register_key_bindings};
use gpui::{App, AppContext as _, Application, Menu, MenuItem, px, size};
use polterware_ui::icons::IconAssets;
use polterware_ui::theme::{self, Theme};
use polterware_ui::window::vibrancy;

/// The design system's root size in the desktop apps.
const REM: f32 = 13.0;

/// Opens the Vitals window and runs the app until it quits.
pub fn run() {
    Application::new()
        .with_assets(IconAssets)
        .run(|cx: &mut App| {
            cx.activate(true);
            theme::set_root_rem(REM);
            cx.set_global(Theme::dark());
            polterware_ui::init(cx);
            register_key_bindings(cx);
            cx.set_menus(vec![Menu {
                name: "Vitals".into(),
                items: vec![MenuItem::action("Quit", Quit)],
            }]);
            cx.on_window_closed(|cx| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();

            let options = polterware_ui::window::window_options(
                "Vitals",
                size(px(1200.0), px(800.0)),
                size(px(800.0), px(560.0)),
                cx,
            );
            cx.open_window(options, |window, cx| {
                vibrancy::install(window);
                cx.new(|cx| app::AppView::new(window, cx))
            })
            .expect("failed to open GPUI window");
        });
}
