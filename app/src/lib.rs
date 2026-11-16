//! Vitals in GPUI: live metrics of the hosts you reach over SSH, read from the vitals-agent,
//! or from a shell script where the agent cannot run.

pub mod app;
pub mod format;
pub mod installer;
pub mod parsers;
pub mod protocol;
pub mod script;
pub mod secrets;
pub mod session;
pub mod settings;
pub mod shell;
pub mod shots;
pub mod ssh;
pub mod state;
pub mod store;
pub mod views;

use app::{Quit, register_key_bindings};
use gpui::{App, AppContext as _, Application, Menu, MenuItem, px, size};
use polterware_ui::icons::IconAssets;
use polterware_ui::theme::{self, Theme};
use polterware_ui::window::{app_icon, vibrancy};

/// The design system's root size in the desktop apps.
const REM: f32 = 13.0;

/// Applies the chosen app icon variant (or resets a bundled app back to its compiled icon when
/// `Default` is selected, falling back to the default PNG when unbundled).
pub(crate) fn apply_app_icon(icon: settings::AppIcon) {
    app_icon::apply(icon.override_png(), settings::AppIcon::Default.png());
}

/// Opens the Vitals window and runs the app until it quits.
pub fn run() {
    let app = Application::new().with_assets(IconAssets);
    // A click on the Dock icon brings back the window that closing hid.
    app.on_reopen(|cx| cx.activate(true));
    app.run(|cx: &mut App| {
        cx.activate(true);
        let (settings, _) = settings::load(&store::data_dir());
        apply_app_icon(settings.app_icon);
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
            // The close button hides the app instead of closing its one window, so the
            // window and its sessions stay as they were until the app quits.
            window.on_window_should_close(cx, |_, cx| {
                cx.hide();
                false
            });
            cx.new(|cx| app::AppView::new(window, cx))
        })
        .expect("failed to open GPUI window");
    });
}
