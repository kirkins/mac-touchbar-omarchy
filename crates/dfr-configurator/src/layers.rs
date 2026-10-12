//! The Layers page: the ordered collection and focused button properties,
//! including action editing and explicit key-combo capture.
//! Every edit writes through to the document model; nothing here
//! touches a file until Save.

use crate::capture::{key_from_gdk, key_name};
use crate::model::{button_kind, set_button_kind, ActionForm, ButtonKind, Document};
use crate::preview::Preview;
use crate::ui;
use gtk4::{glib, prelude::*};
use input_linux::Key;
use libadwaita::{self as adw, prelude::*};
use std::cell::RefCell;
use std::rc::{Rc, Weak};

pub const ACCENTS: &[&str] = &[
    "neutral", "cyan", "amber", "blue", "violet", "orange", "red", "green",
];

type EditorCallback = Box<dyn Fn(Rc<Editor>)>;

/// Shared document and selection context. The window owns the page
/// controllers; installed hooks use weak controller references. The
/// after-edit hook validates, updates feedback and debounces previews;
/// selection updates the focused properties; structure updates rebuild
/// the affected collections and forms after add/remove/reorder or kind
/// changes. Additional previews are held weakly here.
pub struct Editor {
    pub doc: RefCell<Document>,
    pub selection: RefCell<Selection>,
    after_edit: RefCell<Option<EditorCallback>>,
    on_structure: RefCell<Option<EditorCallback>>,
    pub preview: Rc<Preview>,
    pub save_feedback: RefCell<crate::model::SaveFeedback>,
    pub previews: RefCell<Vec<Weak<Preview>>>,
    on_selection: RefCell<Option<EditorCallback>>,
}

impl Editor {
    pub fn new(doc: Document, preview: Rc<Preview>) -> Rc<Self> {
        Rc::new(Self {
            doc: RefCell::new(doc),
            selection: RefCell::new(Selection {
                layer: 0,
                button: None,
            }),
            after_edit: RefCell::new(None),
            on_structure: RefCell::new(None),
            preview,
            save_feedback: RefCell::new(Default::default()),
            previews: RefCell::new(Vec::new()),
            on_selection: RefCell::new(None),
        })
    }

    pub fn select(self: &Rc<Self>, layer: usize, button: Option<usize>) {
        *self.selection.borrow_mut() = Selection { layer, button };
        if let Some(layer) = self.doc.borrow().layer_snapshot().get(layer) {
            self.preview.set_layer(&layer.name);
        }
        self.selection_changed();
    }

    pub fn set_on_selection(&self, callback: EditorCallback) {
        *self.on_selection.borrow_mut() = Some(callback);
    }

    pub fn selection_changed(self: &Rc<Self>) {
        let callback = self.on_selection.borrow_mut().take();
        if let Some(callback) = callback {
            callback(self.clone());
            *self.on_selection.borrow_mut() = Some(callback);
        }
        self.run_pipeline();
    }

    pub fn set_after_edit(&self, callback: EditorCallback) {
        *self.after_edit.borrow_mut() = Some(callback);
    }

    pub fn set_on_structure(&self, callback: EditorCallback) {
        *self.on_structure.borrow_mut() = Some(callback);
    }

    /// Run the after-edit pipeline. Taking the callback out for the
    /// call makes nested notifications no-ops, which keeps signal
    /// cascades from recursing.
    pub fn run_pipeline(self: &Rc<Self>) {
        let callback = self.after_edit.borrow_mut().take();
        if let Some(callback) = callback {
            callback(self.clone());
            *self.after_edit.borrow_mut() = Some(callback);
        }
    }

    /// A shape-changing edit: the pipeline plus the structure hook.
    pub fn restructured(self: &Rc<Self>) {
        self.run_pipeline();
        let callback = self.on_structure.borrow_mut().take();
        if let Some(callback) = callback {
            callback(self.clone());
            *self.on_structure.borrow_mut() = Some(callback);
        }
    }
}

/// What the right pane is editing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Selection {
    pub layer: usize,
    pub button: Option<usize>,
}

pub(crate) fn accent_from_name(name: &str) -> Option<dfr_config::ButtonAccent> {
    use dfr_config::ButtonAccent;
    Some(match name {
        "neutral" => ButtonAccent::Neutral,
        "cyan" => ButtonAccent::Cyan,
        "amber" => ButtonAccent::Amber,
        "blue" => ButtonAccent::Blue,
        "violet" => ButtonAccent::Violet,
        "orange" => ButtonAccent::Orange,
        "red" => ButtonAccent::Red,
        "green" => ButtonAccent::Green,
        _ => return None,
    })
}

pub struct LayersPage {
    pub root: adw::NavigationSplitView,
    rows: gtk4::ListBox,
    buttons: gtk4::ListBox,
    heading: adw::WindowTitle,
    help: gtk4::Label,
    navigation: adw::NavigationView,
    overview: adw::NavigationPage,
    detail: RefCell<Option<adw::NavigationPage>>,
    refreshing: std::cell::Cell<bool>,
}

