//! The window: the host list, the session page and the dialogs.
//!
//! The state lives in one [`App`] behind an `Rc<RefCell>`. GTK runs some signal handlers
//! while a call is still on the stack (popping a page runs `popped`), so no borrow is held
//! across a call that can re-enter: handlers take what they need and release the borrow first.

use std::cell::RefCell;
use std::rc::{Rc, Weak};
use std::sync::mpsc;
use std::time::Duration;

use adw::prelude::*;
use gtk::{gio, glib};
use lumen_transport::mdns::FoundHost;

use crate::APP_ID;
use crate::session::{self, Launch, Session, Trouble, Update};
use crate::store::{self, Backend, Host, Store};

/// How often a running session is looked at.
const SESSION_POLL: Duration = Duration::from_millis(100);
/// How long one network scan listens for answers.
const SCAN_FOR: Duration = Duration::from_secs(3);
/// How often the network is scanned while the app is open.
const SCAN_EVERY: u32 = 30;
/// The session page's tag, which tells it apart when it is popped.
const SESSION_TAG: &str = "session";

const CSS: &str = "
.pairing-code { font-size: 1.8em; font-family: monospace; letter-spacing: 0.35em; }
.host-online { font-weight: bold; }
.session-log { font-family: monospace; font-size: 0.9em; }
";

type Shared = Rc<RefCell<App>>;

/// The state of the window.
struct App {
    window: adw::ApplicationWindow,
    toasts: adw::ToastOverlay,
    nav: adw::NavigationView,
    stack: gtk::Stack,
    saved_group: adw::PreferencesGroup,
    found_group: adw::PreferencesGroup,
    scan_spinner: adw::Spinner,
    welcome_scan: gtk::Box,
    rows: Vec<(adw::PreferencesGroup, adw::ActionRow)>,
    store: Store,
    /// Set when the store file could not be read: the app does not write over it.
    store_broken: bool,
    found: Vec<FoundHost>,
    scanning: bool,
    active: Option<Active>,
}

/// The session the app is running.
struct Active {
    host_id: String,
    name: String,
    address: String,
    session: Option<Session>,
    status: adw::StatusPage,
    buttons: gtk::Box,
    details: gtk::Expander,
    log: gtk::Label,
    pairing: Option<adw::AlertDialog>,
    streaming: bool,
    /// A stream was shown in this session, so a lost link is a reconnect, not a first try.
    streamed: bool,
    retrying: bool,
    cancelled: bool,
    /// A code was sent: a second request means the first one was wrong.
    answered: bool,
}

/// Builds the window and shows it.
pub fn build(app: &adw::Application) {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(CSS);
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }

    let (store, load_error) = match Store::load(&store::store_path()) {
        Ok(store) => (store, None),
        Err(e) => (Store::default(), Some(e)),
    };

    let toasts = adw::ToastOverlay::new();
    let nav = adw::NavigationView::new();
    toasts.set_child(Some(&nav));
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Lumen")
        .default_width(720)
        .default_height(620)
        .width_request(360)
        .height_request(400)
        .content(&toasts)
        .build();

    let saved_group = adw::PreferencesGroup::builder().title("Your Hosts").build();
    let scan_spinner = adw::Spinner::builder().visible(false).build();
    let found_group = adw::PreferencesGroup::builder()
        .title("On This Network")
        .header_suffix(&scan_spinner)
        .build();
    let list = adw::PreferencesPage::new();
    list.add(&saved_group);
    list.add(&found_group);

    let welcome_scan = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .halign(gtk::Align::Center)
        .build();
    welcome_scan.append(&adw::Spinner::new());
    welcome_scan.append(
        &gtk::Label::builder()
            .label("Looking for hosts on this network…")
            .css_classes(["dim-label"])
            .build(),
    );
    let add_first = gtk::Button::builder()
        .label("Add Host…")
        .halign(gtk::Align::Center)
        .action_name("win.add-host")
        .css_classes(["pill", "suggested-action"])
        .build();
    let welcome_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(24)
        .build();
    welcome_box.append(&add_first);
    welcome_box.append(&welcome_scan);
    let welcome = adw::StatusPage::builder()
        .icon_name(APP_ID)
        .title("Welcome to Lumen")
        .description(
            "Use another computer from this one, with its screen, sound and input.\n\
             Start <b>lumen-host</b> on that computer. It shows up here when it is on this \
             network, or add it by its address.",
        )
        .child(&welcome_box)
        .build();
    // The description carries a little markup.
    welcome.add_css_class("compact");

    let stack = gtk::Stack::builder()
        .transition_type(gtk::StackTransitionType::Crossfade)
        .build();
    stack.add_named(&welcome, Some("empty"));
    stack.add_named(&list, Some("list"));

    let header = adw::HeaderBar::new();
    header.pack_start(
        &gtk::Button::builder()
            .icon_name("list-add-symbolic")
            .tooltip_text("Add Host")
            .action_name("win.add-host")
            .build(),
    );
    let menu = gio::Menu::new();
    let section = gio::Menu::new();
    section.append(Some("_Preferences"), Some("win.preferences"));
    section.append(Some("_Keyboard Shortcuts"), Some("win.shortcuts"));
    section.append(Some("_About Lumen"), Some("win.about"));
    menu.append_section(None, &section);
    header.pack_end(
        &gtk::MenuButton::builder()
            .icon_name("open-menu-symbolic")
            .tooltip_text("Main Menu")
            .menu_model(&menu)
            .primary(true)
            .build(),
    );
    header.pack_end(
        &gtk::Button::builder()
            .icon_name("view-refresh-symbolic")
            .tooltip_text("Look for Hosts")
            .action_name("win.refresh")
            .build(),
    );
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&stack));
    nav.add(
        &adw::NavigationPage::builder()
            .title("Lumen")
            .tag("hosts")
            .child(&toolbar)
            .build(),
    );

    let shared: Shared = Rc::new(RefCell::new(App {
        window: window.clone(),
        toasts: toasts.clone(),
        nav: nav.clone(),
        stack,
        saved_group,
        found_group,
        scan_spinner,
        welcome_scan,
        rows: Vec::new(),
        store,
        store_broken: load_error.is_some(),
        found: Vec::new(),
        scanning: false,
        active: None,
    }));

    add_actions(&shared);
    {
        let weak = Rc::downgrade(&shared);
        nav.connect_popped(move |_, page| {
            if page.tag().as_deref() == Some(SESSION_TAG)
                && let Some(app) = weak.upgrade()
            {
                end_session(&app);
            }
        });
    }
    {
        // Closing the window ends the session too, so no client is left behind. This handler
        // holds the one strong reference: the state lives as long as the window does.
        let owner = Rc::clone(&shared);
        window.connect_close_request(move |_| {
            end_session(&owner);
            glib::Propagation::Proceed
        });
    }

    render_hosts(&shared);
    window.present();
    if let Some(error) = load_error {
        toast(
            &shared,
            &format!("Your saved hosts could not be read and will not be changed: {error}"),
        );
    }
    scan(&shared);
    let weak = Rc::downgrade(&shared);
    glib::timeout_add_seconds_local(SCAN_EVERY, move || {
        let Some(app) = weak.upgrade() else {
            return glib::ControlFlow::Break;
        };
        // No scan while streaming: it would only compete with the stream.
        if app.borrow().active.is_none() {
            scan(&app);
        }
        glib::ControlFlow::Continue
    });
}

