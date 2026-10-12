//! dfr-configurator: the native GTK4/libadwaita configurator for the
//! tiny-dfr Touch Bar deck. It edits whatever configuration file it is
//! pointed at (default `/etc/tiny-dfr/config.toml`, per the README)
//! through the shared `dfr-config` model, previews the bar with the
//! real `dfr-render` draw path, and saves through the canonical writer
//! so the daemon's inotify reload applies changes live. Invalid states
//! are rejected in the UI before Save can ever write them.

mod capture;
mod knobs;
mod layers;
mod model;
mod preview;

use gtk4::{glib, prelude::*};
use layers::{Editor, LayersPage};
use libadwaita as adw;
use libadwaita::prelude::*;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

const APP_ID: &str = "org.dfrdeck.Configurator";

fn main() -> anyhow::Result<()> {
    let (base_path, user_path) = parse_args();
    let doc = model::Document::open(&base_path, &user_path)?;

    adw::init().expect("libadwaita initializes");
    let app = adw::Application::builder().application_id(APP_ID).build();
    let document = std::cell::RefCell::new(Some(doc));
    app.connect_activate(move |app| {
        if let Some(doc) = document.borrow_mut().take() {
            build_window(app, doc);
        }
    });
    // The app owns GTK argument parsing; ours are already consumed.
    app.run_with_args::<String>(&[]);
    Ok(())
}

fn parse_args() -> (PathBuf, PathBuf) {
    let mut base = PathBuf::from("/usr/share/tiny-dfr/config.toml");
    let mut user = PathBuf::from("/etc/tiny-dfr/config.toml");
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" | "-c" => {
                if let Some(path) = args.next() {
                    user = PathBuf::from(path);
                }
            }
            "--base" | "-b" => {
                if let Some(path) = args.next() {
                    base = PathBuf::from(path);
                }
            }
            "--help" | "-h" => {
                eprintln!(
                    "usage: dfr-configurator [--config PATH] [--base PATH]\n\
                     --config edits that file (default /etc/tiny-dfr/config.toml)\n\
                     --base loads that base configuration (default /usr/share/tiny-dfr/config.toml)"
                );
                std::process::exit(0);
            }
            other => {
                eprintln!("unknown option {other}; see --help");
                std::process::exit(2);
            }
        }
    }
    (base, user)
}