impl LayersPage {
    pub fn build(editor: &Rc<Editor>) -> Rc<Self> {
        let split = adw::NavigationSplitView::builder()
            .min_sidebar_width(220.0)
            .max_sidebar_width(280.0)
            .sidebar_width_fraction(0.23)
            .build();
        let rows = gtk4::ListBox::new();
        rows.set_selection_mode(gtk4::SelectionMode::Single);
        rows.add_css_class("navigation-sidebar");
        ui::margins(&rows, 12);
        let add = gtk4::Button::from_icon_name("list-add-symbolic");
        add.set_tooltip_text(Some("Add Layer"));
        let sidebar_header = adw::HeaderBar::new();
        sidebar_header.set_show_start_title_buttons(false);
        sidebar_header.set_show_end_title_buttons(false);
        sidebar_header.pack_end(&add);
        let sidebar_toolbar = adw::ToolbarView::new();
        sidebar_toolbar.add_top_bar(&sidebar_header);
        sidebar_toolbar.set_content(Some(&ui::scroll(&rows)));
        let sidebar = adw::NavigationPage::new(&sidebar_toolbar, "Layers");
        sidebar.set_tag(Some("collection"));
        split.set_sidebar(Some(&sidebar));

        let navigation = adw::NavigationView::new();
        let content = gtk4::Box::new(gtk4::Orientation::Vertical, 18);
        ui::margins(&content, 24);
        content.append(&crate::preview::compact(editor, &editor.preview));
        let add_button = gtk4::Button::with_label("Add Button");
        content.append(&ui::header("Buttons", &add_button));
        let buttons = gtk4::ListBox::new();
        buttons.set_selection_mode(gtk4::SelectionMode::Single);
        buttons.add_css_class("boxed-list");
        content.append(&ui::scroll(&buttons));
        let help = gtk4::Label::new(Some("Choose a button to change its appearance or action."));
        help.set_wrap(true);
        help.set_xalign(0.0);
        help.add_css_class("dim-label");
        content.append(&help);
        let heading = adw::WindowTitle::new("", "");
        let options = gtk4::Button::with_label("Layer Options");
        let header = adw::HeaderBar::new();
        header.set_show_start_title_buttons(false);
        header.set_show_end_title_buttons(false);
        header.set_title_widget(Some(&heading));
        header.pack_end(&options);
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
        toolbar.set_content(Some(&content));
        let overview = adw::NavigationPage::new(&toolbar, "Buttons");
        overview.set_tag(Some("buttons"));
        navigation.add(&overview);
        let workspace = adw::NavigationPage::new(&navigation, "Layer");
        workspace.set_tag(Some("workspace"));
        split.set_content(Some(&workspace));
        split.set_show_content(true);
        let page = Rc::new(Self {
            root: split,
            rows,
            buttons,
            heading,
            help,
            navigation,
            overview,
            detail: RefCell::new(None),
            refreshing: std::cell::Cell::new(false),
        });
        page.rows.connect_row_selected(glib::clone!(
            #[weak]
            page,
            #[strong]
            editor,
            move |_, row| {
                if page.refreshing.get() {
                    return;
                }
                if let Some(row) = row {
                    // GTK can reselect the current row while restoring focus after
                    // a structural edit. That is not a request to leave properties.
                    if editor.selection.borrow().layer == row.index() as usize {
                        return;
                    }
                    editor.select(row.index() as usize, None);
                    page.root.set_show_content(true);
                }
            }
        ));
        // Activating the selected collection row also enters it when collapsed.
        page.rows.connect_row_activated(glib::clone!(
            #[weak]
            page,
            #[strong]
            editor,
            move |_, row| {
                editor.select(row.index() as usize, None);
                page.root.set_show_content(true);
            }
        ));
        add.connect_clicked(glib::clone!(
            #[strong]
            editor,
            move |_| {
                let count = {
                    let mut doc = editor.doc.borrow_mut();
                    let layers = doc.layers();
                    let mut n = layers.len() + 1;
                    while layers.iter().any(|l| l.name == format!("Layer {n}")) {
                        n += 1;
                    }
                    layers.push(dfr_config::LayerConfig {
                        name: format!("Layer {n}"),
                        keys: vec![new_button()],
                    });
                    layers.len()
                };
                *editor.selection.borrow_mut() = Selection {
                    layer: count - 1,
                    button: None,
                };
                editor.restructured();
                editor.select(count - 1, None);
            }
        ));
        add_button.connect_clicked(glib::clone!(
            #[strong]
            editor,
            move |_| {
                let layer = editor.selection.borrow().layer;
                let index = {
                    let mut doc = editor.doc.borrow_mut();
                    let Some(layer) = doc.layers().get_mut(layer) else {
                        return;
                    };
                    layer.keys.push(new_button());
                    layer.keys.len() - 1
                };
                *editor.selection.borrow_mut() = Selection {
                    layer,
                    button: Some(index),
                };
                editor.restructured();
                editor.select(layer, Some(index));
            }
        ));
        options.connect_clicked(glib::clone!(
            #[strong]
            editor,
            move |button| layer_options(&editor, button)
        ));
        page.refresh(editor);
        page
    }

    pub fn refresh(&self, editor: &Rc<Editor>) {
        self.refreshing.set(true);
        while let Some(child) = self.rows.first_child() {
            self.rows.remove(&child);
        }
        let layers = editor.doc.borrow().layer_snapshot();
        let (startup, function) = editor.doc.borrow().layer_roles();
        for layer in &layers {
            let row = adw::ActionRow::builder()
                .use_markup(false)
                .title(&layer.name)
                .subtitle(layer_subtitle(layer, &startup, &function))
                .build();
            row.set_activatable(true);
            self.rows.append(&row);
        }
        let index = editor
            .selection
            .borrow()
            .layer
            .min(layers.len().saturating_sub(1));
        editor.selection.borrow_mut().layer = index;
        self.rows
            .select_row(self.rows.row_at_index(index as i32).as_ref());
        self.refreshing.set(false);
        self.refresh_buttons(editor);
        self.update_summaries(editor);
        let selection = editor.selection.borrow().clone();
        if selection.button.is_some() {
            self.show_selection(editor);
        }
    }

    fn refresh_buttons(&self, editor: &Rc<Editor>) {
        while let Some(child) = self.buttons.first_child() {
            self.buttons.remove(&child);
        }
        let selection = editor.selection.borrow().clone();
        let layers = editor.doc.borrow().layer_snapshot();
        if let Some(layer) = layers.get(selection.layer) {
            for (index, button) in layer.keys.iter().enumerate() {
                let row = adw::ActionRow::builder()
                    .use_markup(false)
                    .title(button_title(button))
                    .subtitle(action_summary(&button.action))
                    .activatable(true)
                    .build();
                row.add_prefix(&ui::icon(button));
                row.add_prefix(&gtk4::Label::new(Some(&(index + 1).to_string())));
                row.add_suffix(&gtk4::Image::from_icon_name("go-next-symbolic"));
                row.connect_activated(glib::clone!(
                    #[strong]
                    editor,
                    move |_| editor.select(selection.layer, Some(index))
                ));
                self.buttons.append(&row);
            }
        }
        self.buttons.select_row(
            selection
                .button
                .and_then(|index| self.buttons.row_at_index(index as i32))
                .as_ref(),
        );
    }