/// The window actions: the menu, the shortcuts and the per-host actions of the rows.
fn add_actions(shared: &Shared) {
    let window = shared.borrow().window.clone();
    let simple = |name: &str, run: fn(&Shared)| {
        let action = gio::SimpleAction::new(name, None);
        let weak = Rc::downgrade(shared);
        action.connect_activate(move |_, _| {
            if let Some(app) = weak.upgrade() {
                run(&app);
            }
        });
        window.add_action(&action);
    };
    simple("add-host", |app| host_dialog(app, None));
    simple("refresh", scan);
    simple("preferences", preferences);
    simple("about", about);
    simple("shortcuts", shortcuts);

    let with_id = |name: &str, run: fn(&Shared, &str)| {
        let action = gio::SimpleAction::new(name, Some(glib::VariantTy::STRING));
        let weak = Rc::downgrade(shared);
        action.connect_activate(move |_, parameter| {
            if let (Some(app), Some(id)) = (
                weak.upgrade(),
                parameter.and_then(glib::Variant::get::<String>),
            ) {
                run(&app, &id);
            }
        });
        window.add_action(&action);
    };
    with_id("connect", connect);
    with_id("edit-host", |app, id| host_dialog(app, Some(id)));
    with_id("forget-host", forget);
    with_id("wake-host", wake);
    with_id("connect-found", connect_found);
}

fn toast(shared: &Shared, text: &str) {
    let toasts = shared.borrow().toasts.clone();
    toasts.add_toast(adw::Toast::builder().title(text).timeout(4).build());
}

/// Writes the store, unless the file on disk could not be read.
fn save(shared: &Shared) {
    let result = {
        let app = shared.borrow();
        if app.store_broken {
            return;
        }
        app.store.save(&store::store_path())
    };
    if let Err(e) = result {
        toast(shared, &format!("Cannot save: {e}"));
    }
}

/// Rebuilds both host lists.
fn render_hosts(shared: &Shared) {
    let mut app = shared.borrow_mut();
    for (group, row) in app.rows.drain(..) {
        group.remove(&row);
    }
    let now = store::now();
    let mut rows = Vec::new();
    for host in &app.store.hosts {
        let online = app.found.iter().any(|f| same_host(f, &host.address));
        let mut subtitle = host.address.clone();
        if let Some(then) = host.last_used {
            subtitle.push_str(" · ");
            subtitle.push_str(&store::last_used_text(now, then));
        }
        let row = adw::ActionRow::builder()
            .title(&host.name)
            .subtitle(&subtitle)
            .use_markup(false)
            .activatable(true)
            .action_name("win.connect")
            .action_target(&host.id.to_variant())
            .build();
        row.add_prefix(&gtk::Image::from_icon_name("computer-symbolic"));
        if online {
            row.add_suffix(
                &gtk::Label::builder()
                    .label("Online")
                    .css_classes(["success", "host-online", "caption"])
                    .valign(gtk::Align::Center)
                    .build(),
            );
        }
        row.add_suffix(&host_menu(host));
        row.add_suffix(
            &gtk::Image::builder()
                .icon_name("go-next-symbolic")
                .css_classes(["dim-label"])
                .build(),
        );
        app.saved_group.add(&row);
        rows.push((app.saved_group.clone(), row));
    }

    let unsaved = unsaved_hosts(&app);
    for (index, found) in unsaved.iter().enumerate() {
        let row = adw::ActionRow::builder()
            .title(&found.name)
            .subtitle(found_address(found))
            .use_markup(false)
            .activatable(true)
            .action_name("win.connect-found")
            .action_target(&index.to_string().to_variant())
            .build();
        row.add_prefix(&gtk::Image::from_icon_name("network-server-symbolic"));
        row.add_suffix(
            &gtk::Button::builder()
                .label("Connect")
                .valign(gtk::Align::Center)
                .action_name("win.connect-found")
                .action_target(&index.to_string().to_variant())
                .build(),
        );
        app.found_group.add(&row);
        rows.push((app.found_group.clone(), row));
    }
    app.rows = rows;

    app.saved_group.set_visible(!app.store.hosts.is_empty());
    app.found_group.set_description(if unsaved.is_empty() {
        Some("No other hosts answered. A host started with --advertise shows up here.")
    } else {
        None
    });
    show_scanning(&app);
    let empty = app.store.hosts.is_empty() && unsaved.is_empty();
    app.stack
        .set_visible_child_name(if empty { "empty" } else { "list" });
}