fn build_window(app: &adw::Application, doc: model::Document) {
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .default_width(1280)
        .default_height(800)
        .title("Touch Bar Configurator")
        .build();

    // ---- shell: header with page switcher, status bar, deck/knobs ---
    let stack = adw::ViewStack::new();
    let switcher = adw::ViewSwitcher::new();
    switcher.set_stack(Some(&stack));
    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&switcher));

    let save = gtk4::Button::with_label("Save");
    save.add_css_class("suggested-action");
    header.pack_end(&save);

    let status = gtk4::Label::new(None);
    status.set_wrap(true);
    status.set_xalign(0.0);
    status.set_margin_start(6);
    status.set_margin_end(6);
    status.set_margin_top(2);
    status.set_margin_bottom(2);

    let editor = Editor::new(doc, preview::Preview::new("primary"));
    let layers_page = LayersPage::build(&editor);
    let knobs_page = knobs::KnobsPage::build(&editor);

    stack.add_titled(&layers_page.root, Some("deck"), "Deck");
    stack.add_titled(&knobs_page.root, Some("knobs"), "Knobs");
    stack
        .page(&layers_page.root)
        .set_icon_name(Some("view-grid-symbolic"));
    stack
        .page(&knobs_page.root)
        .set_icon_name(Some("preferences-system-symbolic"));

    let body = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    body.set_margin_top(6);
    body.set_margin_bottom(6);
    body.append(&status);
    body.append(&stack);
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&body));
    window.set_content(Some(&toolbar));

    // ---- the after-edit pipeline ---------------------------------
    // Every mutation notifies: revalidate through the shared
    // parse/validate path, reflect validity in the status bar and the
    // Save button, and re-render the preview debounced. Shape changes
    // additionally rebuild the deck page (selection preserved).
    let weak_status = status.downgrade();
    let weak_save = save.downgrade();
    let weak_editor = Rc::downgrade(&editor);
    editor.set_after_edit(Box::new(move |editor: Rc<Editor>| {
        let diagnostics = editor.doc.borrow().validate();
        let (Some(status), Some(save)) = (weak_status.upgrade(), weak_save.upgrade()) else {
            return;
        };
        if diagnostics.is_empty() {
            save.set_sensitive(
                editor.doc.borrow().is_dirty() || !editor.doc.borrow().user_path().exists(),
            );
            status.set_markup(
                "<span weight='bold'>Configuration is valid</span> — Save installs it \
                 atomically and the daemon applies it live on its inotify reload.",
            );
        } else {
            save.set_sensitive(false);
            status.set_markup(&format!(
                "<span weight='bold' foreground='#e01b24'>Invalid — Save stays disabled \
                 until the configuration parses clean:</span>\n{}",
                glib::markup_escape_text(&diagnostics.join("\n"))
            ));
        }
        if let Some(error) = editor.save_feedback.borrow().error.as_ref() {
            status.set_text(&format!("Save failed — {error}. Retry with Save."));
        }
        // Debounce the preview: one render per burst of edits.
        let weak_editor = weak_editor.clone();
        editor.preview.invalidate();
        glib::timeout_add_local_once(Duration::from_millis(400), move || {
            if let Some(editor) = weak_editor.upgrade() {
                editor.preview.render(&editor.doc.borrow());
            }
        });
    }));

    let layers_root = Rc::new(std::cell::RefCell::new(layers_page.root.clone()));
    let knobs_root = Rc::new(std::cell::RefCell::new(knobs_page.root.clone()));
    let weak_stack = stack.downgrade();
    editor.set_on_structure(Box::new(move |editor: Rc<Editor>| {
        // Rebuild the deck page under its stack parent.
        let Some(stack) = weak_stack.upgrade() else {
            return;
        };
        let visible = stack.visible_child_name();
        if let Some(parent) = editor.preview.area.parent().and_downcast::<gtk4::Box>() {
            parent.remove(&editor.preview.area);
        }
        let replacement = LayersPage::build(&editor);
        stack.remove(&*layers_root.borrow());
        stack.add_titled(&replacement.root, Some("deck"), "Deck");
        stack
            .page(&replacement.root)
            .set_icon_name(Some("view-grid-symbolic"));
        *layers_root.borrow_mut() = replacement.root.clone();
        stack.remove(&*knobs_root.borrow());
        let knobs = knobs::KnobsPage::build(&editor);
        stack.add_titled(&knobs.root, Some("knobs"), "Knobs");
        *knobs_root.borrow_mut() = knobs.root;
        if let Some(visible) = visible {
            stack.set_visible_child_name(&visible);
        }
        replacement.refresh(&editor);
    }));

    // The Knobs page's "Open Deck page" pointer.
    let show_deck = gio::SimpleAction::new("show-deck", None);
    let weak_stack = stack.downgrade();
    show_deck.connect_activate(move |_, _| {
        if let Some(stack) = weak_stack.upgrade() {
            stack.set_visible_child_name("deck");
        }
    });
    window.add_action(&show_deck);

    save.connect_clicked(glib::clone!(
        #[strong]
        editor,
        #[strong]
        status,
        move |_| {
            let outcome = editor
                .doc
                .borrow_mut()
                .save()
                .map_err(|err| format!("{err:#}"));
            editor.save_feedback.borrow_mut().record(outcome.clone());
            editor.run_pipeline();
            match outcome {
                Ok(()) => {
                    status.set_markup(&format!(
                        "<span weight='bold' foreground='#26a269'>Saved {}</span> — the daemon \
                     reloads it on the inotify rename.",
                        glib::markup_escape_text(
                            &editor.doc.borrow().user_path().display().to_string()
                        )
                    ));
                }
                Err(err) => {
                    status.set_markup(&format!(
                        "<span weight='bold' foreground='#e01b24'>Save failed:</span> {}",
                        glib::markup_escape_text(&err)
                    ));
                }
            }
        }
    ));

    // Close guard: unsaved changes ask first.
    window.connect_close_request(glib::clone!(
        #[strong]
        editor,
        #[strong]
        window,
        move |_| {
            if !editor.doc.borrow().is_dirty() {
                return glib::Propagation::Proceed;
            }
            let dialog = adw::AlertDialog::builder()
                .heading("Unsaved changes")
                .body("The deck has changes that were not saved to the configuration file.")
                .build();
            dialog.add_response("discard", "Discard");
            dialog.add_response("cancel", "Cancel");
            dialog.set_response_appearance("discard", adw::ResponseAppearance::Destructive);
            dialog.choose(
                Some(&window),
                None::<&gio::Cancellable>,
                glib::clone!(
                    #[strong]
                    window,
                    move |response| {
                        if response == "discard" {
                            window.close();
                        }
                    }
                ),
            );
            glib::Propagation::Stop
        }
    ));

    // Initial state: first layer selected, preview rendered, validated.
    layers_page.refresh(&editor);
    {
        let mut doc = editor.doc.borrow_mut();
        let name = doc
            .layers()
            .first()
            .map(|layer| layer.name.clone())
            .unwrap_or_else(|| "primary".into());
        editor.preview.set_layer(&name);
        editor.preview.render(&doc);
    }
    editor.run_pipeline();

    window.present();
}

use gtk4::gio;