    pub fn show_selection(&self, editor: &Rc<Editor>) {
        self.refreshing.set(true);
        self.rows.select_row(
            self.rows
                .row_at_index(editor.selection.borrow().layer as i32)
                .as_ref(),
        );
        self.refreshing.set(false);
        self.refresh_buttons(editor);
        self.update_summaries(editor);
        let selection = editor.selection.borrow().clone();
        let layers = editor.doc.borrow().layer_snapshot();
        let Some(layer) = layers.get(selection.layer) else {
            return;
        };
        let Some(button) = selection.button.and_then(|index| layer.keys.get(index)) else {
            self.navigation.pop_to_page(&self.overview);
            return;
        };
        editor.preview.set_layer(&layer.name);
        let title = format!("{} · {}", button_title(button), layer.name);
        let content = gtk4::Box::new(gtk4::Orientation::Vertical, 18);
        ui::margins(&content, 24);
        let preview = Preview::new(&layer.name);
        preview.copy_context(&editor.preview);
        editor.previews.borrow_mut().push(Rc::downgrade(&preview));
        content.append(&crate::preview::compact(editor, &preview));
        content.append(&button_editor(
            editor,
            selection.layer,
            selection.button.unwrap(),
            button,
        ));
        // Keep this view's renderer alive for precisely the page lifetime.
        content.connect_destroy(move |_| {
            let _ = &preview;
        });
        let options = gtk4::MenuButton::builder()
            .icon_name("view-more-symbolic")
            .tooltip_text("Button Options")
            .build();
        let popover = gtk4::Popover::new();
        let actions = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
        ui::margins(&actions, 12);
        for (label, delta) in [("Move Earlier", -1), ("Move Later", 1)] {
            let action = gtk4::Button::with_label(label);
            let index = selection.button.unwrap();
            action.set_sensitive(if delta < 0 {
                index > 0
            } else {
                index + 1 < layer.keys.len()
            });
            action.connect_clicked(glib::clone!(
                #[strong]
                editor,
                #[weak]
                popover,
                move |_| {
                    popover.popdown();
                    move_button(&editor, selection.layer, index, delta);
                }
            ));
            actions.append(&action);
        }
        let remove = gtk4::Button::with_label("Delete Button");
        remove.set_sensitive(layer.keys.len() > 1);
        remove.set_tooltip_text(Some("A layer must keep at least one button"));
        remove.connect_clicked(glib::clone!(
            #[strong]
            editor,
            #[weak]
            popover,
            move |_| {
                popover.popdown();
                let mut doc = editor.doc.borrow_mut();
                let keys = &mut doc.layers()[selection.layer].keys;
                if keys.len() <= 1 {
                    return;
                }
                keys.remove(selection.button.unwrap());
                drop(doc);
                editor.selection.borrow_mut().button = None;
                editor.restructured();
                editor.select(selection.layer, None);
            }
        ));
        actions.append(&remove);
        popover.set_child(Some(&actions));
        options.set_popover(Some(&popover));
        let header = adw::HeaderBar::new();
        header.set_show_start_title_buttons(false);
        header.set_show_end_title_buttons(false);
        header.pack_end(&options);
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
        toolbar.set_content(Some(&content));
        let existing = self.detail.borrow().clone();
        let detail = if let Some(detail) = existing {
            detail.set_title(&title);
            detail.set_child(Some(&toolbar));
            detail
        } else {
            let detail = adw::NavigationPage::new(&toolbar, &title);
            detail.set_tag(Some("button"));
            self.navigation.add(&detail);
            *self.detail.borrow_mut() = Some(detail.clone());
            detail
        };
        if self.navigation.visible_page().as_ref() != Some(&detail) {
            self.navigation.push(&detail);
        }
        self.root.set_show_content(true);
    }

    /// Value edits update summaries without rebuilding focused entries.
    pub fn update_summaries(&self, editor: &Rc<Editor>) {
        let layers = editor.doc.borrow().layer_snapshot();
        let (startup, function) = editor.doc.borrow().layer_roles();
        let reachable = editor.doc.borrow().reachable_layers();
        for (index, layer) in layers.iter().enumerate() {
            if let Some(row) = self
                .rows
                .row_at_index(index as i32)
                .and_downcast::<adw::ActionRow>()
            {
                row.set_title(&layer.name);
                row.set_subtitle(&layer_subtitle(layer, &startup, &function));
            }
        }
        if let Some(layer) = layers.get(editor.selection.borrow().layer) {
            self.help.set_text(if reachable.contains(&layer.name) {
                "Choose a button to change its appearance or action."
            } else {
                "This layer is not linked from Startup or Fn. Add a button that opens it, or assign it in Behavior → Layer Switching."
            });
            self.heading.set_title(&layer.name);
            self.heading
                .set_subtitle(&layer_subtitle(layer, &startup, &function));
            self.overview.set_title(&layer.name);
            for (index, button) in layer.keys.iter().enumerate() {
                if let Some(row) = self
                    .buttons
                    .row_at_index(index as i32)
                    .and_downcast::<adw::ActionRow>()
                {
                    row.set_title(&button_title(button));
                    row.set_subtitle(&action_summary(&button.action));
                }
            }
        }
    }
}

fn layer_subtitle(
    layer: &dfr_config::LayerConfig,
    startup: &str,
    function: &Option<String>,
) -> String {
    let mut roles = Vec::new();
    if layer.name == startup {
        roles.push("Startup");
    }
    if function.as_deref() == Some(&layer.name) {
        roles.push("Fn");
    }
    let prefix = if roles.is_empty() {
        String::new()
    } else {
        format!("{} · ", roles.join(" / "))
    };
    format!("{prefix}{} authored buttons", layer.keys.len())
}

pub fn button_title(button: &dfr_config::ButtonConfig) -> String {
    if let Some(text) = &button.text {
        return text.clone();
    }
    if let Some(icon) = &button.icon {
        return match &button.action {
            dfr_config::ButtonAction::VolumeSlider => "Volume".into(),
            dfr_config::ButtonAction::BrightnessSlider => "Brightness".into(),
            _ => match icon.as_str() {
                "play_pause" => "Play/Pause".into(),
                "fast_rewind" => "Previous Track".into(),
                "fast_forward" => "Next Track".into(),
                _ => icon.replace(['_', '-'], " "),
            },
        };
    }
    button_kind(button).label().into()
}