/// The "more" menu of a saved host.
fn host_menu(host: &Host) -> gtk::MenuButton {
    let menu = gio::Menu::new();
    let id = host.id.to_variant();
    let item = |label: &str, action: &str| {
        let item = gio::MenuItem::new(Some(label), None);
        item.set_action_and_target_value(Some(action), Some(&id));
        item
    };
    if host.mac.is_some() {
        menu.append_item(&item("_Wake", "win.wake-host"));
    }
    menu.append_item(&item("_Edit…", "win.edit-host"));
    let danger = gio::Menu::new();
    danger.append_item(&item("_Forget", "win.forget-host"));
    menu.append_section(None, &danger);
    gtk::MenuButton::builder()
        .icon_name("view-more-symbolic")
        .tooltip_text("More")
        .valign(gtk::Align::Center)
        .menu_model(&menu)
        .css_classes(["flat"])
        .build()
}

/// The hosts on the network that are not saved, in list order.
fn unsaved_hosts(app: &App) -> Vec<FoundHost> {
    app.found
        .iter()
        .filter(|f| !app.store.hosts.iter().any(|h| same_host(f, &h.address)))
        .cloned()
        .collect()
}

fn found_address(found: &FoundHost) -> String {
    std::net::SocketAddr::new(found.address, found.port).to_string()
}

/// Whether a saved address names the host that answered on the network.
fn same_host(found: &FoundHost, address: &str) -> bool {
    store::parse_address(address).is_ok_and(|(host, port)| {
        port == found.port && host.parse::<std::net::IpAddr>().ok() == Some(found.address)
    })
}

/// Runs `work` on a thread and `done` on the main loop with its answer.
fn in_background<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
    done: impl FnOnce(T) + 'static,
) {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(work());
    });
    let mut done = Some(done);
    glib::timeout_add_local(Duration::from_millis(50), move || match rx.try_recv() {
        Ok(value) => {
            if let Some(done) = done.take() {
                done(value);
            }
            glib::ControlFlow::Break
        }
        Err(mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
        Err(mpsc::TryRecvError::Disconnected) => glib::ControlFlow::Break,
    });
}

/// Looks for hosts on the network.
fn scan(shared: &Shared) {
    {
        let mut app = shared.borrow_mut();
        if app.scanning {
            return;
        }
        app.scanning = true;
        show_scanning(&app);
    }
    let weak = Rc::downgrade(shared);
    in_background(
        || lumen_transport::mdns::browse(SCAN_FOR),
        move |result| {
            let Some(app) = weak.upgrade() else {
                return;
            };
            // The rows are only rebuilt when the answer changed: a rebuild closes a menu the
            // user has open.
            let changed = {
                let mut state = app.borrow_mut();
                state.scanning = false;
                show_scanning(&state);
                match result {
                    Ok(mut hosts) => {
                        hosts.sort_by(|a, b| a.name.cmp(&b.name));
                        hosts.dedup_by(|a, b| a.address == b.address && a.port == b.port);
                        let changed = hosts != state.found;
                        state.found = hosts;
                        changed
                    }
                    Err(e) => {
                        eprintln!("lumen: cannot look for hosts: {e}");
                        false
                    }
                }
            };
            if changed {
                render_hosts(&app);
            }
        },
    );
}

/// Shows whether a scan is running, without touching the rows.
fn show_scanning(app: &App) {
    app.scan_spinner.set_visible(app.scanning);
    app.welcome_scan.set_visible(app.scanning);
}

const WAKE_HINT: &str = "With the MAC address of the host's network card, Lumen can wake the \
                         host from sleep before connecting.";

fn address_hint() -> String {
    format!(
        "An IP address or a name, like 192.168.1.20 or desk.local. Add :port when lumen-host \
         does not use {}.",
        store::DEFAULT_PORT
    )
}

