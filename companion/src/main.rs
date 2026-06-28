//! Photon Companion — a small GNOME (GTK4 + libadwaita) desktop client for a
//! Photon photo library: sign in to a server, then browse your timeline in a
//! native, HIG-compliant window. Networking runs on a tokio runtime; results are
//! marshalled back to the GTK main thread over `async-channel`.

mod api;
mod runtime;

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gdk, gio, glib};

use api::Session;
use runtime::runtime;

const APP_ID: &str = "app.photon.Companion";
const DEFAULT_SERVER: &str = "http://localhost:3000";

fn main() -> glib::ExitCode {
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_activate(build_ui);
    app.run()
}

fn build_ui(app: &adw::Application) {
    let session: Rc<RefCell<Option<Session>>> = Rc::new(RefCell::new(None));

    // ---- Library page: a scrollable grid of thumbnails. ----
    let grid = gtk::FlowBox::builder()
        .valign(gtk::Align::Start)
        .selection_mode(gtk::SelectionMode::None)
        .homogeneous(true)
        .column_spacing(8)
        .row_spacing(8)
        .max_children_per_line(8)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();
    let library_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&grid)
        .build();

    // ---- Login page: a clamped form. ----
    let server_entry = gtk::Entry::builder().text(DEFAULT_SERVER).build();
    let ident_entry = gtk::Entry::builder().placeholder_text("email or username").build();
    let pw_entry = gtk::PasswordEntry::builder().show_peek_icon(true).build();
    let status = gtk::Label::builder().css_classes(["error"]).wrap(true).build();
    let sign_in = gtk::Button::builder()
        .label("Sign in")
        .css_classes(["suggested-action", "pill"])
        .halign(gtk::Align::Center)
        .build();

    let form = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .valign(gtk::Align::Center)
        .build();
    let title = gtk::Label::builder().label("Photon").css_classes(["title-1"]).build();
    let subtitle = gtk::Label::builder()
        .label("Sign in to your photo library")
        .css_classes(["dim-label"])
        .margin_bottom(8)
        .build();
    let group = adw::PreferencesGroup::new();
    let server_row = adw::ActionRow::builder().title("Server").build();
    server_row.add_suffix(&server_entry);
    let ident_row = adw::ActionRow::builder().title("Account").build();
    ident_row.add_suffix(&ident_entry);
    let pw_row = adw::ActionRow::builder().title("Password").build();
    pw_row.add_suffix(&pw_entry);
    group.add(&server_row);
    group.add(&ident_row);
    group.add(&pw_row);
    form.append(&title);
    form.append(&subtitle);
    form.append(&group);
    form.append(&sign_in);
    form.append(&status);
    let login_clamp = adw::Clamp::builder()
        .maximum_size(420)
        .margin_top(24)
        .margin_bottom(24)
        .margin_start(12)
        .margin_end(12)
        .child(&form)
        .build();

    // ---- Stack swaps login ↔ library. ----
    let stack = gtk::Stack::new();
    stack.add_named(&login_clamp, Some("login"));
    stack.add_named(&library_scroll, Some("library"));
    stack.set_visible_child_name("login");

    // ---- Header bar + primary menu. ----
    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&adw::WindowTitle::new("Photon Companion", "")));

    let refresh_btn = gtk::Button::from_icon_name("view-refresh-symbolic");
    refresh_btn.set_tooltip_text(Some("Refresh"));
    refresh_btn.set_visible(false);
    let upload_btn = gtk::Button::from_icon_name("list-add-symbolic");
    upload_btn.set_tooltip_text(Some("Upload photos"));
    upload_btn.set_visible(false);
    header.pack_start(&upload_btn);
    header.pack_start(&refresh_btn);

    let menu = gio::Menu::new();
    menu.append(Some("Sign out"), Some("win.sign-out"));
    menu.append(Some("About Photon Companion"), Some("win.about"));
    let menu_btn = gtk::MenuButton::builder()
        .icon_name("open-menu-symbolic")
        .menu_model(&menu)
        .build();
    header.pack_end(&menu_btn);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&stack));

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Photon Companion")
        .default_width(960)
        .default_height(680)
        .content(&toolbar)
        .build();

    // ---- Window actions (menu). ----
    let about = gio::ActionEntry::builder("about")
        .activate(glib::clone!(
            #[weak]
            window,
            move |_: &adw::ApplicationWindow, _, _| show_about(&window)
        ))
        .build();
    let sign_out = gio::ActionEntry::builder("sign-out")
        .activate(glib::clone!(
            #[weak]
            stack,
            #[weak]
            refresh_btn,
            #[weak]
            upload_btn,
            #[strong]
            session,
            move |_: &adw::ApplicationWindow, _, _| {
                *session.borrow_mut() = None;
                stack.set_visible_child_name("login");
                refresh_btn.set_visible(false);
                upload_btn.set_visible(false);
            }
        ))
        .build();
    window.add_action_entries([about, sign_out]);

    // ---- Sign-in handler. ----
    let do_login = glib::clone!(
        #[weak]
        server_entry,
        #[weak]
        ident_entry,
        #[weak]
        pw_entry,
        #[weak]
        status,
        #[weak]
        sign_in,
        #[weak]
        stack,
        #[weak]
        grid,
        #[weak]
        refresh_btn,
        #[weak]
        upload_btn,
        #[strong]
        session,
        move || {
            let base = server_entry.text().to_string();
            let ident = ident_entry.text().to_string();
            let pw = pw_entry.text().to_string();
            if ident.is_empty() || pw.is_empty() {
                status.set_text("Enter your account and password.");
                return;
            }
            status.set_text("");
            sign_in.set_sensitive(false);
            let (tx, rx) = async_channel::bounded(1);
            runtime().spawn(async move {
                let _ = tx.send(api::login(&base, &ident, &pw).await).await;
            });
            glib::spawn_future_local(glib::clone!(
                #[weak]
                status,
                #[weak]
                sign_in,
                #[weak]
                stack,
                #[weak]
                grid,
                #[weak]
                refresh_btn,
                #[weak]
                upload_btn,
                #[strong]
                session,
                async move {
                    match rx.recv().await {
                        Ok(Ok(s)) => {
                            *session.borrow_mut() = Some(s);
                            refresh_btn.set_visible(true);
                            upload_btn.set_visible(true);
                            stack.set_visible_child_name("library");
                            load_timeline(&session, &grid);
                        }
                        Ok(Err(e)) => status.set_text(&e),
                        Err(_) => status.set_text("internal error"),
                    }
                    sign_in.set_sensitive(true);
                }
            ));
        }
    );
    sign_in.connect_clicked(glib::clone!(
        #[strong]
        do_login,
        move |_| do_login()
    ));
    pw_entry.connect_activate(glib::clone!(
        #[strong]
        do_login,
        move |_| do_login()
    ));

    refresh_btn.connect_clicked(glib::clone!(
        #[weak]
        grid,
        #[strong]
        session,
        move |_| load_timeline(&session, &grid)
    ));
    upload_btn.connect_clicked(glib::clone!(
        #[weak]
        window,
        #[weak]
        grid,
        #[strong]
        session,
        move |_| pick_and_upload(&window, &session, &grid)
    ));

    window.present();
}