pub fn action_summary(action: &dfr_config::ButtonAction) -> String {
    match ActionForm::of(action) {
        ActionForm::SwitchLayer(layer) => format!("Open {layer} layer"),
        ActionForm::Command(command) => format!("Run {command}"),
        ActionForm::CommandLayer { command, layer } => format!("Run {command} · then open {layer}"),
        ActionForm::Keys(keys) => keys_label(&keys),
        ActionForm::ApplyTheme(theme) => format!("Use {theme} theme"),
        ActionForm::ApplyThemeLayer { theme, layer } => {
            format!("Use {theme} theme · then open {layer}")
        }
        other => other.label().into(),
    }
}

fn new_button() -> dfr_config::ButtonConfig {
    dfr_config::ButtonConfig {
        text: Some("Button".into()),
        ..Default::default()
    }
}

fn edit_button(
    editor: &Rc<Editor>,
    layer: usize,
    index: usize,
    change: impl FnOnce(&mut dfr_config::ButtonConfig),
) {
    {
        let mut doc = editor.doc.borrow_mut();
        if let Some(button) = doc
            .layers()
            .get_mut(layer)
            .and_then(|l| l.keys.get_mut(index))
        {
            change(button);
        }
    }
    editor.run_pipeline();
}

fn move_button(editor: &Rc<Editor>, layer: usize, index: usize, delta: i32) {
    let count = editor.doc.borrow().layer_snapshot()[layer].keys.len();
    let target = index as i32 + delta;
    if target < 0 || target as usize >= count {
        return;
    }
    editor.doc.borrow_mut().layers()[layer]
        .keys
        .swap(index, target as usize);
    editor.selection.borrow_mut().button = Some(target as usize);
    editor.restructured();
}

fn layer_options(editor: &Rc<Editor>, parent: &gtk4::Button) {
    let index = editor.selection.borrow().layer;
    let layers = editor.doc.borrow().layer_snapshot();
    let Some(layer) = layers.get(index) else {
        return;
    };
    let dialog = adw::Dialog::builder()
        .title("Layer Options")
        .content_width(480)
        .content_height(520)
        .build();
    let page = adw::PreferencesPage::new();
    let group = ui::group("Layer Name");
    let name = ui::entry("Name", &layer.name);
    name.set_show_apply_button(true);
    let feedback = adw::ActionRow::builder()
        .use_markup(false)
        .title("Rename updates all layer links together")
        .build();
    name.connect_apply(glib::clone!(
        #[strong]
        editor,
        #[weak]
        feedback,
        move |entry| {
            let result = editor.doc.borrow_mut().rename_layer(index, &entry.text());
            match result {
                Ok(()) => {
                    feedback.set_title("Renamed");
                    editor.restructured();
                    editor.select(index, None);
                }
                Err(error) => feedback.set_title(&error.to_string()),
            }
        }
    ));
    group.add(&name);
    group.add(&feedback);
    page.add(&group);
    let assignments = ui::group("Assignments");
    let link = adw::ActionRow::builder()
        .use_markup(false)
        .title("Startup and Fn Layers")
        .subtitle("Choose them in Behavior → Layer Switching")
        .activatable(true)
        .build();
    link.add_suffix(&gtk4::Image::from_icon_name("go-next-symbolic"));
    link.connect_activated(glib::clone!(
        #[weak]
        dialog,
        move |row| {
            let _ = row.activate_action("win.show-behavior", None);
            dialog.close();
        }
    ));
    assignments.add(&link);
    page.add(&assignments);
    let order = ui::group("Order");
    for (title, delta) in [("Move Earlier", -1), ("Move Later", 1)] {
        let row = adw::ActionRow::builder()
            .use_markup(false)
            .title(title)
            .activatable(true)
            .build();
        let target = index as i32 + delta;
        row.set_sensitive(target >= 0 && (target as usize) < layers.len());
        row.connect_activated(glib::clone!(
            #[strong]
            editor,
            #[weak]
            dialog,
            move |_| {
                editor
                    .doc
                    .borrow_mut()
                    .layers()
                    .swap(index, target as usize);
                editor.selection.borrow_mut().layer = target as usize;
                editor.restructured();
                editor.select(target as usize, None);
                dialog.close();
            }
        ));
        order.add(&row);
    }
    page.add(&order);
    let delete = ui::group("Delete Layer");
    let links = editor.doc.borrow().incoming_links(index);
    let (startup, function) = editor.doc.borrow().layer_roles();
    let needs_repair =
        startup == layer.name || function.as_deref() == Some(&layer.name) || !links.is_empty();
    delete.set_description(Some(if needs_repair {
        "This layer has assignments or incoming links. Choose where they should go before deleting it."
    } else { "This removes the layer and its buttons from the draft." }));
    for (from, button, name) in links {
        let row = adw::ActionRow::builder()
            .use_markup(false)
            .title(format!("{name} · button {}", button + 1))
            .subtitle("Open the button that links here")
            .activatable(true)
            .build();
        row.connect_activated(glib::clone!(
            #[strong]
            editor,
            #[weak]
            dialog,
            move |_| {
                dialog.close();
                editor.select(from, Some(button));
            }
        ));
        delete.add(&row);
    }
    let mut replacements = vec!["Choose…".to_owned()];
    replacements.extend(
        layers
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != index)
            .map(|(_, l)| l.name.clone()),
    );
    let strings: Vec<&str> = replacements.iter().map(String::as_str).collect();
    let replacement = ui::combo("Move Links and Assignments To", &strings, 0);
    if needs_repair {
        delete.add(&replacement);
    }
    let remove = gtk4::Button::with_label("Delete Layer");
    remove.add_css_class("destructive-action");
    remove.set_sensitive(layers.len() > 1 && !needs_repair);
    let can_delete = layers.len() > 1;
    replacement.connect_selected_notify(glib::clone!(
        #[weak]
        remove,
        move |row| remove.set_sensitive(can_delete && row.selected() > 0)
    ));
    let remove_row = adw::ActionRow::builder()
        .use_markup(false)
        .title("Delete This Layer")
        .build();
    remove.set_valign(gtk4::Align::Center);
    remove_row.add_suffix(&remove);
    delete.add(&remove_row);
    page.add(&delete);
    remove.connect_clicked(glib::clone!(
        #[strong]
        editor,
        #[weak]
        dialog,
        move |button| {
            let target = if needs_repair {
                replacements
                    .get(replacement.selected() as usize)
                    .filter(|_| replacement.selected() > 0)
                    .map(String::as_str)
            } else {
                None
            };
            let result = editor.doc.borrow_mut().delete_layer(index, target);
            match result {
                Ok(()) => {
                    let layer = index.saturating_sub(1);
                    *editor.selection.borrow_mut() = Selection {
                        layer,
                        button: None,
                    };
                    editor.restructured();
                    editor.select(layer, None);
                    dialog.close();
                }
                Err(error) => ui::error_dialog(button, "Cannot Delete Layer", &error.to_string()),
            }
        }
    ));
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(&page));
    dialog.set_child(Some(&toolbar));
    dialog.present(Some(parent));
}

