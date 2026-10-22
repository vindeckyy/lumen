//! `lumen`: the Lumen desktop app.
//!
//! Lists the hosts the user saved and the ones that answer on the network, pairs with a host
//! through the code it shows, and starts a `lumen-client` for the session. Built with GTK 4
//! and libadwaita, so it follows the desktop's light or dark style, accent colour, scaling and
//! accessibility settings.

mod session;
mod store;
mod ui;

use adw::prelude::*;

/// The application id. The stream window carries the same one, so the desktop groups it with
/// the app and shows the app's icon on it.
const APP_ID: &str = "io.lumen.Lumen";

fn main() -> gtk::glib::ExitCode {
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_startup(|_| {
        // A build that is not installed finds its icon next to its sources.
        if let Some(display) = gtk::gdk::Display::default() {
            let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/data/icons");
            if std::path::Path::new(dir).is_dir() {
                gtk::IconTheme::for_display(&display).add_search_path(dir);
            }
        }
        gtk::Window::set_default_icon_name(APP_ID);
    });
    app.connect_activate(|app| {
        // One window: a second launch raises the first.
        if let Some(window) = app.active_window() {
            window.present();
            return;
        }
        ui::build(app);
    });
    app.set_accels_for_action("win.add-host", &["<Control>n"]);
    app.set_accels_for_action("win.refresh", &["F5", "<Control>r"]);
    app.set_accels_for_action("win.preferences", &["<Control>comma"]);
    app.set_accels_for_action("win.shortcuts", &["<Control>question"]);
    app.set_accels_for_action("window.close", &["<Control>w"]);
    app.set_accels_for_action("app.quit", &["<Control>q"]);
    let quit = gtk::gio::ActionEntry::builder("quit")
        .activate(|app: &adw::Application, _, _| app.quit())
        .build();
    app.add_action_entries([quit]);
    app.run()
}