/// Clear and repopulate the grid from the session's timeline (async).
fn load_timeline(session: &Rc<RefCell<Option<Session>>>, grid: &gtk::FlowBox) {
    let Some(s) = session.borrow().clone() else { return };
    while let Some(child) = grid.first_child() {
        grid.remove(&child);
    }
    let (tx, rx) = async_channel::bounded(1);
    runtime().spawn(async move {
        let _ = tx.send(s.timeline().await).await;
    });
    glib::spawn_future_local(glib::clone!(
        #[weak]
        grid,
        #[strong]
        session,
        async move {
            let photos = match rx.recv().await {
                Ok(Ok(p)) => p,
                _ => return,
            };
            for photo in photos {
                let pic = gtk::Picture::builder()
                    .content_fit(gtk::ContentFit::Cover)
                    .width_request(170)
                    .height_request(170)
                    .build();
                pic.add_css_class("card");
                pic.set_tooltip_text(Some(&photo.id));
                grid.insert(&pic, -1);
                // Lazily fetch each thumbnail.
                if let (Some(url), Some(s)) = (photo.thumb_url.clone(), session.borrow().clone()) {
                    let (btx, brx) = async_channel::bounded(1);
                    runtime().spawn(async move {
                        let _ = btx.send(s.fetch_bytes(&url).await).await;
                    });
                    glib::spawn_future_local(glib::clone!(
                        #[weak]
                        pic,
                        async move {
                            if let Ok(Ok(bytes)) = brx.recv().await {
                                if let Ok(tex) = gdk::Texture::from_bytes(&glib::Bytes::from(&bytes)) {
                                    pic.set_paintable(Some(&tex));
                                }
                            }
                        }
                    ));
                }
            }
        }
    ));
}

/// Open a native file chooser and upload the picked images to the server.
fn pick_and_upload(
    window: &adw::ApplicationWindow,
    session: &Rc<RefCell<Option<Session>>>,
    grid: &gtk::FlowBox,
) {
    let Some(s) = session.borrow().clone() else { return };
    let dialog = gtk::FileDialog::builder().title("Upload photos").build();
    dialog.open_multiple(
        Some(window),
        gio::Cancellable::NONE,
        glib::clone!(
            #[weak]
            grid,
            #[strong]
            session,
            move |res| {
                let Ok(files) = res else { return };
                let mut payload: Vec<(String, Vec<u8>)> = Vec::new();
                for i in 0..files.n_items() {
                    if let Some(f) = files.item(i).and_downcast::<gio::File>() {
                        let name = f
                            .basename()
                            .map(|p| p.to_string_lossy().to_string())
                            .unwrap_or_default();
                        if let Ok((bytes, _)) = f.load_contents(gio::Cancellable::NONE) {
                            payload.push((name, bytes.to_vec()));
                        }
                    }
                }
                if payload.is_empty() {
                    return;
                }
                let s = s.clone();
                let (tx, rx) = async_channel::bounded(1);
                runtime().spawn(async move {
                    let _ = tx.send(s.upload(payload).await).await;
                });
                glib::spawn_future_local(glib::clone!(
                    #[weak]
                    grid,
                    #[strong]
                    session,
                    async move {
                        let _ = rx.recv().await;
                        load_timeline(&session, &grid);
                    }
                ));
            }
        ),
    );
}

fn show_about(window: &adw::ApplicationWindow) {
    let about = adw::AboutDialog::builder()
        .application_name("Photon Companion")
        .application_icon(APP_ID)
        .version(env!("CARGO_PKG_VERSION"))
        .developer_name("Photon")
        .comments("A GNOME desktop client for your Photon photo library.")
        .license_type(gtk::License::MitX11)
        .build();
    about.present(Some(window));
}