fn button_editor(
    editor: &Rc<Editor>,
    layer: usize,
    index: usize,
    button: &dfr_config::ButtonConfig,
) -> adw::PreferencesPage {
    let page = adw::PreferencesPage::new();
    let appearance = ui::group("Appearance");
    let kinds: Vec<&str> = ButtonKind::ALL.iter().map(|kind| kind.label()).collect();
    let kind = button_kind(button);
    let content = ui::combo(
        "Content",
        &kinds,
        ButtonKind::ALL.iter().position(|k| *k == kind).unwrap_or(0) as u32,
    );
    appearance.add(&content);
    content.connect_selected_notify(glib::clone!(
        #[strong]
        editor,
        move |row| {
            edit_button(&editor, layer, index, |button| {
                set_button_kind(button, ButtonKind::ALL[row.selected() as usize])
            });
            editor.restructured();
        }
    ));
    match kind {
        ButtonKind::Text => {
            let entry = ui::entry("Label", button.text.as_deref().unwrap_or_default());
            entry.connect_changed(glib::clone!(
                #[strong]
                editor,
                move |row| {
                    edit_button(&editor, layer, index, |button| {
                        button.text = Some(row.text().into())
                    });
                }
            ));
            appearance.add(&entry);
        }
        ButtonKind::Icon => {
            let row = adw::ActionRow::builder()
                .use_markup(false)
                .title("Icon")
                .subtitle(button.icon.as_deref().unwrap_or("Choose an icon"))
                .activatable(true)
                .build();
            row.add_prefix(&ui::icon(button));
            row.add_suffix(&gtk4::Image::from_icon_name("go-next-symbolic"));
            row.connect_activated(glib::clone!(
                #[strong]
                editor,
                move |row| icon_chooser(&editor, row, layer, index)
            ));
            appearance.add(&row);
        }
        ButtonKind::Time => {
            let current = button.time.as_deref().unwrap_or("24hr");
            let format = ui::combo(
                "Clock Format",
                &["24 Hour", "12 Hour", "Custom"],
                match current {
                    "24hr" => 0,
                    "12hr" => 1,
                    _ => 2,
                },
            );
            let custom = ui::entry(
                "Custom Format",
                if current == "24hr" || current == "12hr" {
                    "%H:%M"
                } else {
                    current
                },
            );
            custom.set_visible(format.selected() == 2);
            format.connect_selected_notify(glib::clone!(
                #[strong]
                editor,
                #[weak]
                custom,
                move |row| {
                    custom.set_visible(row.selected() == 2);
                    let value = match row.selected() {
                        0 => "24hr".into(),
                        1 => "12hr".into(),
                        _ => custom.text().to_string(),
                    };
                    edit_button(&editor, layer, index, |button| button.time = Some(value));
                }
            ));
            custom.connect_changed(glib::clone!(
                #[strong]
                editor,
                #[weak]
                format,
                move |row| {
                    if format.selected() == 2 {
                        edit_button(&editor, layer, index, |button| {
                            button.time = Some(row.text().into())
                        });
                    }
                }
            ));
            let locale = ui::entry("Clock Locale", button.locale.as_deref().unwrap_or("POSIX"));
            locale.connect_changed(glib::clone!(
                #[strong]
                editor,
                move |row| {
                    edit_button(&editor, layer, index, |button| {
                        button.locale = Some(row.text().into())
                    });
                }
            ));
            appearance.add(&format);
            appearance.add(&custom);
            appearance.add(&locale);
        }
        ButtonKind::Battery => {
            let mode = ui::combo(
                "Battery Display",
                &["Icon and Percentage", "Icon", "Percentage"],
                match button.battery.as_deref() {
                    Some("icon") => 1,
                    Some("percentage") => 2,
                    _ => 0,
                },
            );
            mode.connect_selected_notify(glib::clone!(
                #[strong]
                editor,
                move |row| {
                    edit_button(&editor, layer, index, |button| {
                        button.battery =
                            Some(["both", "icon", "percentage"][row.selected() as usize].into())
                    });
                }
            ));
            appearance.add(&mode);
        }
        ButtonKind::Waveform | ButtonKind::Visualizer => {
            let wave = kind == ButtonKind::Waveform;
            let style = ui::combo(
                "Graph Style",
                &["Line", "Bars"],
                u32::from(
                    if wave {
                        button.waveform_style.as_deref()
                    } else {
                        button.visualizer_style.as_deref()
                    } == Some("bars"),
                ),
            );
            style.connect_selected_notify(glib::clone!(
                #[strong]
                editor,
                move |row| {
                    edit_button(&editor, layer, index, |button| {
                        let value = Some(["line", "bars"][row.selected() as usize].into());
                        if wave {
                            button.waveform_style = value;
                        } else {
                            button.visualizer_style = value;
                        }
                    });
                }
            ));
            appearance.add(&style);
        }
        _ => {}
    }
    let width = ui::spin(
        "Width in Button Spaces",
        1.0,
        u32::MAX as f64,
        button.stretch.unwrap_or(1) as f64,
    );
    width.connect_value_notify(glib::clone!(
        #[strong]
        editor,
        move |row| {
            edit_button(&editor, layer, index, |button| {
                button.stretch = Some(row.value() as usize)
            });
        }
    ));
    appearance.add(&width);
    let mut accents = vec!["Automatic"];
    accents.extend(ACCENTS);
    let accent = ui::combo(
        "Accent",
        &accents,
        button
            .accent
            .and_then(|a| ACCENTS.iter().position(|name| *name == a.name()))
            .map(|i| i + 1)
            .unwrap_or(0) as u32,
    );
    accent.connect_selected_notify(glib::clone!(
        #[strong]
        editor,
        move |row| {
            edit_button(&editor, layer, index, |button| {
                button.accent = if row.selected() == 0 {
                    None
                } else {
                    accent_from_name(ACCENTS[row.selected() as usize - 1])
                }
            });
        }
    ));
    appearance.add(&accent);
    page.add(&appearance);
    page.add(&action_editor(editor, layer, index, &button.action));
    let advanced_group = ui::group("");
    let advanced = adw::ExpanderRow::builder()
        .use_markup(false)
        .title("Advanced")
        .subtitle("Technical options for this content type")
        .build();
    if kind == ButtonKind::Icon || kind == ButtonKind::Battery {
        for (title, is_width) in [("Icon Width (px)", true), ("Icon Height (px)", false)] {
            let value = if is_width {
                button.icon_width
            } else {
                button.icon_height
            }
            .unwrap_or(40);
            let row = ui::spin(title, 8.0, 256.0, value as f64);
            row.connect_value_notify(glib::clone!(
                #[strong]
                editor,
                move |row| {
                    edit_button(&editor, layer, index, |button| {
                        if is_width {
                            button.icon_width = Some(row.value() as i32);
                        } else {
                            button.icon_height = Some(row.value() as i32);
                        }
                    });
                }
            ));
            advanced.add_row(&row);
        }
        let theme = ui::entry("Icon Theme", button.theme.as_deref().unwrap_or_default());
        theme.connect_changed(glib::clone!(
            #[strong]
            editor,
            move |row| {
                edit_button(&editor, layer, index, |button| {
                    button.theme = (!row.text().is_empty()).then(|| row.text().into())
                });
            }
        ));
        advanced.add_row(&theme);
        if kind == ButtonKind::Icon {
            let resource = ui::entry(
                "Icon Resource Name",
                button.icon.as_deref().unwrap_or_default(),
            );
            resource.connect_changed(glib::clone!(
                #[strong]
                editor,
                move |row| {
                    edit_button(&editor, layer, index, |button| {
                        button.icon = Some(row.text().into())
                    });
                }
            ));
            advanced.add_row(&resource);
        }
    }
    if kind == ButtonKind::Waveform || kind == ButtonKind::Visualizer {
        let wave = kind == ButtonKind::Waveform;
        for (title, smoothing, max) in [
            ("Motion Smoothing (ms)", true, 5000.0),
            ("Visual Delay (ms)", false, 500.0),
        ] {
            let value = match (wave, smoothing) {
                (true, true) => button.waveform_smoothing,
                (true, false) => button.waveform_lag,
                (false, true) => button.visualizer_smoothing,
                (false, false) => button.visualizer_lag,
            }
            .unwrap_or(0);
            let row = ui::spin(title, 0.0, max, value as f64);
            row.connect_value_notify(glib::clone!(
                #[strong]
                editor,
                move |row| {
                    edit_button(&editor, layer, index, |button| {
                        let value = Some(row.value() as u32);
                        match (wave, smoothing) {
                            (true, true) => button.waveform_smoothing = value,
                            (true, false) => button.waveform_lag = value,
                            (false, true) => button.visualizer_smoothing = value,
                            (false, false) => button.visualizer_lag = value,
                        }
                    });
                }
            ));
            advanced.add_row(&row);
        }
    }
    if matches!(
        kind,
        ButtonKind::Icon | ButtonKind::Battery | ButtonKind::Waveform | ButtonKind::Visualizer
    ) {
        advanced_group.add(&advanced);
        page.add(&advanced_group);
    }
    page
}