/// The dialog that adds a host, or edits the host with `id`.
fn host_dialog(shared: &Shared, id: Option<&str>) {
    let existing = id.and_then(|id| shared.borrow().store.host(id).cloned());
    let editing = existing.is_some();
    let name = adw::EntryRow::builder().title("Name").build();
    let address = adw::EntryRow::builder()
        .title("Address")
        .input_purpose(gtk::InputPurpose::Url)
        .build();
    let mac = adw::EntryRow::builder()
        .title("MAC Address (Optional)")
        .build();
    if let Some(host) = &existing {
        name.set_text(&host.name);
        address.set_text(&host.address);
        mac.set_text(host.mac.as_deref().unwrap_or_default());
    }
    let hint = gtk::Label::builder()
        .wrap(true)
        .xalign(0.0)
        .css_classes(["dim-label", "caption"])
        .label(address_hint())
        .build();
    let group = adw::PreferencesGroup::new();
    group.add(&name);
    group.add(&address);
    let wake_group = adw::PreferencesGroup::builder()
        .description(WAKE_HINT)
        .build();
    wake_group.add(&mac);
    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(18)
        .margin_top(12)
        .margin_bottom(24)
        .margin_start(12)
        .margin_end(12)
        .build();
    content.append(&group);
    content.append(&hint);
    content.append(&wake_group);

    let save_button = gtk::Button::builder()
        .label(if editing { "Save" } else { "Add" })
        .css_classes(["suggested-action"])
        .sensitive(false)
        .build();
    let cancel = gtk::Button::builder().label("Cancel").build();
    let header = adw::HeaderBar::builder()
        .show_start_title_buttons(false)
        .show_end_title_buttons(false)
        .build();
    header.pack_start(&cancel);
    header.pack_end(&save_button);
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&content));
    let dialog = adw::Dialog::builder()
        .title(if editing { "Edit Host" } else { "Add Host" })
        .content_width(440)
        .child(&toolbar)
        .default_widget(&save_button)
        .focus_widget(if editing { &address } else { &name })
        .build();

    // The address and the MAC are checked as they are typed.
    let validate = {
        let (address, mac, save_button, hint, wake_group) = (
            address.clone(),
            mac.clone(),
            save_button.clone(),
            hint.clone(),
            wake_group.clone(),
        );
        move || {
            let text = address.text();
            let address_ok = store::parse_address(&text);
            let mac_text = mac.text();
            let mac_ok = mac_text.trim().is_empty()
                || lumen_transport::MacAddr::parse(mac_text.trim()).is_ok();
            set_error(&address, !text.is_empty() && address_ok.is_err());
            set_error(&mac, !mac_ok);
            // Each problem is said next to its own field.
            match (text.is_empty(), &address_ok) {
                (false, Err(why)) => hint.set_label(why),
                _ => hint.set_label(&address_hint()),
            }
            wake_group.set_description(Some(if mac_ok {
                WAKE_HINT
            } else {
                "A MAC address is six pairs of hex digits, like 3c:7c:3f:12:ab:cd."
            }));
            save_button.set_sensitive(address_ok.is_ok() && mac_ok);
        }
    };
    validate();
    for row in [&address, &mac] {
        let validate = validate.clone();
        row.connect_changed(move |_| validate());
    }
    {
        let dialog = dialog.clone();
        cancel.connect_clicked(move |_| {
            dialog.close();
        });
    }
    let submit = {
        let weak = Rc::downgrade(shared);
        let dialog = dialog.clone();
        let existing_id = existing.map(|h| h.id);
        let (name, address, mac, save_button) = (
            name.clone(),
            address.clone(),
            mac.clone(),
            save_button.clone(),
        );
        move || {
            if !save_button.is_sensitive() {
                return;
            }
            let Some(app) = weak.upgrade() else {
                return;
            };
            let address_text = address.text().trim().to_owned();
            let mut name_text = name.text().trim().to_owned();
            if name_text.is_empty() {
                name_text = store::parse_address(&address_text)
                    .map_or_else(|_| address_text.clone(), |(host, _)| host);
            }
            let mac_text = mac.text().trim().to_owned();
            let mac_value = (!mac_text.is_empty()).then_some(mac_text);
            {
                let mut state = app.borrow_mut();
                match &existing_id {
                    Some(id) => {
                        if let Some(host) = state.store.hosts.iter_mut().find(|h| &h.id == id) {
                            host.name = name_text;
                            host.address = address_text;
                            host.mac = mac_value;
                        }
                    }
                    None => state.store.hosts.push(Host {
                        id: store::new_id(),
                        name: name_text,
                        address: address_text,
                        mac: mac_value,
                        last_used: None,
                    }),
                }
            }
            save(&app);
            render_hosts(&app);
            dialog.close();
        }
    };
    {
        let submit = submit.clone();
        save_button.connect_clicked(move |_| submit());
    }
    for row in [&name, &address, &mac] {
        let submit = submit.clone();
        row.connect_entry_activated(move |_| submit());
    }
    let window = shared.borrow().window.clone();
    dialog.present(Some(&window));
}

fn set_error(widget: &impl IsA<gtk::Widget>, error: bool) {
    if error {
        widget.add_css_class("error");
    } else {
        widget.remove_css_class("error");
    }
}

/// Asks, then forgets a host and its pairing.
fn forget(shared: &Shared, id: &str) {
    let Some(host) = shared.borrow().store.host(id).cloned() else {
        return;
    };
    let dialog = adw::AlertDialog::builder()
        .heading(format!("Forget {}?", host.name))
        .body(
            "Its address and this computer's pairing with it are removed. To connect again, \
             add it and pair with a new code.",
        )
        .close_response("cancel")
        .default_response("cancel")
        .build();
    dialog.add_responses(&[("cancel", "_Cancel"), ("forget", "_Forget")]);
    dialog.set_response_appearance("forget", adw::ResponseAppearance::Destructive);
    let weak = Rc::downgrade(shared);
    let id = id.to_owned();
    dialog.connect_response(None, move |_, response| {
        if response != "forget" {
            return;
        }
        let Some(app) = weak.upgrade() else {
            return;
        };
        app.borrow_mut().store.hosts.retain(|h| h.id != id);
        if let Some(dir) = store::identity_path(&id).parent() {
            let _ = std::fs::remove_dir_all(dir);
        }
        save(&app);
        render_hosts(&app);
        toast(&app, &format!("{} was forgotten", host.name));
    });
    let window = shared.borrow().window.clone();
    dialog.present(Some(&window));
}

