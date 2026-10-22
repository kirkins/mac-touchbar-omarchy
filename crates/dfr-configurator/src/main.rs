//! dfr-configurator: the native GTK4/libadwaita configurator for the
//! tiny-dfr Touch Bar deck. It edits whatever configuration file it is
//! pointed at (default a user-owned draft, per INSTALL.md)
//! through the shared `dfr-config` model, previews the bar with the
//! real `dfr-render` draw path, and saves through the canonical writer.
//! User drafts are applied separately to the daemon's watched overlay. Invalid states
//! are rejected in the UI before Save can ever write them.

mod capture;
mod knobs;
mod layers;
mod model;
mod preview;
mod startup;
mod ui;

use gtk4::{glib, prelude::*};
use layers::{Editor, LayersPage};
use libadwaita as adw;
use libadwaita::prelude::*;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

const APP_ID: &str = "org.dfrdeck.Configurator";

/// Where the full-deck example is installed: the deck's data directory under
/// either install prefix, or the location older installs used.
const EXAMPLE_DECKS: &[&str] = &[
    "/usr/share/tiny-dfr-deck/multibar.toml",
    "/usr/local/share/tiny-dfr-deck/multibar.toml",
    "/usr/share/tiny-dfr/multibar.toml",
];

fn example_deck() -> PathBuf {
    EXAMPLE_DECKS
        .iter()
        .map(PathBuf::from)
        .find(|path| path.is_file())
        .unwrap_or_else(|| PathBuf::from(EXAMPLE_DECKS[0]))
}

fn main() -> anyhow::Result<()> {
    let options = startup::parse(
        std::env::args().skip(1),
        std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
        std::env::var_os("HOME").map(PathBuf::from),
    );
    if matches!(options, Ok(None)) {
        println!("usage: dfr-configurator [--config PATH] [--base PATH]\n\
                  --config edits that file directly; by default Settings saves a user draft\n\
                  at $XDG_CONFIG_HOME/dfr-deck/config.toml (or $HOME/.config/dfr-deck/config.toml)\n\
                  --base defaults to /usr/share/tiny-dfr/config.toml\n\
                  Apply saved drafts with the administrator steps in INSTALL.md (https://github.com/kirkins/mac-touchbar-omarchy)");
        return Ok(());
    };
    let document = match options {
        Ok(Some(options)) => startup::open_document(
            &options,
            Path::new("/etc/tiny-dfr/config.toml"),
            &example_deck(),
        ),
        _ => Err(startup::Problem::options()),
    };
    let failed = document.is_err();
    if let Err(problem) = &document {
        eprintln!("{}: {}", problem.heading, problem.body);
    }
    adw::init().map_err(|_| anyhow::anyhow!("Cannot initialize Touch Bar Settings. Launch from the graphical desktop as its ordinary user; check the user-manager display/session environment and journalctl --user -b. See INSTALL.md (https://github.com/kirkins/mac-touchbar-omarchy). No files changed."))?;
    let app = adw::Application::builder().application_id(APP_ID).build();
    let document = std::cell::RefCell::new(Some(document));
    app.connect_activate(move |app| {
        if let Some(window) = app.active_window() {
            window.present();
        } else if let Some(document) = document.borrow_mut().take() {
            match document {
                Ok(doc) => build_window(app, doc).window.present(),
                Err(problem) => build_startup_error(app, &problem).present(),
            }
        }
    });
    // The app owns GTK argument parsing; ours are already consumed.
    app.run_with_args::<String>(&[]);
    if failed {
        anyhow::bail!(
            "Touch Bar Settings could not load; no files changed. See the startup guidance above."
        );
    }
    Ok(())
}