fn icon_chooser(editor: &Rc<Editor>, parent: &adw::ActionRow, layer: usize, index: usize) {
    let dialog = adw::Dialog::builder()
        .title("Choose an Icon")
        .content_width(480)
        .content_height(520)
        .build();
    let content = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    ui::margins(&content, 18);
    let search = gtk4::SearchEntry::new();
    search.set_placeholder_text(Some("Search icon names"));
    content.append(&search);
    let mut names = std::collections::BTreeSet::new();
    for directory in dfr_render::config::ICON_DIRS {
        if let Ok(files) = std::fs::read_dir(directory) {
            for file in files.flatten() {
                let path = file.path();
                if matches!(
                    path.extension().and_then(|e| e.to_str()),
                    Some("svg" | "png")
                ) {
                    if let Some(name) = path.file_stem().and_then(|s| s.to_str()) {
                        names.insert(name.to_owned());
                    }
                }
            }
        }
    }
    let strings: Vec<&str> = names.iter().map(String::as_str).collect();
    let model = gtk4::StringList::new(&strings);
    let query = Rc::new(RefCell::new(String::new()));
    let filter = gtk4::CustomFilter::new(glib::clone!(
        #[strong]
        query,
        move |object| {
            object
                .downcast_ref::<gtk4::StringObject>()
                .is_some_and(|name| {
                    name.string()
                        .to_lowercase()
                        .contains(query.borrow().as_str())
                })
        }
    ));
    let filtered = gtk4::FilterListModel::new(Some(model), Some(filter.clone()));
    let selection = gtk4::SingleSelection::new(Some(filtered.clone()));
    let factory = gtk4::SignalListItemFactory::new();
    factory.connect_bind(|_, object| {
        let item = object.downcast_ref::<gtk4::ListItem>().unwrap();
        let name = item
            .item()
            .and_downcast::<gtk4::StringObject>()
            .unwrap()
            .string();
        let content = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
        ui::margins(&content, 12);
        content.append(&ui::icon(&dfr_config::ButtonConfig {
            icon: Some(name.to_string()),
            ..Default::default()
        }));
        let label = gtk4::Label::new(Some(&name));
        label.set_xalign(0.0);
        label.set_hexpand(true);
        content.append(&label);
        item.set_child(Some(&content));
    });
    let list = gtk4::ListView::new(Some(selection), Some(factory));
    list.add_css_class("boxed-list");
    list.connect_activate(glib::clone!(
        #[strong]
        editor,
        #[weak]
        dialog,
        #[strong]
        filtered,
        move |_, position| {
            let Some(name) = filtered.item(position).and_downcast::<gtk4::StringObject>() else {
                return;
            };
            edit_button(&editor, layer, index, |button| {
                button.icon = Some(name.string().into());
                button.theme = None;
            });
            editor.restructured();
            dialog.close();
        }
    ));
    search.connect_search_changed(glib::clone!(
        #[strong]
        query,
        #[weak]
        filter,
        move |entry| {
            *query.borrow_mut() = entry.text().to_lowercase();
            filter.changed(gtk4::FilterChange::Different);
        }
    ));
    content.append(&ui::scroll(&list));
    let help = gtk4::Label::new(Some(
        "Use Advanced → Icon Resource Name and Icon Theme for XDG theme icons.",
    ));
    help.set_wrap(true);
    help.add_css_class("dim-label");
    content.append(&help);
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(&content));
    dialog.set_child(Some(&toolbar));
    dialog.present(Some(parent));
}
/// Retarget the layer a layer-bearing action points at, keeping the
/// rest of the action intact.
fn patch_layer_target(action: &dfr_config::ButtonAction, layer: &str) -> ActionForm {
    match &ActionForm::of(action) {
        ActionForm::SwitchLayer(_) => ActionForm::SwitchLayer(layer.to_owned()),
        ActionForm::CommandLayer { command, .. } => ActionForm::CommandLayer {
            command: command.clone(),
            layer: layer.to_owned(),
        },
        ActionForm::ApplyThemeLayer { theme, .. } => ActionForm::ApplyThemeLayer {
            theme: theme.clone(),
            layer: layer.to_owned(),
        },
        other => other.clone(),
    }
}