/// Sends a wake-on-LAN packet to a saved host.
fn wake(shared: &Shared, id: &str) {
    let Some(host) = shared.borrow().store.host(id).cloned() else {
        return;
    };
    let Some(mac) = host.mac.clone() else {
        return;
    };
    let weak = Rc::downgrade(shared);
    in_background(
        move || -> Result<usize, String> {
            let mac = lumen_transport::MacAddr::parse(&mac)?;
            let addr = store::resolve(&host.address).unwrap_or_else(|_| {
                std::net::SocketAddr::from(([255, 255, 255, 255], store::DEFAULT_PORT))
            });
            lumen_transport::wol::send(&mac, &lumen_transport::broadcast_targets(addr))
                .map_err(|e| e.to_string())
        },
        move |result| {
            if let Some(app) = weak.upgrade() {
                match result {
                    Ok(_) => toast(&app, "Wake-up sent. The host may take a moment to start."),
                    Err(e) => toast(&app, &format!("Cannot send the wake-up: {e}")),
                }
            }
        },
    );
}

/// Saves a host found on the network and connects to it.
fn connect_found(shared: &Shared, index: &str) {
    let id = {
        let mut app = shared.borrow_mut();
        let unsaved = unsaved_hosts(&app);
        let Some(found) = index.parse::<usize>().ok().and_then(|i| unsaved.get(i)) else {
            return;
        };
        let id = store::new_id();
        app.store.hosts.push(Host {
            id: id.clone(),
            name: found.name.clone(),
            address: found_address(found),
            mac: None,
            last_used: None,
        });
        id
    };
    save(shared);
    render_hosts(shared);
    connect(shared, &id);
}

/// Opens the session page and starts a client for host `id`.
fn connect(shared: &Shared, id: &str) {
    let host = {
        let app = shared.borrow();
        if let Some(active) = &app.active {
            let name = active.name.clone();
            drop(app);
            toast(shared, &format!("Disconnect from {name} first"));
            return;
        }
        let Some(host) = app.store.host(id).cloned() else {
            return;
        };
        host
    };

    let status = adw::StatusPage::new();
    let buttons = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(12)
        .halign(gtk::Align::Center)
        .build();
    let log = gtk::Label::builder()
        .selectable(true)
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .xalign(0.0)
        .css_classes(["session-log"])
        .build();
    let details = gtk::Expander::builder()
        .label("Details")
        .child(&log)
        .visible(false)
        .margin_top(12)
        .build();
    let body = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .build();
    body.append(&buttons);
    body.append(&details);
    // The details line up under the buttons instead of the window's edge.
    status.set_child(Some(
        &adw::Clamp::builder().maximum_size(560).child(&body).build(),
    ));
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(&status));
    let page = adw::NavigationPage::builder()
        .title(&host.name)
        .tag(SESSION_TAG)
        .child(&toolbar)
        .build();

    let nav = {
        let mut app = shared.borrow_mut();
        if let Some(saved) = app.store.hosts.iter_mut().find(|h| h.id == host.id) {
            saved.last_used = Some(store::now());
        }
        app.active = Some(Active {
            host_id: host.id.clone(),
            name: host.name.clone(),
            address: host.address.clone(),
            session: None,
            status,
            buttons,
            details,
            log,
            pairing: None,
            streaming: false,
            streamed: false,
            retrying: false,
            cancelled: false,
            answered: false,
        });
        app.nav.clone()
    };
    save(shared);
    render_hosts(shared);
    nav.push(&page);
    start(shared, host);
}

/// Resolves the host's address and starts the client. Also the "Try Again" of a failure.
fn start(shared: &Shared, host: Host) {
    show_connecting(shared, None);
    let weak = Rc::downgrade(shared);
    let address = host.address.clone();
    in_background(
        move || store::resolve(&address),
        move |resolved| {
            let Some(app) = weak.upgrade() else {
                return;
            };
            // The user may have left the page while the name was looked up.
            if app
                .borrow()
                .active
                .as_ref()
                .is_none_or(|a| a.host_id != host.id)
            {
                return;
            }
            let addr = match resolved {
                Ok(addr) => addr,
                Err(why) => {
                    show_failure(
                        &app,
                        &Trouble {
                            title: "Host Not Found",
                            advice: why,
                        },
                        "",
                    );
                    return;
                }
            };
            let settings = app.borrow().store.settings.clone();
            let launch = Launch {
                program: session::client_program(),
                host: addr,
                title: format!("{} — Lumen", host.name),
                identity: store::identity_path(&host.id),
                events: session::events_path(),
                mac: host.mac.clone(),
                settings,
            };
            match Session::start(&launch) {
                Ok(session) => {
                    if let Some(active) = app.borrow_mut().active.as_mut() {
                        active.session = Some(session);
                    }
                    follow(&app);
                }
                Err(e) => show_failure(
                    &app,
                    &Trouble {
                        title: "Cannot Start the Stream",
                        advice: "The lumen-client program could not be started. Make sure it \
                                 is installed next to this app or on the PATH."
                            .to_owned(),
                    },
                    &e,
                ),
            }
        },
    );
}