fn build_startup_error(
    app: &adw::Application,
    problem: &startup::Problem,
) -> adw::ApplicationWindow {
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Touch Bar Settings — Cannot Open")
        .default_width(560)
        .default_height(460)
        .build();
    let content = gtk4::Box::new(gtk4::Orientation::Vertical, 18);
    ui::margins(&content, 24);
    let heading = gtk4::Label::new(Some(problem.heading));
    heading.add_css_class("title-2");
    heading.set_wrap(true);
    let body = gtk4::Label::new(Some(problem.body));
    body.set_wrap(true);
    body.set_selectable(true);
    body.set_xalign(0.0);
    let close = gtk4::Button::with_label("Close");
    close.connect_clicked(glib::clone!(
        #[weak]
        window,
        move |_| window.close()
    ));
    content.append(&heading);
    content.append(&body);
    content.append(&close);
    let scroll = gtk4::ScrolledWindow::builder().child(&content).build();
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(&scroll));
    window.set_content(Some(&toolbar));
    window
}

struct Workspace {
    window: adw::ApplicationWindow,
    #[cfg(test)]
    editor: Rc<Editor>,
    #[cfg(test)]
    layers: Rc<LayersPage>,
    #[cfg(test)]
    stack: adw::ViewStack,
    #[cfg(test)]
    save: gio::SimpleAction,
    #[cfg(test)]
    error: gtk4::Label,
}