impl ActionForm {
    /// The shell command this form runs, if any.
    fn command(&self) -> Option<&str> {
        match self {
            Self::Command(command) | Self::CommandLayer { command, .. } => Some(command),
            _ => None,
        }
    }

    /// The layer this form switches to, if any.
    fn layer_target(&self) -> Option<&str> {
        match self {
            Self::SwitchLayer(layer) => Some(layer),
            Self::CommandLayer { layer, .. } => Some(layer),
            Self::ApplyThemeLayer { layer, .. } => Some(layer),
            _ => None,
        }
    }
}

fn keys_label(keys: &[Key]) -> String {
    if keys.is_empty() {
        "no keys captured".into()
    } else {
        keys.iter()
            .map(|key| key_name(*key))
            .collect::<Vec<_>>()
            .join(" + ")
    }
}

fn patch_command(action: &dfr_config::ButtonAction, command: &str) -> ActionForm {
    match &ActionForm::of(action) {
        ActionForm::Command(_) => ActionForm::Command(command.to_owned()),
        ActionForm::CommandLayer { layer, .. } => ActionForm::CommandLayer {
            command: command.to_owned(),
            layer: layer.clone(),
        },
        other => other.clone(),
    }
}

fn patch_theme(action: &dfr_config::ButtonAction, theme: &str) -> ActionForm {
    match &ActionForm::of(action) {
        ActionForm::ApplyTheme(_) => ActionForm::ApplyTheme(theme.to_owned()),
        ActionForm::ApplyThemeLayer { layer, .. } => ActionForm::ApplyThemeLayer {
            theme: theme.to_owned(),
            layer: layer.clone(),
        },
        other => other.clone(),
    }
}

fn action_editor(
    editor: &Rc<Editor>,
    layer: usize,
    index: usize,
    action: &dfr_config::ButtonAction,
) -> adw::PreferencesGroup {
    let group = ui::group("Action");
    let forms = [
        "None",
        "Keyboard Shortcut",
        "Open Layer",
        "Run Command",
        "Run Command, Then Open Layer",
        "Use Theme",
        "Use Theme, Then Open Layer",
        "Volume Slider",
        "Brightness Slider",
        "Cycle Themes",
    ];
    let form = ActionForm::of(action);
    let selected = match form {
        ActionForm::None => 0,
        ActionForm::Keys(_) => 1,
        ActionForm::SwitchLayer(_) => 2,
        ActionForm::Command(_) => 3,
        ActionForm::CommandLayer { .. } => 4,
        ActionForm::ApplyTheme(_) => 5,
        ActionForm::ApplyThemeLayer { .. } => 6,
        ActionForm::VolumeSlider => 7,
        ActionForm::BrightnessSlider => 8,
        ActionForm::CycleTheme => 9,
    };
    let row = ui::combo("When Pressed", &forms, selected);
    group.add(&row);
    let fields = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    // PreferencesGroup accepts ordinary children, so each conditional field remains a native row.
    group.add(&fields);
    rebuild_action_fields(&fields, editor, layer, index, &form);
    row.connect_selected_notify(glib::clone!(
        #[strong]
        editor,
        #[weak]
        fields,
        move |row| {
            let form = match row.selected() {
                0 => ActionForm::None,
                1 => ActionForm::Keys(Vec::new()),
                2 => ActionForm::SwitchLayer(String::new()),
                3 => ActionForm::Command(String::new()),
                4 => ActionForm::CommandLayer {
                    command: String::new(),
                    layer: String::new(),
                },
                5 => ActionForm::ApplyTheme(String::new()),
                6 => ActionForm::ApplyThemeLayer {
                    theme: String::new(),
                    layer: String::new(),
                },
                7 => ActionForm::VolumeSlider,
                8 => ActionForm::BrightnessSlider,
                _ => ActionForm::CycleTheme,
            };
            edit_button(&editor, layer, index, |button| {
                button.action = form.to_action()
            });
            // Rebuild only the dependent action fields; preserve the choice widget and focus.
            rebuild_action_fields(&fields, &editor, layer, index, &form);
        }
    ));
    group
}