/// Follows the client until it exits.
fn follow(shared: &Shared) {
    let weak: Weak<RefCell<App>> = Rc::downgrade(shared);
    glib::timeout_add_local(SESSION_POLL, move || {
        let Some(app) = weak.upgrade() else {
            return glib::ControlFlow::Break;
        };
        let (updates, exit) = {
            let mut state = app.borrow_mut();
            let Some(session) = state.active.as_mut().and_then(|a| a.session.as_mut()) else {
                return glib::ControlFlow::Break;
            };
            let updates = session.poll();
            let exit = session.exited();
            (updates, exit)
        };
        for update in updates {
            apply(&app, update);
        }
        match exit {
            Some(status) => {
                // The last events may have been written just before the exit.
                let late = app
                    .borrow_mut()
                    .active
                    .as_mut()
                    .and_then(|a| a.session.as_mut())
                    .map(Session::poll)
                    .unwrap_or_default();
                for update in late {
                    apply(&app, update);
                }
                finished(&app, status.success());
                glib::ControlFlow::Break
            }
            None => glib::ControlFlow::Continue,
        }
    });
}

fn apply(shared: &Shared, update: Update) {
    match update {
        Update::PairingRequested(fingerprint) => ask_code(shared, &fingerprint),
        Update::Established => {
            close_pairing(shared);
            show_streaming(shared);
        }
        Update::Retrying(detail) => {
            if let Some(active) = shared.borrow_mut().active.as_mut() {
                active.retrying = true;
                active.streaming = false;
            }
            show_connecting(shared, Some(detail.as_deref().unwrap_or_default()));
        }
        // The page already says whether this is a first try or a retry, and the end of the
        // session is read from the exit.
        Update::Connecting | Update::Closed | Update::Failed(_) => {}
    }
}

/// The client exited.
fn finished(shared: &Shared, success: bool) {
    close_pairing(shared);
    let (cancelled, streamed, name, failure, log, address) = {
        let app = shared.borrow();
        let Some(active) = app.active.as_ref() else {
            return;
        };
        let session = active.session.as_ref();
        (
            active.cancelled,
            active.streamed,
            active.name.clone(),
            // A clean exit ended the session; anything else failed, with the client's own error
            // or, when it died without one, its last line.
            match session {
                Some(session) if !success || session.failure().is_some() => Some(
                    session
                        .failure()
                        .map_or_else(|| session.last_line(), str::to_owned),
                ),
                _ => None,
            },
            session.map(Session::log).unwrap_or_default(),
            active.address.clone(),
        )
    };
    let Some(failure) = failure.filter(|_| !cancelled) else {
        let nav = shared.borrow().nav.clone();
        nav.pop();
        let text = if cancelled {
            "Pairing cancelled".to_owned()
        } else if streamed {
            format!("Session with {name} ended")
        } else {
            format!("{name} ended the session")
        };
        toast(shared, &text);
        return;
    };
    let trouble = session::explain(&failure, &address);
    let details = if failure.is_empty() {
        log
    } else {
        format!("{failure}\n\n{log}")
    };
    show_failure(shared, &trouble, &details);
}

/// Stops the session, if any. Runs when its page is popped or the window closes.
fn end_session(shared: &Shared) {
    let active = shared.borrow_mut().active.take();
    if let Some(mut active) = active {
        if let Some(dialog) = active.pairing.take() {
            dialog.force_close();
        }
        // Dropping the session stops the client.
        drop(active.session.take());
    }
}

/// A button of the session page: label, style class, what it does.
type Action = (&'static str, &'static str, Rc<dyn Fn()>);

/// Replaces the session page's buttons.
fn set_buttons(active: &Active, buttons: Vec<Action>) {
    while let Some(child) = active.buttons.first_child() {
        active.buttons.remove(&child);
    }
    for (label, style, run) in buttons {
        let button = gtk::Button::builder()
            .label(label)
            .css_classes(["pill"])
            .build();
        if !style.is_empty() {
            button.add_css_class(style);
        }
        button.connect_clicked(move |_| run());
        active.buttons.append(&button);
    }
}

fn leave_button(shared: &Shared) -> Rc<dyn Fn()> {
    let weak = Rc::downgrade(shared);
    Rc::new(move || {
        if let Some(app) = weak.upgrade() {
            let nav = app.borrow().nav.clone();
            nav.pop();
        }
    })
}

/// The waiting state. `retry` is `None` for the first try, or the reason the last try failed.
fn show_connecting(shared: &Shared, retry: Option<&str>) {
    let app = shared.borrow();
    let Some(active) = app.active.as_ref() else {
        return;
    };
    active.status.set_icon_name(None);
    active
        .status
        .set_paintable(Some(&adw::SpinnerPaintable::new(Some(&active.status))));
    match retry {
        None => {
            active
                .status
                .set_title(&format!("Connecting to {}", active.name));
            active.status.set_description(Some(&active.address));
        }
        Some(_) if active.streamed => {
            active
                .status
                .set_title(&format!("Reconnecting to {}", active.name));
            active
                .status
                .set_description(Some("The connection dropped. Lumen keeps trying."));
        }
        Some(reason) => {
            let trouble = session::explain(reason, &active.address);
            active
                .status
                .set_title(&format!("Waiting for {}", active.name));
            active
                .status
                .set_description(Some(&format!("{} Lumen keeps trying.", trouble.advice)));
        }
    }
    active.details.set_visible(false);
    let label = if retry.is_some() { "Stop" } else { "Cancel" };
    set_buttons(active, vec![(label, "", leave_button(shared))]);
}