fn build_window(app: &adw::Application, doc: model::Document) -> Workspace {
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .default_width(1100)
        .default_height(760)
        .title("Touch Bar")
        .build();
    window.set_size_request(360, 420);
    let layer = doc.layer_roles().0;
    let guidance = doc.save_guidance();
    let editor = Editor::new(doc, preview::Preview::new(&layer));
    editor.selection.borrow_mut().layer = editor
        .doc
        .borrow()
        .layer_snapshot()
        .iter()
        .position(|l| l.name == layer)
        .unwrap_or(0);
    let layers = LayersPage::build(&editor);
    let appearance = knobs::SettingsPage::build(&editor, knobs::PageKind::Appearance);
    let behavior = knobs::SettingsPage::build(&editor, knobs::PageKind::Behavior);
    let stack = adw::ViewStack::new();
    stack.set_vexpand(true);
    for (child, name, title, icon) in [
        (
            layers.root.clone().upcast::<gtk4::Widget>(),
            "layers",
            "Layers",
            "view-list-symbolic",
        ),
        (
            appearance.root.clone().upcast(),
            "appearance",
            "Appearance",
            "applications-graphics-symbolic",
        ),
        (
            behavior.root.clone().upcast(),
            "behavior",
            "Behavior",
            "preferences-system-symbolic",
        ),
    ] {
        stack
            .add_titled(&child, Some(name), title)
            .set_icon_name(Some(icon));
    }
    let switcher = adw::ViewSwitcher::builder()
        .stack(&stack)
        .policy(adw::ViewSwitcherPolicy::Wide)
        .build();
    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&switcher));
    let save_button = gtk4::Button::with_label(guidance.save_label);
    save_button.set_tooltip_text(Some(guidance.save_tooltip));
    save_button.add_css_class("suggested-action");
    save_button.set_action_name(Some("win.save"));
    header.pack_end(&save_button);
    let menu = gtk4::MenuButton::builder()
        .icon_name("open-menu-symbolic")
        .tooltip_text("Main Menu")
        .build();
    let menu_model = gio::Menu::new();
    menu_model.append(Some("Configuration File"), Some("win.file-info"));
    menu_model.append(Some("Keyboard Shortcuts"), Some("win.shortcut-info"));
    menu.set_menu_model(Some(&menu_model));
    header.pack_end(&menu);
    let bottom = adw::ViewSwitcherBar::builder()
        .stack(&stack)
        .reveal(false)
        .build();
    let breakpoint =
        adw::Breakpoint::new(adw::BreakpointCondition::parse("max-width: 850sp").unwrap());
    breakpoint.add_setter(&layers.root, "collapsed", Some(&true.to_value()));
    breakpoint.add_setter(&layers.root, "show-content", Some(&false.to_value()));
    breakpoint.add_setter(&switcher, "visible", Some(&false.to_value()));
    breakpoint.add_setter(&bottom, "reveal", Some(&true.to_value()));
    window.add_breakpoint(breakpoint);
    let toast = adw::ToastOverlay::new();
    let status = gtk4::Label::new(None);
    status.set_xalign(0.0);
    status.add_css_class("dim-label");
    let summary = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
    ui::margins(&summary, 12);
    summary.append(&status);
    let validation = gtk4::Label::new(None);
    validation.set_wrap(true);
    validation.set_lines(3);
    validation.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    validation.set_xalign(0.0);
    validation.set_hexpand(true);
    validation.add_css_class("error");
    let review = gtk4::Button::with_label("Review Issue");
    summary.append(&validation);
    summary.append(&review);
    let error_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
    ui::margins(&error_row, 12);
    let error = gtk4::Label::new(None);
    error.set_wrap(true);
    error.set_xalign(0.0);
    error.set_hexpand(true);
    error.add_css_class("error");
    let retry = gtk4::Button::with_label("Retry Save");
    retry.set_action_name(Some("win.save"));
    let dismiss = gtk4::Button::from_icon_name("window-close-symbolic");
    dismiss.set_tooltip_text(Some("Dismiss Save Error"));
    error_row.append(&error);
    error_row.append(&retry);
    error_row.append(&dismiss);
    let body = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    let apply_banner = adw::Banner::new(guidance.banner);
    apply_banner.set_revealed(true);
    apply_banner.set_button_label(Some(guidance.instructions_label));
    apply_banner.connect_button_clicked(glib::clone!(
        #[weak]
        window,
        move |_| {
            ui::error_dialog(&window, guidance.instructions_title, guidance.instructions);
        }
    ));
    body.append(&apply_banner);
    body.append(&summary);
    body.append(&error_row);
    body.append(&stack);
    toast.set_child(Some(&body));
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.add_bottom_bar(&bottom);
    toolbar.set_content(Some(&toast));
    window.set_content(Some(&toolbar));
    let save = gio::SimpleAction::new("save", None);
    window.add_action(&save);
    app.set_accels_for_action("win.save", &["<Primary>s"]);
    let pending: Rc<std::cell::RefCell<Option<glib::SourceId>>> = Rc::new(Default::default());
    let diagnostics = Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
    editor.set_after_edit(Box::new(glib::clone!(
        #[weak]
        status,
        #[weak]
        validation,
        #[weak]
        review,
        #[weak]
        error,
        #[weak]
        error_row,
        #[weak]
        save,
        #[weak]
        layers,
        #[weak]
        appearance,
        #[weak]
        behavior,
        #[strong]
        pending,
        #[strong]
        diagnostics,
        move |editor: Rc<Editor>| {
            let errors = editor.doc.borrow().validate();
            let doc = editor.doc.borrow();
            save.set_enabled(errors.is_empty() && (doc.is_dirty() || !doc.user_path().exists()));
            status.set_text(if doc.is_dirty() {
                "Unsaved changes"
            } else if !doc.user_path().exists() {
                "New configuration"
            } else {
                ""
            });
            status.set_visible(!status.text().is_empty());
            validation.set_text(&errors.join("\n"));
            validation.set_tooltip_text((!errors.is_empty()).then(|| errors.join("\n")).as_deref());
            validation.set_visible(!errors.is_empty());
            review.set_visible(!errors.is_empty());
            drop(doc);
            *diagnostics.borrow_mut() = errors.clone();
            let feedback = editor.save_feedback.borrow();
            error_row.set_visible(feedback.error.is_some());
            error.set_text(
                &feedback
                    .error
                    .as_ref()
                    .map(|e| format!("Save failed — {e}"))
                    .unwrap_or_default(),
            );
            drop(feedback);
            layers.update_summaries(&editor);
            appearance.update_feedback(&editor);
            behavior.update_feedback(&editor);
            if let Some(id) = pending.borrow_mut().take() {
                id.remove();
            }
            let mut previews = vec![editor.preview.clone()];
            editor
                .previews
                .borrow_mut()
                .retain(|p| p.strong_count() > 0);
            previews.extend(editor.previews.borrow().iter().filter_map(|p| p.upgrade()));
            for preview in &previews {
                if errors.is_empty() {
                    preview.invalidate();
                } else {
                    preview.pause(&errors.join("\n"));
                }
            }
            if errors.is_empty() {
                let weak = Rc::downgrade(&editor);
                *pending.borrow_mut() = Some(glib::timeout_add_local_once(
                    Duration::from_millis(180),
                    glib::clone!(
                        #[strong]
                        pending,
                        move || {
                            pending.borrow_mut().take();
                            if let Some(editor) = weak.upgrade() {
                                for preview in previews {
                                    preview.render(&editor.doc.borrow());
                                }
                            }
                        }
                    ),
                ));
            }
        }
    )));
    editor.set_on_selection(Box::new(glib::clone!(
        #[weak]
        layers,
        move |editor: Rc<Editor>| {
            editor
                .preview
                .set_selected(editor.selection.borrow().button);
            layers.show_selection(&editor);
        }
    )));
    editor.set_on_structure(Box::new(glib::clone!(
        #[weak]
        layers,
        #[weak]
        appearance,
        #[weak]
        behavior,
        move |editor: Rc<Editor>| {
            layers.refresh(&editor);
            appearance.refresh(&editor);
            behavior.refresh(&editor);
            editor.run_pipeline();
        }
    )));
    for (action_name, page) in [
        ("show-layers", "layers"),
        ("show-behavior", "behavior"),
        ("show-appearance", "appearance"),
    ] {
        let action = gio::SimpleAction::new(action_name, None);
        action.connect_activate(glib::clone!(
            #[weak]
            stack,
            move |_, _| stack.set_visible_child_name(page)
        ));
        window.add_action(&action);
    }
    review.connect_clicked(glib::clone!(
        #[strong]
        editor,
        #[weak]
        stack,
        #[strong]
        diagnostics,
        move |_| {
            let errors = diagnostics.borrow().join("\n");
            let page = if errors.contains("Battery")
                || errors.contains("Brightness")
                || errors.contains("Pet")
                || errors.contains("FnLayer")
                || errors.contains("DefaultLayer")
            {
                "behavior"
            } else if errors.contains("Theme")
                || errors.contains("Color")
                || errors.contains("Band")
                || errors.contains("Font")
            {
                "appearance"
            } else {
                "layers"
            };
            stack.set_visible_child_name(page);
            if page == "layers" {
                let layers = editor.doc.borrow().layer_snapshot();
                if let Some(index) = layers
                    .iter()
                    .position(|layer| errors.contains(&format!("{:?}", layer.name)))
                {
                    editor.select(index, None);
                }
            }
        }
    ));
    dismiss.connect_clicked(glib::clone!(
        #[strong]
        editor,
        move |_| {
            editor.save_feedback.borrow_mut().error = None;
            editor.run_pipeline();
        }
    ));
    save.connect_activate(glib::clone!(
        #[strong]
        editor,
        #[weak]
        toast,
        move |action, _| {
            if !action.is_enabled() {
                return;
            }
            let result = editor
                .doc
                .borrow_mut()
                .save()
                .map_err(|error| format!("{error:#}"));
            editor.save_feedback.borrow_mut().record(result.clone());
            editor.run_pipeline();
            if result.is_ok() {
                // The persistent banner offers instructions without interrupting
                // every successful save with a modal dialog.
                toast.add_toast(adw::Toast::new(editor.doc.borrow().save_message()));
            }
        }
    ));
    let file_info = gio::SimpleAction::new("file-info", None);
    file_info.connect_activate(glib::clone!(
        #[strong]
        editor,
        #[weak]
        window,
        move |_, _| {
            ui::error_dialog(
                &window,
                "Configuration File",
                &format!(
                    "Editing {}.\n\n{}",
                    editor.doc.borrow().user_path().display(),
                    editor.doc.borrow().save_guidance().instructions
                ),
            );
        }
    ));
    window.add_action(&file_info);
    let shortcuts = gio::SimpleAction::new("shortcut-info", None);
    shortcuts.connect_activate(glib::clone!(#[weak] window, move |_, _| ui::error_dialog(&window,"Keyboard Shortcuts","Ctrl+S saves valid changes to the selected configuration file. Use the arrow keys to select rows and Enter to open button properties. Alt+Left returns to the button list.")));
    window.add_action(&shortcuts);
    let close_guard = Rc::new(std::cell::RefCell::new(model::CloseGuard::default()));
    window.connect_close_request(glib::clone!(
        #[strong]
        editor,
        #[strong]
        close_guard,
        #[weak]
        toast,
        #[upgrade_or]
        glib::Propagation::Proceed,
        move |window| {
            if close_guard
                .borrow_mut()
                .request(editor.doc.borrow().is_dirty())
            {
                return glib::Propagation::Proceed;
            }
            if close_guard.borrow().dialog_open {
                return glib::Propagation::Stop;
            }
            close_guard.borrow_mut().dialog_open = true;
            let dialog = adw::AlertDialog::builder()
                .heading("Unsaved Changes")
                .body("Save changes to the selected file, discard them, or keep editing.")
                .build();
            dialog.add_response("cancel", "Cancel");
            dialog.add_response("discard", "Discard");
            dialog.set_response_appearance("discard", adw::ResponseAppearance::Destructive);
            dialog.set_default_response(Some("cancel"));
            dialog.set_close_response("cancel");
            if editor.doc.borrow().validate().is_empty() {
                dialog.add_response("save", guidance.close_save_label);
            }
            dialog.choose(
                Some(window),
                None::<&gio::Cancellable>,
                glib::clone!(
                    #[strong]
                    editor,
                    #[strong]
                    close_guard,
                    #[weak]
                    window,
                    #[weak]
                    toast,
                    move |response| {
                        close_guard.borrow_mut().dialog_open = false;
                        if response == "save" {
                            let result =
                                editor.doc.borrow_mut().save().map_err(|e| format!("{e:#}"));
                            editor.save_feedback.borrow_mut().record(result.clone());
                            editor.run_pipeline();
                            if result.is_err() {
                                return;
                            }
                            toast.add_toast(adw::Toast::new(editor.doc.borrow().save_message()));
                        }
                        if response == "discard" || response == "save" {
                            close_guard.borrow_mut().discard();
                            window.close();
                        }
                    }
                ),
            );
            glib::Propagation::Stop
        }
    ));
    // Keep page controllers alive with the window. Editor callbacks hold only weak controller references.
    window.connect_destroy(glib::clone!(
        #[strong]
        layers,
        #[strong]
        appearance,
        #[strong]
        behavior,
        #[strong]
        pending,
        move |_| {
            if let Some(id) = pending.borrow_mut().take() {
                id.remove();
            }
            let _ = (&layers, &appearance, &behavior);
        }
    ));
    editor.run_pipeline();
    Workspace {
        window,
        #[cfg(test)]
        editor,
        #[cfg(test)]
        layers,
        #[cfg(test)]
        stack,
        #[cfg(test)]
        save,
        #[cfg(test)]
        error,
    }
}

use gtk4::gio;

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn widgets(root: &impl IsA<gtk4::Widget>) -> Vec<gtk4::Widget> {
        let mut result = vec![root.as_ref().clone()];
        let mut child = root.first_child();
        while let Some(widget) = child {
            child = widget.next_sibling();
            result.extend(widgets(&widget));
        }
        result
    }

    fn pump(ms: u64) {
        let end = Instant::now() + Duration::from_millis(ms);
        while Instant::now() < end {
            while glib::MainContext::default().pending() {
                glib::MainContext::default().iteration(false);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn entry(window: &adw::ApplicationWindow, title: &str) -> adw::EntryRow {
        let mut found = None;
        assert!(
            until(2000, || {
                found = widgets(window)
                    .into_iter()
                    .filter_map(|w| w.downcast::<adw::EntryRow>().ok())
                    .find(|r| r.title() == title && r.is_mapped());
                found.is_some()
            }),
            "the selected {title} field must be mapped"
        );
        found.unwrap()
    }

    fn button(window: &adw::ApplicationWindow, label: &str) -> gtk4::Button {
        let mut found = None;
        assert!(
            until(2000, || {
                found = widgets(window)
                    .into_iter()
                    .filter_map(|w| w.downcast::<gtk4::Button>().ok())
                    .find(|r| r.label().as_deref() == Some(label) && r.is_mapped());
                found.is_some()
            }),
            "the {label} button must be mapped"
        );
        found.unwrap()
    }

    fn activate_dialog_button(window: &adw::ApplicationWindow, label: &str) {
        // A closing dialog can remain mapped during its transition. Scope the
        // lookup to the current dialog, rather than matching an old response
        // button with the same label elsewhere in the window.
        let dialog = window
            .visible_dialog()
            .expect("current confirmation dialog");
        let mut found = None;
        assert!(until(2000, || {
            found = widgets(&dialog)
                .into_iter()
                .filter_map(|widget| widget.downcast::<gtk4::Button>().ok())
                .find(|button| {
                    button.label().as_deref() == Some(label)
                        && button.is_mapped()
                        && button.is_sensitive()
                });
            found.is_some()
        }));
        let button = found.unwrap();
        assert!(button.is_ancestor(&dialog));
        assert!(button.grab_focus());
        // GtkWidget activation is the same path used by Space on a focused
        // GtkButton, including its native activation/animation timing.
        assert!(button.activate());
    }

    fn proof(workspace: &Workspace, name: &str, width: i32, height: i32) {
        workspace.window.set_size_request(width, height);
        workspace.window.set_default_size(width, height);
        pump(250);
        // Let GTK and the compositor finish the allocation instead of forcing a
        // new allocation immediately before reading the cached widget snapshot.
        let painted = Rc::new(std::cell::Cell::new(false));
        let clock = workspace
            .window
            .frame_clock()
            .expect("mapped window frame clock");
        let handler = clock.connect_after_paint(glib::clone!(
            #[strong]
            painted,
            move |_| painted.set(true)
        ));
        workspace.window.queue_draw();
        assert!(
            until(2000, || painted.get()),
            "capture waits for a real GTK paint"
        );
        clock.disconnect(handler);
        let Some(directory) = std::env::var_os("DFR_CONFIGURATOR_PROOF_DIR") else {
            return;
        };
        let directory = PathBuf::from(directory);
        std::fs::create_dir_all(&directory).unwrap();
        let snapshot = gtk4::Snapshot::new();
        let paintable = gtk4::WidgetPaintable::new(Some(&workspace.window));
        paintable.snapshot(&snapshot, width as f64, height as f64);
        let node = snapshot
            .to_node()
            .expect("native window snapshot after GTK paint");
        let renderer = workspace.window.renderer().expect("native renderer");
        let texture = renderer.render_texture(
            &node,
            Some(&gtk4::graphene::Rect::new(
                0.,
                0.,
                width as f32,
                height as f32,
            )),
        );
        texture
            .save_to_png(directory.join(format!("{name}.png")))
            .unwrap();
    }

    /// Run explicitly on a GTK display; all writes use an isolated temporary configuration.
    #[test]
    #[ignore = "requires a GTK display; run with --ignored --test-threads=1"]
    fn native_selection_structure_save_feedback_and_layouts() {
        adw::init().unwrap();
        let app = adw::Application::builder()
            .application_id("org.dfrdeck.Configurator.Tests")
            .flags(gio::ApplicationFlags::NON_UNIQUE)
            .build();
        app.register(None::<&gio::Cancellable>).unwrap();
        let directory =
            std::env::temp_dir().join(format!("dfr-configurator-ui-test-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let base = directory.join("base.toml");
        let user = directory.join("config.toml");
        std::fs::write(&base, include_str!("../../../share/tiny-dfr/config.toml")).unwrap();
        std::fs::write(&user, include_str!("../../../share/tiny-dfr/multibar.toml")).unwrap();
        let workspace = build_window(&app, model::Document::open(&base, &user).unwrap());
        workspace.window.present();
        pump(250);
        assert!(!workspace.editor.doc.borrow().is_dirty());
        assert!(!workspace.save.is_enabled());
        assert!(widgets(&workspace.window).into_iter().any(|widget| widget
            .downcast::<adw::Banner>()
            .ok()
            .is_some_and(|banner| banner.is_revealed()
                && banner.button_label().as_deref() == Some("Apply Instructions"))));
        proof(&workspace, "layers-1100x760", 1100, 760);
        workspace.editor.select(0, Some(0));
        let mut icon_row = None;
        assert!(until(2000, || {
            icon_row = widgets(&workspace.window)
                .into_iter()
                .filter_map(|widget| widget.downcast::<adw::ActionRow>().ok())
                .find(|row| row.title() == "Icon" && row.is_mapped());
            icon_row.is_some()
        }));
        icon_row.unwrap().emit_by_name::<()>("activated", &[]);
        assert!(until(2000, || workspace.window.visible_dialog().is_some()));
        let icon_dialog = workspace.window.visible_dialog().unwrap();
        let icon_list = widgets(&icon_dialog)
            .into_iter()
            .find_map(|widget| widget.downcast::<gtk4::ListView>().ok())
            .expect("resource choices use a native list model");
        let search = widgets(&icon_dialog)
            .into_iter()
            .find_map(|widget| widget.downcast::<gtk4::SearchEntry>().ok())
            .unwrap();
        let original_icon = workspace.editor.doc.borrow().layer_snapshot()[0].keys[0]
            .icon
            .clone()
            .unwrap();
        search.set_text(&original_icon);
        assert!(until(2000, || {
            let model = icon_list.model().unwrap();
            model.n_items() > 0
                && model
                    .item(0)
                    .and_downcast::<gtk4::StringObject>()
                    .is_some_and(|item| item.string().contains(&original_icon))
        }));
        icon_list.emit_by_name::<()>("activate", &[&0u32]);
        assert!(until(2000, || workspace.window.visible_dialog().is_none()));
        assert!(workspace.editor.doc.borrow().layer_snapshot()[0].keys[0]
            .icon
            .as_ref()
            .is_some_and(|name| name.contains(&original_icon)));
        workspace.editor.select(0, Some(3));
        assert_eq!(entry(&workspace.window, "Label").text(), "F1-F12");
        entry(&workspace.window, "Label").set_text("Function Keys");
        assert_eq!(entry(&workspace.window, "Label").text(), "Function Keys");
        assert!(workspace.save.is_enabled());
        workspace.editor.select(1, Some(1));
        assert_eq!(entry(&workspace.window, "Label").text(), "F1");
        workspace.editor.select(0, None);
        pump(250);
        button(&workspace.window, "Add Button").emit_clicked();
        pump(250);
        assert_eq!(entry(&workspace.window, "Label").text(), "Button");
        let count = workspace.editor.doc.borrow().layer_snapshot()[0].keys.len();
        workspace.editor.select(0, None);
        pump(250);
        button(&workspace.window, "Add Button").emit_clicked();
        pump(250);
        assert_eq!(
            workspace.editor.doc.borrow().layer_snapshot()[0].keys.len(),
            count + 1
        );
        assert_eq!(entry(&workspace.window, "Label").text(), "Button");
        proof(&workspace, "button-properties-1000x650", 1000, 650);
        std::fs::create_dir(directory.join("config.toml.tmp")).unwrap();
        workspace.save.activate(None);
        assert!(workspace.editor.doc.borrow().is_dirty());
        assert!(workspace.error.text().contains("staging"));
        workspace.editor.select(1, None);
        assert!(workspace.error.text().contains("staging"));
        assert!(workspace.error.is_visible());
        std::fs::remove_dir(directory.join("config.toml.tmp")).unwrap();
        workspace.save.activate(None);
        pump(250);
        assert!(
            workspace.window.visible_dialog().is_none(),
            "Save must not interrupt editing"
        );
        button(&workspace.window, "Apply Instructions").emit_clicked();
        assert!(until(2000, || workspace.window.visible_dialog().is_some()));
        assert!(widgets(&workspace.window.visible_dialog().unwrap())
            .into_iter()
            .any(|widget| widget
                .downcast::<gtk4::Label>()
                .ok()
                .is_some_and(|label| label.text().contains("administrator")
                    && label.text().contains("does not apply"))));
        activate_dialog_button(&workspace.window, "Close");
        assert!(until(2000, || workspace.window.visible_dialog().is_none()));
        assert!(!workspace.editor.doc.borrow().is_dirty());
        assert!(!workspace.error.is_visible());
        assert!(!workspace.save.is_enabled());
        workspace.stack.set_visible_child_name("appearance");
        pump(250);
        proof(&workspace, "appearance-1000x650", 1000, 650);
        button(&workspace.window, "Enlarge").emit_clicked();
        pump(250);
        assert!(
            until(2000, || workspace.window.visible_dialog().is_some()),
            "Enlarge opens a native dialog"
        );
        let dialog = workspace.window.visible_dialog().unwrap();
        assert!(widgets(&dialog).iter().any(|w| w
            .clone()
            .downcast::<adw::ComboRow>()
            .ok()
            .is_some_and(|r| r.title() == "Scale")));
        dialog.close();
        pump(250);
        workspace.stack.set_visible_child_name("behavior");
        pump(250);
        let pet = widgets(&workspace.window)
            .into_iter()
            .filter_map(|w| w.downcast::<adw::SwitchRow>().ok())
            .find(|r| r.title() == "Pet")
            .unwrap();
        assert!(pet.is_active());
        assert!(!workspace.editor.doc.borrow().has_override("PetEnabled"));
        proof(&workspace, "behavior-1000x650", 1000, 650);
        workspace.stack.set_visible_child_name("layers");
        workspace.editor.select(0, None);
        pump(250);
        proof(&workspace, "layers-narrow-700x650", 700, 650);
        assert!(workspace.layers.root.is_collapsed());
        let provider = gtk4::CssProvider::new();
        provider.load_from_data("* { font-size: 20px; }");
        gtk4::style_context_add_provider_for_display(
            &gtk4::gdk::Display::default().unwrap(),
            &provider,
            gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
        workspace.editor.select(0, Some(3));
        pump(250);
        proof(&workspace, "button-properties-large-text-700x650", 700, 650);
        gtk4::style_context_remove_provider_for_display(
            &gtk4::gdk::Display::default().unwrap(),
            &provider,
        );
        let saved_text = std::fs::read_to_string(&user).unwrap();
        workspace
            .editor
            .doc
            .borrow_mut()
            .user_mut()
            .active_brightness = Some(129);
        workspace.editor.run_pipeline();
        workspace.window.close();
        assert!(until(2000, || workspace.window.visible_dialog().is_some()));
        activate_dialog_button(&workspace.window, "Cancel");
        assert!(until(2000, || workspace.window.visible_dialog().is_none()));
        assert!(workspace.window.is_visible());
        assert!(workspace.editor.doc.borrow().is_dirty());
        workspace.window.close();
        assert!(until(2000, || workspace.window.visible_dialog().is_some()));
        activate_dialog_button(&workspace.window, "Discard");
        assert!(
            until(2000, || !workspace.window.is_visible()),
            "Discard closes without re-entering the dirty guard"
        );
        assert!(workspace.editor.doc.borrow().is_dirty());
        assert_eq!(
            std::fs::read_to_string(&user).unwrap(),
            saved_text,
            "Discard leaves the saved file unchanged"
        );
        workspace.window.destroy();
        pump(100);
        std::fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    #[ignore = "requires an isolated GTK display; never run on the live desktop"]
    fn native_startup_error_is_bounded_readable_and_closable() {
        adw::init().unwrap();
        let app = adw::Application::builder()
            .application_id("org.dfrdeck.Configurator.StartupTests")
            .flags(gio::ApplicationFlags::NON_UNIQUE)
            .build();
        app.register(None::<&gio::Cancellable>).unwrap();
        let window = build_startup_error(&app, &startup::Problem::options());
        window.present();
        assert!(until(2000, || window.is_mapped()));
        assert!(widgets(&window).into_iter().any(|widget| widget
            .downcast::<gtk4::Label>()
            .ok()
            .is_some_and(|label| label.is_selectable() && label.text().contains("--help"))));
        button(&window, "Close").emit_clicked();
        assert!(until(2000, || !window.is_visible()));
        window.destroy();
    }

    fn until(timeout: u64, mut condition: impl FnMut() -> bool) -> bool {
        let end = Instant::now() + Duration::from_millis(timeout);
        loop {
            while glib::MainContext::default().pending() {
                glib::MainContext::default().iteration(false);
            }
            if condition() {
                return true;
            }
            if Instant::now() >= end {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}