fn rebuild_action_fields(
    fields: &gtk4::Box,
    editor: &Rc<Editor>,
    layer: usize,
    index: usize,
    form: &ActionForm,
) {
    ui::clear(fields);
    let group = ui::group("");
    add_action_fields(&group, editor, layer, index, form);
    fields.append(&group);
}

fn add_action_fields(
    group: &adw::PreferencesGroup,
    editor: &Rc<Editor>,
    layer: usize,
    index: usize,
    form: &ActionForm,
) {
    if let ActionForm::Keys(keys) = form {
        let row = adw::ActionRow::builder()
            .use_markup(false)
            .title("Keyboard Shortcut")
            .subtitle(keys_label(keys))
            .build();
        let capture = gtk4::ToggleButton::with_label("Record");
        let finish = gtk4::Button::with_label("Finish");
        let cancel = gtk4::Button::with_label("Cancel");
        for button in [
            capture.clone().upcast::<gtk4::Widget>(),
            finish.clone().upcast(),
            cancel.clone().upcast(),
        ] {
            button.set_valign(gtk4::Align::Center);
            row.add_suffix(&button);
        }
        finish.set_visible(false);
        cancel.set_visible(false);
        let captured = Rc::new(RefCell::new(Vec::<Key>::new()));
        let original = keys_label(keys);
        capture.connect_toggled(glib::clone!(
            #[weak]
            row,
            #[weak]
            finish,
            #[weak]
            cancel,
            #[strong]
            captured,
            move |toggle| {
                finish.set_visible(toggle.is_active());
                cancel.set_visible(toggle.is_active());
                if toggle.is_active() {
                    captured.borrow_mut().clear();
                    toggle.set_label("Listening…");
                    toggle.grab_focus();
                    row.set_subtitle("Press your shortcut, then choose Finish or Cancel");
                } else {
                    toggle.set_label("Record");
                }
            }
        ));
        let controller = gtk4::EventControllerKey::new();
        controller.set_propagation_phase(gtk4::PropagationPhase::Capture);
        controller.connect_key_pressed(glib::clone!(
            #[weak]
            capture,
            #[weak]
            row,
            #[strong]
            captured,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |_, _, keycode, _| {
                if !capture.is_active() {
                    return glib::Propagation::Proceed;
                }
                if let Some(key) = key_from_gdk(keycode) {
                    let mut keys = captured.borrow_mut();
                    if !keys.contains(&key) {
                        keys.push(key);
                    }
                    row.set_subtitle(&keys_label(&keys));
                }
                glib::Propagation::Stop
            }
        ));
        capture.add_controller(controller);
        finish.connect_clicked(glib::clone!(
            #[strong]
            editor,
            #[weak]
            capture,
            #[weak]
            row,
            #[strong]
            captured,
            move |_| {
                let keys = captured.borrow().clone();
                capture.set_active(false);
                row.set_subtitle(&keys_label(&keys));
                edit_button(&editor, layer, index, |button| {
                    button.action = ActionForm::Keys(keys).to_action()
                });
            }
        ));
        cancel.connect_clicked(glib::clone!(
            #[weak]
            capture,
            #[weak]
            row,
            move |_| {
                capture.set_active(false);
                row.set_subtitle(&original);
            }
        ));
        group.add(&row);
    }
    if let Some(current) = form.layer_target() {
        let doc = editor.doc.borrow();
        let mut names: Vec<String> = vec!["Choose…".into()];
        names.extend(doc.layer_snapshot().into_iter().map(|l| l.name));
        let effective = doc.effective().unwrap();
        if effective.pet_enabled.unwrap_or(true) && !names.iter().any(|n| n == "pet") {
            names.push("pet".into());
        }
        if effective.themes_enabled.unwrap_or(true) && !names.iter().any(|n| n == "themes") {
            names.push("themes".into());
        }
        if !current.is_empty() && !names.iter().any(|n| n == current) {
            names.push(current.into());
        }
        drop(doc);
        let strings: Vec<&str> = names.iter().map(String::as_str).collect();
        let row = ui::combo(
            "Open Layer",
            &strings,
            names.iter().position(|n| n == current).unwrap_or(0) as u32,
        );
        if current.is_empty() {
            row.set_subtitle("Choose a destination to complete this action");
        }
        row.connect_selected_notify(glib::clone!(
            #[strong]
            editor,
            move |row| {
                let target = if row.selected() == 0 {
                    ""
                } else {
                    &names[row.selected() as usize]
                };
                edit_button(&editor, layer, index, |button| {
                    button.action = patch_layer_target(&button.action, target).to_action()
                });
                row.set_subtitle(if target.is_empty() {
                    "Choose a destination to complete this action"
                } else {
                    ""
                });
            }
        ));
        group.add(&row);
    }
    if let Some(command) = form.command() {
        let row = ui::entry("Command", command);
        row.connect_changed(glib::clone!(
            #[strong]
            editor,
            move |row| {
                edit_button(&editor, layer, index, |button| {
                    button.action = patch_command(&button.action, &row.text()).to_action()
                });
            }
        ));
        group.add(&row);
    }
    if let ActionForm::ApplyTheme(current) | ActionForm::ApplyThemeLayer { theme: current, .. } =
        form
    {
        let mut names = vec!["Choose…".to_owned()];
        names.extend(editor.doc.borrow().known_themes());
        if !current.is_empty() && !names.contains(current) {
            names.push(current.clone());
        }
        let strings: Vec<&str> = names.iter().map(String::as_str).collect();
        let row = ui::combo(
            "Color Theme",
            &strings,
            names.iter().position(|n| n == current).unwrap_or(0) as u32,
        );
        row.connect_selected_notify(glib::clone!(
            #[strong]
            editor,
            move |row| {
                let target = if row.selected() == 0 {
                    ""
                } else {
                    &names[row.selected() as usize]
                };
                edit_button(&editor, layer, index, |button| {
                    button.action = patch_theme(&button.action, target).to_action()
                });
            }
        ));
        group.add(&row);
    }
}