fn show_streaming(shared: &Shared) {
    let app = shared.borrow();
    let Some(active) = app.active.as_ref() else {
        return;
    };
    active.status.set_paintable(None::<&gtk::gdk::Paintable>);
    active.status.set_icon_name(Some("video-display-symbolic"));
    active
        .status
        .set_title(&format!("Streaming {}", active.name));
    active.status.set_description(Some(
        "The host's screen is in its own window. Press F11 there for fullscreen, and close \
         that window to end the session.",
    ));
    active.details.set_visible(false);
    set_buttons(
        active,
        vec![("Disconnect", "destructive-action", leave_button(shared))],
    );
    drop(app);
    if let Some(active) = shared.borrow_mut().active.as_mut() {
        active.streaming = true;
        active.streamed = true;
        active.retrying = false;
        active.answered = false;
    }
}

fn show_failure(shared: &Shared, trouble: &Trouble, details: &str) {
    let (host_id, retry_host) = {
        let app = shared.borrow();
        let Some(active) = app.active.as_ref() else {
            return;
        };
        (
            active.host_id.clone(),
            app.store.host(&active.host_id).cloned(),
        )
    };
    let retry: Rc<dyn Fn()> = {
        let weak = Rc::downgrade(shared);
        Rc::new(move || {
            let Some(app) = weak.upgrade() else {
                return;
            };
            let Some(host) = retry_host.clone() else {
                return;
            };
            {
                let mut state = app.borrow_mut();
                let Some(active) = state.active.as_mut() else {
                    return;
                };
                if active.host_id != host_id {
                    return;
                }
                drop(active.session.take());
                active.cancelled = false;
                active.streaming = false;
                active.retrying = false;
                active.answered = false;
            }
            start(&app, host);
        })
    };
    let app = shared.borrow();
    let Some(active) = app.active.as_ref() else {
        return;
    };
    active.status.set_paintable(None::<&gtk::gdk::Paintable>);
    active.status.set_icon_name(Some(match trouble.title {
        "Host Not Reachable" | "Host Not Found" | "Connection Lost" => "network-offline-symbolic",
        "Pairing Failed" | "Host Identity Changed" => "channel-insecure-symbolic",
        _ => "dialog-warning-symbolic",
    }));
    active.status.set_title(trouble.title);
    active.status.set_description(Some(&trouble.advice));
    active.log.set_label(details.trim());
    active.details.set_visible(!details.trim().is_empty());
    active.details.set_expanded(false);
    set_buttons(
        active,
        vec![
            ("Try Again", "suggested-action", retry),
            ("Back to Hosts", "", leave_button(shared)),
        ],
    );
}

/// Asks for the pairing code the host shows.
fn ask_code(shared: &Shared, fingerprint: &str) {
    let (name, address, window, again) = {
        let app = shared.borrow();
        let Some(active) = app.active.as_ref() else {
            return;
        };
        (
            active.name.clone(),
            active.address.clone(),
            app.window.clone(),
            active.answered,
        )
    };
    let known = {
        let app = shared.borrow();
        app.found
            .iter()
            .find(|f| same_host(f, &address))
            .and_then(|f| f.fingerprint.clone())
    };
    let intro = if again {
        "That code did not match. The host shows a new code now: type it here.".to_owned()
    } else {
        format!(
            "This computer has not paired with {name} yet. Type the 6-digit code that \
             lumen-host shows on that computer (in its log or terminal)."
        )
    };
    let mut body = format!(
        "{intro}\n\nHost fingerprint: {}",
        session::group_fingerprint(fingerprint)
    );
    match known {
        Some(full) if session::fingerprint_matches(fingerprint, &full) => {
            body.push_str("\nIt matches the host found on this network.");
        }
        Some(_) => body.push_str(
            "\nWarning: it does not match the host found on this network. Do not pair unless \
             you are sure this is your host.",
        ),
        None => {}
    }
    let entry = gtk::Entry::builder()
        .max_length(6)
        .input_purpose(gtk::InputPurpose::Digits)
        .placeholder_text("000000")
        .xalign(0.5)
        .activates_default(true)
        .css_classes(["pairing-code"])
        .build();
    let dialog = adw::AlertDialog::builder()
        .heading(format!("Pair with {name}"))
        .body(body)
        .extra_child(&entry)
        .close_response("cancel")
        .default_response("pair")
        .build();
    dialog.add_responses(&[("cancel", "_Cancel"), ("pair", "_Pair")]);
    dialog.set_response_appearance("pair", adw::ResponseAppearance::Suggested);
    dialog.set_response_enabled("pair", false);
    {
        let dialog = dialog.clone();
        entry.connect_changed(move |entry| {
            let text = entry.text();
            let digits: String = text.chars().filter(char::is_ascii_digit).collect();
            if digits != text.as_str() {
                // Only digits: a pasted "123 456" keeps its digits.
                entry.set_text(&digits);
                return;
            }
            dialog.set_response_enabled("pair", digits.len() == 6);
        });
    }
    let weak = Rc::downgrade(shared);
    let code_entry = entry.clone();
    dialog.connect_response(None, move |_, response| {
        let Some(app) = weak.upgrade() else {
            return;
        };
        let mut state = app.borrow_mut();
        let Some(active) = state.active.as_mut() else {
            return;
        };
        active.pairing = None;
        let Some(session) = active.session.as_mut() else {
            return;
        };
        if response == "pair" {
            session.answer(&code_entry.text());
            active.answered = true;
            active.status.set_title("Checking the Code");
        } else {
            // Stopping the client, not just refusing: a client that reconnects would ask again.
            active.cancelled = true;
            session.stop();
        }
    });
    if let Some(active) = shared.borrow_mut().active.as_mut() {
        active.status.set_title(&format!("Pairing with {name}"));
        active
            .status
            .set_description(Some("Enter the code the host shows."));
        active.pairing = Some(dialog.clone());
    }
    dialog.present(Some(&window));
    entry.grab_focus();
}

fn close_pairing(shared: &Shared) {
    let dialog = shared
        .borrow_mut()
        .active
        .as_mut()
        .and_then(|a| a.pairing.take());
    if let Some(dialog) = dialog {
        dialog.force_close();
    }
}

/// The preferences dialog. Changes are saved at once and apply to the next session.
fn preferences(shared: &Shared) {
    let settings = shared.borrow().store.settings.clone();
    let page = adw::PreferencesPage::new();

    let session_group = adw::PreferencesGroup::builder()
        .title("Session")
        .description("Changes apply to the next session.")
        .build();
    let input = adw::SwitchRow::builder()
        .title("Send Keyboard, Mouse and Controllers")
        .subtitle("Input goes to the host while the stream window has focus")
        .active(settings.input)
        .build();
    let audio = adw::SwitchRow::builder()
        .title("Play Sound")
        .subtitle("The host must run with --with-audio")
        .active(settings.audio)
        .build();
    let reconnect = adw::SwitchRow::builder()
        .title("Reconnect Automatically")
        .subtitle("Come back when the connection drops")
        .active(settings.reconnect)
        .build();
    session_group.add(&input);
    session_group.add(&audio);
    session_group.add(&reconnect);

    let performance = adw::PreferencesGroup::builder()
        .title("Performance")
        .build();
    let low_power = adw::SwitchRow::builder()
        .title("Low Power Mode")
        .subtitle("Decode on one thread and stay at 1080p or below, for laptops on battery")
        .active(settings.low_power)
        .build();
    performance.add(&low_power);

    let window_group = adw::PreferencesGroup::builder()
        .title("Stream Window")
        .description(
            "Press F11 in the stream window to switch fullscreen on and off. Automatic uses \
             X11 when the desktop draws no title bar for Wayland windows.",
        )
        .build();
    let backend = adw::ComboRow::builder()
        .title("Window System")
        .model(&gtk::StringList::new(&["Automatic", "Wayland", "X11"]))
        .selected(match settings.backend {
            Backend::Auto => 0,
            Backend::Wayland => 1,
            Backend::X11 => 2,
        })
        .build();
    window_group.add(&backend);

    page.add(&session_group);
    page.add(&performance);
    page.add(&window_group);

    let update = {
        let weak = Rc::downgrade(shared);
        let (input, audio, reconnect, low_power, backend) = (
            input.clone(),
            audio.clone(),
            reconnect.clone(),
            low_power.clone(),
            backend.clone(),
        );
        move || {
            let Some(app) = weak.upgrade() else {
                return;
            };
            app.borrow_mut().store.settings = store::Settings {
                input: input.is_active(),
                audio: audio.is_active(),
                reconnect: reconnect.is_active(),
                low_power: low_power.is_active(),
                backend: match backend.selected() {
                    1 => Backend::Wayland,
                    2 => Backend::X11,
                    _ => Backend::Auto,
                },
            };
            save(&app);
        }
    };
    for row in [&input, &audio, &reconnect, &low_power] {
        let update = update.clone();
        row.connect_active_notify(move |_| update());
    }
    backend.connect_selected_notify(move |_| update());

    let dialog = adw::PreferencesDialog::new();
    dialog.add(&page);
    let window = shared.borrow().window.clone();
    dialog.present(Some(&window));
}

fn about(shared: &Shared) {
    let dialog = adw::AboutDialog::builder()
        .application_name("Lumen")
        .application_icon(APP_ID)
        .version(env!("CARGO_PKG_VERSION"))
        .developer_name("The Lumen developers")
        .comments("Low-latency remote desktop for Linux.")
        .license_type(gtk::License::MitX11)
        .build();
    dialog.add_acknowledgement_section(
        Some("Ideas and data from"),
        &["punktfunk https://git.unom.io/unom/punktfunk"],
    );
    let window = shared.borrow().window.clone();
    dialog.present(Some(&window));
}

fn shortcuts(shared: &Shared) {
    // GTK draws each accelerator in the user's keyboard language.
    let rows = [
        ("Add a host", "<Control>n"),
        ("Look for hosts", "F5"),
        ("Preferences", "<Control>comma"),
        ("Keyboard shortcuts", "<Control>question"),
        ("Close the window", "<Control>w"),
        ("Quit", "<Control>q"),
        ("Fullscreen (in the stream window)", "F11"),
    ];
    let group = adw::PreferencesGroup::new();
    for (what, accelerator) in rows {
        let row = adw::ActionRow::builder()
            .title(what)
            .use_markup(false)
            .build();
        row.add_suffix(
            &gtk::ShortcutLabel::builder()
                .accelerator(accelerator)
                .valign(gtk::Align::Center)
                .build(),
        );
        group.add(&row);
    }
    let page = adw::PreferencesPage::new();
    page.add(&group);
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(&page));
    let dialog = adw::Dialog::builder()
        .title("Keyboard Shortcuts")
        .content_width(420)
        .content_height(420)
        .child(&toolbar)
        .build();
    let window = shared.borrow().window.clone();
    dialog.present(Some(&window));
}
