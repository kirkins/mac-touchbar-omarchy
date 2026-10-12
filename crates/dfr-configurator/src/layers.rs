//! The Deck page: the layer list, the per-layer button grid, and the
//! button editor with the action editor and its key-combo capture.
//! Every edit writes through to the document model; nothing here
//! touches a file until Save.

use crate::capture::{key_from_gdk, key_name};
use crate::model::{button_kind, set_button_kind, ActionForm, ButtonKind, Document};
use crate::preview::Preview;
use gtk4::{glib, prelude::*};
use input_linux::Key;
use std::cell::RefCell;
use std::rc::Rc;

pub const ACCENTS: &[&str] = &[
    "neutral", "cyan", "amber", "blue", "violet", "orange", "red", "green",
];

/// Shared editing context handed to every callback. The page's widgets
/// hold `Rc<Editor>`s; the editor holds no widgets, so teardown drops
/// everything with the window. The window installs the two hooks:
/// `notify` runs after every mutation (validation, status, preview
/// debounce) and the structure hook rebuilds pages after edits that
/// change the deck's shape (add/remove/reorder, kind switches).
pub struct Editor {
    pub doc: RefCell<Document>,
    pub selection: RefCell<Selection>,
    after_edit: RefCell<Option<Box<dyn Fn(Rc<Editor>)>>>,
    on_structure: RefCell<Option<Box<dyn Fn(Rc<Editor>)>>>,
    pub preview: Rc<Preview>,
}

impl Editor {
    pub fn new(doc: Document, preview: Rc<Preview>) -> Rc<Self> {
        Rc::new(Self {
            doc: RefCell::new(doc),
            selection: RefCell::new(Selection { layer: 0, button: None }),
            after_edit: RefCell::new(None),
            on_structure: RefCell::new(None),
            preview,
        })
    }

    pub fn set_after_edit(&self, callback: Box<dyn Fn(Rc<Editor>)>) {
        *self.after_edit.borrow_mut() = Some(callback);
    }

    pub fn set_on_structure(&self, callback: Box<dyn Fn(Rc<Editor>)>) {
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

impl Editor {
    /// Read the selected button's clone (for building editors).
    fn selected_button(&self) -> Option<dfr_config::ButtonConfig> {
        let selection = self.selection.borrow().clone();
        let mut doc = self.doc.borrow_mut();
        doc.layers()
            .get(selection.layer)?
            .keys
            .get(selection.button?)
            .cloned()
    }

    /// Mutate the selected button through one borrow, then notify.
    fn edit_selected_button(self: &Rc<Self>, change: impl FnOnce(&mut dfr_config::ButtonConfig)) {
        let selection = self.selection.borrow().clone();
        {
            let mut doc = self.doc.borrow_mut();
            let button = doc
                .layers_mut()
                .get_mut(selection.layer)
                .and_then(|layer| layer.keys.get_mut(selection.button?));
            if let Some(button) = button {
                change(button);
            }
        }
        self.run_pipeline();
    }

    /// Mutate the selected layer through one borrow, then notify.
    fn edit_selected_layer(self: &Rc<Self>, change: impl FnOnce(&mut dfr_config::LayerConfig)) {
        let selection = self.selection.borrow().clone();
        {
            let mut doc = self.doc.borrow_mut();
            if let Some(layer) = doc.layers_mut().get_mut(selection.layer) {
                change(layer);
            }
        }
        self.run_pipeline();
    }
}

/// Document-level layer mutation helper (the page re-borrows the
/// overlay's layer list each edit, migrating legacy documents lazily).
trait LayersMut {
    fn layers_mut(&mut self) -> &mut Vec<dfr_config::LayerConfig>;
}

impl LayersMut for Document {
    fn layers_mut(&mut self) -> &mut Vec<dfr_config::LayerConfig> {
        self.layers()
    }
}

pub struct LayersPage {
    pub root: gtk4::Widget,
    layer_rows: gtk4::ListBox,
    layer_header: gtk4::Box,
    buttons_box: gtk4::Box,
    button_editor: gtk4::Box,
}

impl LayersPage {
    pub fn build(editor: &Rc<Editor>) -> Self {
        let paned = gtk4::Paned::new(gtk4::Orientation::Horizontal);
        paned.set_position(300);
        paned.set_shrink_start_child(false);
        paned.set_shrink_end_child(false);

        // ---- Left: the layer list ---------------------------------
        let left = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
        let title = gtk4::Label::new(Some("Layers"));
        title.set_halign(gtk4::Align::Start);
        title.add_css_class("title-4");
        left.append(&title);

        let rows = gtk4::ListBox::new();
        rows.set_css_classes(&["boxed-list"]);
        rows.set_selection_mode(gtk4::SelectionMode::Single);
        let scrolled = gtk4::ScrolledWindow::new();
        scrolled.set_child(Some(&rows));
        scrolled.set_vexpand(true);
        scrolled.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Automatic);
        left.append(&scrolled);

        let add = gtk4::Button::with_label("Add layer");
        add.add_css_class("suggested-action");
        left.append(&add);

        // ---- Right: preview + layer header + button grid + editor ---
        let right = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
        right.set_margin_top(6);
        right.set_margin_bottom(6);
        right.set_margin_start(6);
        right.set_margin_end(6);

        let preview_frame = gtk4::Frame::new(None);
        let preview_box = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
        let preview_controls = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        preview_controls.append(&gtk4::Label::new(Some("Theme:")));
        let themes = gtk4::DropDown::from_strings(&["Active"]);
        themes.set_valign(gtk4::Align::Center);
        let width_toggle = gtk4::CheckButton::with_label("2170 px bar (adds Esc)");
        width_toggle.set_valign(gtk4::Align::Center);
        preview_controls.append(&themes);
        preview_controls.append(&width_toggle);
        preview_box.append(&preview_controls);
        preview_box.append(&editor.preview.area);
        preview_frame.set_child(Some(&preview_box));
        right.append(&preview_frame);

        let layer_header = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        right.append(&layer_header);

        let buttons_label = gtk4::Label::new(Some("Buttons"));
        buttons_label.set_halign(gtk4::Align::Start);
        buttons_label.add_css_class("heading");
        right.append(&buttons_label);
        let buttons_scrolled = gtk4::ScrolledWindow::new();
        buttons_scrolled.set_policy(gtk4::PolicyType::Automatic, gtk4::PolicyType::Never);
        buttons_scrolled.set_vexpand(true);
        let buttons_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        buttons_box.set_margin_top(4);
        buttons_scrolled.set_child(Some(&buttons_box));
        right.append(&buttons_scrolled);

        let editor_label = gtk4::Label::new(Some("Button editor"));
        editor_label.set_halign(gtk4::Align::Start);
        editor_label.add_css_class("heading");
        let button_editor = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
        right.append(&editor_label);
        right.append(&button_editor);

        paned.set_start_child(Some(&left));
        paned.set_end_child(Some(&right));

        let page = Self {
            root: paned.upcast(),
            layer_rows: rows,
            layer_header,
            buttons_box,
            button_editor,
        };

        // ---- wiring ------------------------------------------------
        page.layer_rows.connect_row_selected(
            glib::clone!(#[strong] editor, move |_, row| {
                let Some(row) = row else { return };
                let index = row.index() as usize;
                let name = editor
                    .doc
                    .borrow_mut()
                    .layers()
                    .get(index)
                    .map(|layer| layer.name.clone());
                if let Some(name) = name {
                    *editor.selection.borrow_mut() = Selection { layer: index, button: None };
                    editor.preview.set_layer(&name);
                    editor.run_pipeline();
                }
            }),
        );

        add.connect_clicked(glib::clone!(#[strong] editor, move |_| {
            let name = {
                let mut doc = editor.doc.borrow_mut();
                let layers = doc.layers_mut();
                let mut n = layers.len() + 1;
                while layers.iter().any(|layer| layer.name == format!("layer-{n}")) {
                    n += 1;
                }
                let name = format!("layer-{n}");
                layers.push(dfr_config::LayerConfig {
                    name: name.clone(),
                    keys: vec![dfr_config::ButtonConfig {
                        text: Some("Button".into()),
                        ..Default::default()
                    }],
                });
                name
            };
            let count = editor.doc.borrow_mut().layers().len();
            *editor.selection.borrow_mut() = Selection { layer: count - 1, button: None };
            editor.preview.set_layer(&name);
            editor.restructured();
        }));

        themes.connect_selected_notify(
            glib::clone!(#[strong] editor, #[strong] themes, move |_| {
                let Some(item) = themes.selected_item().and_downcast::<gtk4::StringObject>() else {
                    return;
                };
                let name = item.string().to_string();
                if name == "Active" {
                    return;
                }
                // Choosing a theme in the preview pins it in the
                // document, exactly like writing `ActiveTheme` by hand.
                editor.doc.borrow_mut().user_mut().active_theme = Some(name);
                editor.run_pipeline();
            }),
        );

        width_toggle.connect_toggled(
            glib::clone!(#[strong] editor, move |toggle| {
                editor
                    .preview
                    .set_width(if toggle.is_active() { 2170 } else { 2008 });
                editor.preview.render(&editor.doc.borrow());
            }),
        );

        page
    }

    /// Rebuild the page from the document (structural changes; value
    /// widgets are rebuilt with their rows, which is plenty fast at
    /// deck sizes).
    pub fn refresh(&self, editor: &Rc<Editor>) {
        let (layers, default_layer, fn_layer, reachable) = {
            let mut doc = editor.doc.borrow_mut();
            (
                doc.layers().clone(),
                doc.user().default_layer.clone(),
                doc.user().fn_layer.clone(),
                doc.reachable_layers(),
            )
        };

        let selected_name = editor
            .selection
            .borrow()
            .layer
            .checked_sub(0)
            .and_then(|i| layers.get(i).map(|l| l.name.clone()));

        let mut child = self.layer_rows.first_child();
        while let Some(row) = child {
            child = row.next_sibling();
            self.layer_rows.remove(&row);
        }
        for layer in &layers {
            self.layer_rows.append(&layer_row(editor, layer, &default_layer, &fn_layer, &reachable));
        }
        // Restore selection by name so reorders keep their target.
        let mut selected = false;
        if let Some(name) = &selected_name {
            for (index, layer) in layers.iter().enumerate() {
                if &layer.name == name {
                    if let Some(row) = self.layer_rows.row_at_index(index as i32) {
                        self.layer_rows.select_row(Some(&row));
                        selected = true;
                    }
                }
            }
        }
        if !selected {
            let index = editor.selection.borrow().layer.min(layers.len().saturating_sub(1));
            *editor.selection.borrow_mut() = Selection { layer: index, button: None };
            if let Some(layer) = layers.get(index) {
                editor.preview.set_layer(&layer.name);
            }
            if let Some(row) = self.layer_rows.row_at_index(index as i32) {
                self.layer_rows.select_row(Some(&row));
            }
        }

        self.refresh_layer_header(editor);
        self.refresh_buttons(editor, &layers);
    }

    fn refresh_layer_header(&self, editor: &Rc<Editor>) {
        let mut child = self.layer_header.first_child();
        while let Some(widget) = child {
            child = widget.next_sibling();
            self.layer_header.remove(&widget);
        }
        let selection = editor.selection.borrow().clone();
        let Some(layer_index) = selection.layer.checked_sub(0) else {
            return;
        };
        let Some(layer) = editor.doc.borrow_mut().layers().get(layer_index).cloned() else {
            return;
        };

        self.layer_header.append(&gtk4::Label::new(Some("Layer name:")));
        let name = gtk4::Entry::new();
        name.set_text(&layer.name);
        name.set_hexpand(true);
        self.layer_header.append(&name);
        let default_toggle = gtk4::CheckButton::with_label("Default layer");
        default_toggle.set_active(
            editor
                .doc
                .borrow()
                .user()
                .default_layer
                .as_deref()
                .is_some_and(|n| n == layer.name),
        );
        let fn_toggle = gtk4::CheckButton::with_label("Fn layer");
        fn_toggle.set_active(
            editor
                .doc
                .borrow()
                .user()
                .fn_layer
                .as_deref()
                .is_some_and(|n| n == layer.name),
        );
        let remove = gtk4::Button::from_icon_name("user-trash-symbolic");
        remove.add_css_class("destructive-action");
        remove.set_tooltip_text(Some("Delete this layer"));
        self.layer_header.append(&default_toggle);
        self.layer_header.append(&fn_toggle);
        self.layer_header.append(&remove);

        name.connect_changed(glib::clone!(#[strong] editor, move |entry| {
            let new = entry.text().to_string();
            if new.is_empty() {
                return;
            }
            editor.edit_selected_layer(|layer| {
                if layer.name != new {
                    layer.name = new.clone();
                }
            });
            editor.preview.set_layer(&new);
        }));

        default_toggle.connect_toggled(glib::clone!(#[strong] editor, move |toggle| {
            let name = editor.selected_button_layer_name();
            if let Some(name) = name {
                let pinned = toggle.is_active().then_some(name);
                editor.doc.borrow_mut().user_mut().default_layer = pinned;
                editor.restructured();
            }
        }));

        fn_toggle.connect_toggled(glib::clone!(#[strong] editor, move |toggle| {
            let name = editor.selected_button_layer_name();
            if let Some(name) = name {
                let pinned = toggle.is_active().then_some(name);
                editor.doc.borrow_mut().user_mut().fn_layer = pinned;
                editor.restructured();
            }
        }));

        remove.connect_clicked(glib::clone!(#[strong] editor, move |_| {
            let selection = editor.selection.borrow().clone();
            let name = editor
                .doc
                .borrow_mut()
                .layers()
                .get(selection.layer)
                .map(|layer| layer.name.clone());
            let Some(name) = name else { return };
            {
                let mut doc = editor.doc.borrow_mut();
                doc.layers_mut().remove(selection.layer);
                // References to the removed layer go with it.
                if doc.user().default_layer.as_deref() == Some(name.as_str()) {
                    let fallback = doc.layers().first().map(|l| l.name.clone());
                    doc.user_mut().default_layer = fallback;
                }
                if doc.user().fn_layer.as_deref() == Some(name.as_str()) {
                    doc.user_mut().fn_layer = None;
                }
            }
            *editor.selection.borrow_mut() = Selection {
                layer: selection.layer.saturating_sub(1),
                button: None,
            };
            editor.restructured();
        }));
    }

    fn refresh_buttons(&self, editor: &Rc<Editor>, layers: &[dfr_config::LayerConfig]) {
        let mut child = self.buttons_box.first_child();
        while let Some(widget) = child {
            child = widget.next_sibling();
            self.buttons_box.remove(&widget);
        }
        let selection = editor.selection.borrow().clone();
        let Some(layer_index) = selection.layer.checked_sub(0) else {
            return;
        };
        let Some(layer) = layers.get(layer_index) else {
            return;
        };

        for (button_index, button) in layer.keys.iter().enumerate() {
            self.buttons_box
                .append(&button_card(editor, layer_index, button_index, button));
        }
        let add = gtk4::Button::from_icon_name("list-add-symbolic");
        add.set_tooltip_text(Some("Add a button"));
        add.set_valign(gtk4::Align::Center);
        add.connect_clicked(glib::clone!(#[strong] editor, move |_| {
            let new_index = {
                let mut doc = editor.doc.borrow_mut();
                let layers = doc.layers_mut();
                layers
                    .get_mut(layer_index)
                    .map(|layer| {
                        layer.keys.push(dfr_config::ButtonConfig {
                            text: Some("Button".into()),
                            ..Default::default()
                        });
                        layer.keys.len() - 1
                    })
                    .unwrap_or(0)
            };
            *editor.selection.borrow_mut() = Selection {
                layer: layer_index,
                button: Some(new_index),
            };
            editor.restructured();
        }));
        self.buttons_box.append(&add);

        self.refresh_button_editor(editor, layers);
    }

    fn refresh_button_editor(&self, editor: &Rc<Editor>, layers: &[dfr_config::LayerConfig]) {
        let mut child = self.button_editor.first_child();
        while let Some(widget) = child {
            child = widget.next_sibling();
            self.button_editor.remove(&widget);
        }
        let selection = editor.selection.borrow().clone();
        let Some(layer) = selection.layer.checked_sub(0).and_then(|i| layers.get(i)) else {
            self.button_editor
                .append(&gtk4::Label::new(Some("Select a button to edit it.")));
            return;
        };
        let Some(button_index) = selection.button else {
            self.button_editor
                .append(&gtk4::Label::new(Some("Select a button to edit it.")));
            return;
        };
        let Some(button) = layer.keys.get(button_index) else {
            return;
        };
        self.button_editor
            .append(&button_editor(editor, selection.layer, button_index, button));
    }
}


/// Mutate one identified button through a single borrow, then notify.
fn edit_button(
    editor: &Rc<Editor>,
    layer_index: usize,
    button_index: usize,
    change: impl FnOnce(&mut dfr_config::ButtonConfig),
) {
    {
        let mut doc = editor.doc.borrow_mut();
        let target = doc
            .layers_mut()
            .get_mut(layer_index)
            .and_then(|layer| layer.keys.get_mut(button_index));
        if let Some(button) = target {
            change(button);
        }
    }
    editor.run_pipeline();
}

impl Editor {
    fn selected_button_layer_name(&self) -> Option<String> {
        let selection = self.selection.borrow().clone();
        self.doc
            .borrow_mut()
            .layers()
            .get(selection.layer)
            .map(|layer| layer.name.clone())
    }
}

/// The accent a wire name names (the same lowercase spelling the
/// configuration's enum accepts).
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

fn badge(text: &str, css: &str) -> gtk4::Widget {
    let label = gtk4::Label::new(Some(text));
    label.add_css_class("badge");
    label.add_css_class(css);
    label.set_valign(gtk4::Align::Center);
    label.upcast()
}

/// One layer's row: name, badges, and reordering.
fn layer_row(
    editor: &Rc<Editor>,
    layer: &dfr_config::LayerConfig,
    default_layer: &Option<String>,
    fn_layer: &Option<String>,
    reachable: &[String],
) -> gtk4::Widget {
    let row = gtk4::ListBoxRow::new();
    let box_ = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    box_.set_margin_top(6);
    box_.set_margin_bottom(6);
    box_.set_margin_start(8);
    box_.set_margin_end(4);
    let label = gtk4::Label::new(Some(&layer.name));
    label.set_hexpand(true);
    label.set_halign(gtk4::Align::Start);
    label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    box_.append(&label);
    if default_layer.as_deref() == Some(layer.name.as_str()) {
        box_.append(&badge("default", "suggested-action"));
    }
    if fn_layer.as_deref() == Some(layer.name.as_str()) {
        box_.append(&badge("fn", "accent"));
    }
    if !reachable.contains(&layer.name) {
        box_.append(&badge("unreachable", "warning"));
    }
    let count = gtk4::Label::new(Some(&format!("{} buttons", layer.keys.len())));
    count.add_css_class("dim-label");
    box_.append(&count);

    let up = gtk4::Button::from_icon_name("go-up-symbolic");
    up.set_tooltip_text(Some("Move layer up"));
    let down = gtk4::Button::from_icon_name("go-down-symbolic");
    down.set_tooltip_text(Some("Move layer down"));
    up.add_css_class("flat");
    down.add_css_class("flat");
    box_.append(&up);
    box_.append(&down);
    row.set_child(Some(&box_));

    let index = editor
        .doc
        .borrow_mut()
        .layers()
        .iter()
        .position(|candidate| candidate.name == layer.name)
        .unwrap_or_default();
    up.connect_clicked(glib::clone!(#[strong] editor, move |_| {
        move_layer(&editor, index, -1);
    }));
    down.connect_clicked(glib::clone!(#[strong] editor, move |_| {
        move_layer(&editor, index, 1);
    }));
    row.upcast()
}

fn move_layer(editor: &Rc<Editor>, index: usize, delta: i32) {
    let target = index as i32 + delta;
    if target < 0 {
        return;
    }
    let target = target as usize;
    {
        let mut doc = editor.doc.borrow_mut();
        let layers = doc.layers_mut();
        if target < layers.len() {
            layers.swap(index, target);
        }
    }
    *editor.selection.borrow_mut() = Selection {
        layer: target,
        button: None,
    };
    editor.restructured();
}

/// One button's card: a summary, reordering, selection, removal.
fn button_card(
    editor: &Rc<Editor>,
    layer_index: usize,
    button_index: usize,
    button: &dfr_config::ButtonConfig,
) -> gtk4::Widget {
    let frame = gtk4::Frame::new(None);
    let box_ = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
    box_.set_margin_top(6);
    box_.set_margin_bottom(6);
    box_.set_margin_start(8);
    box_.set_margin_end(8);
    let kind = button_kind(button);
    let summary = match kind {
        ButtonKind::Text => button.text.clone().unwrap_or_default(),
        ButtonKind::Icon => format!("icon: {}", button.icon.clone().unwrap_or_default()),
        ButtonKind::Time => format!("clock ({})", button.time.clone().unwrap_or_default()),
        ButtonKind::Battery => "battery".to_owned(),
        ButtonKind::Waveform => "waveform".to_owned(),
        ButtonKind::Visualizer => "visualizer".to_owned(),
        ButtonKind::NowPlaying => "now playing".to_owned(),
        ButtonKind::Spacer => "spacer".to_owned(),
    };
    let kind_label = gtk4::Label::new(Some(kind.label()));
    kind_label.add_css_class("dim-label");
    let label = gtk4::Label::new(Some(&summary));
    label.set_width_chars(12);
    label.set_max_width_chars(14);
    label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    box_.append(&kind_label);
    box_.append(&label);
    if button.stretch.unwrap_or(1) > 1 {
        box_.append(&gtk4::Label::new(Some(&format!(
            "stretch ×{}",
            button.stretch.unwrap_or(1)
        ))));
    }
    let action_label = gtk4::Label::new(Some(ActionForm::of(&button.action).label()));
    action_label.add_css_class("dim-label");
    action_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    action_label.set_max_width_chars(14);
    box_.append(&action_label);

    let controls = gtk4::Box::new(gtk4::Orientation::Horizontal, 2);
    let left = gtk4::Button::from_icon_name("go-previous-symbolic");
    left.set_tooltip_text(Some("Move left"));
    left.add_css_class("flat");
    let right = gtk4::Button::from_icon_name("go-next-symbolic");
    right.set_tooltip_text(Some("Move right"));
    right.add_css_class("flat");
    let remove = gtk4::Button::from_icon_name("user-trash-symbolic");
    remove.set_tooltip_text(Some("Remove button"));
    remove.add_css_class("flat");
    controls.append(&left);
    controls.append(&right);
    controls.append(&remove);
    box_.append(&controls);
    frame.set_child(Some(&box_));

    let clickable = gtk4::GestureClick::new();
    clickable.connect_pressed(glib::clone!(#[strong] editor, move |_, _, _, _| {
        *editor.selection.borrow_mut() = Selection {
            layer: layer_index,
            button: Some(button_index),
        };
        editor.run_pipeline();
    }));
    frame.add_controller(clickable);

    left.connect_clicked(glib::clone!(#[strong] editor, move |_| {
        move_button(&editor, layer_index, button_index, -1);
    }));
    right.connect_clicked(glib::clone!(#[strong] editor, move |_| {
        move_button(&editor, layer_index, button_index, 1);
    }));
    remove.connect_clicked(glib::clone!(#[strong] editor, move |_| {
        {
            let mut doc = editor.doc.borrow_mut();
            if let Some(layer) = doc.layers_mut().get_mut(layer_index) {
                if button_index < layer.keys.len() {
                    layer.keys.remove(button_index);
                }
            }
        }
        *editor.selection.borrow_mut() = Selection {
            layer: layer_index,
            button: None,
        };
        editor.restructured();
    }));

    let is_selected = {
        let selection = editor.selection.borrow();
        selection.button == Some(button_index) && selection.layer == layer_index
    };
    if is_selected {
        frame.add_css_class("selected-frame");
    }
    frame.upcast()
}

fn move_button(editor: &Rc<Editor>, layer_index: usize, button_index: usize, delta: i32) {
    let target = button_index as i32 + delta;
    if target < 0 {
        return;
    }
    let target = target as usize;
    {
        let mut doc = editor.doc.borrow_mut();
        if let Some(layer) = doc.layers_mut().get_mut(layer_index) {
            if target < layer.keys.len() {
                layer.keys.swap(button_index, target);
            }
        }
    }
    *editor.selection.borrow_mut() = Selection {
        layer: layer_index,
        button: Some(target),
    };
    editor.restructured();
}

/// The full button editor: content kind, per-kind fields, size and
/// accent, and the action editor with key capture.
fn button_editor(
    editor: &Rc<Editor>,
    layer_index: usize,
    button_index: usize,
    button: &dfr_config::ButtonConfig,
) -> gtk4::Widget {
    let outer = gtk4::Box::new(gtk4::Orientation::Vertical, 10);

    // ---- content kind ----
    let kind_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    kind_row.append(&gtk4::Label::new(Some("Content:")));
    let kinds: Vec<&str> = ButtonKind::ALL.iter().map(|k| k.label()).collect();
    let kind_dropdown = gtk4::DropDown::from_strings(&kinds);
    kind_dropdown.set_selected(
        ButtonKind::ALL
            .iter()
            .position(|k| *k == button_kind(button))
            .unwrap_or(0) as u32,
    );
    kind_row.append(&kind_dropdown);
    outer.append(&kind_row);

    // ---- per-kind fields ----
    let fields = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
    outer.append(&fields);

    // ---- size and accent ----
    let size_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    size_row.append(&gtk4::Label::new(Some("Stretch:")));
    let stretch = gtk4::SpinButton::with_range(1.0, 20.0, 1.0);
    stretch.set_value(button.stretch.unwrap_or(1) as f64);
    size_row.append(&stretch);
    size_row.append(&gtk4::Label::new(Some("Accent:")));
    let mut accent_names: Vec<&str> = vec!["auto"];
    accent_names.extend_from_slice(ACCENTS);
    let accent = gtk4::DropDown::from_strings(&accent_names);
    let accent_value = button
        .accent
        .map(|a| a.name().to_owned())
        .unwrap_or_else(|| "auto".into());
    accent.set_selected(
        accent_names
            .iter()
            .position(|name| *name == accent_value)
            .unwrap_or(0) as u32,
    );
    size_row.append(&accent);
    size_row.append(&gtk4::Label::new(Some("Icon size:")));
    let icon_width = gtk4::SpinButton::with_range(8.0, 256.0, 1.0);
    icon_width.set_value(button.icon_width.unwrap_or(40) as f64);
    let icon_height = gtk4::SpinButton::with_range(8.0, 256.0, 1.0);
    icon_height.set_value(button.icon_height.unwrap_or(40) as f64);
    size_row.append(&icon_width);
    size_row.append(&gtk4::Label::new(Some("×")));
    size_row.append(&icon_height);
    outer.append(&size_row);

    // ---- action editor ----
    outer.append(&action_editor(editor, layer_index, button_index, &button.action));

    // ---- plumbing ---------------------------------------------
    rebuild_kind_fields(&fields, editor, layer_index, button_index, button_kind(button), button);

    kind_dropdown.connect_selected_notify(
        glib::clone!(#[strong] editor, #[strong] fields, #[strong] kind_dropdown, move |_| {
            let Some(index) = kind_dropdown.selected().checked_sub(0) else {
                return;
            };
            let kind = ButtonKind::ALL[index as usize];
            editor.edit_selected_button(|button| set_button_kind(button, kind));
            let snapshot = editor.selected_button();
            if let Some(button) = snapshot {
                rebuild_kind_fields(&fields, &editor, layer_index, button_index, kind, &button);
            }
            editor.restructured();
        }),
    );

    stretch.connect_value_changed(glib::clone!(#[strong] editor, move |spin| {
        let value = spin.value() as usize;
        edit_button(&editor, layer_index, button_index, |button| {
            button.stretch = (value > 1).then_some(value);
        });
    }));

    accent.connect_selected_notify(
        glib::clone!(#[strong] editor, #[strong] accent, move |_| {
            let index = accent.selected() as usize;
            let name = accent_names[index];
            edit_button(&editor, layer_index, button_index, |button| {
                button.accent = if name == "auto" {
                    None
                } else {
                    accent_from_name(name)
                };
            });
        }),
    );

    icon_width.connect_value_changed(glib::clone!(#[strong] editor, move |spin| {
        let value = spin.value() as i32;
        edit_button(&editor, layer_index, button_index, |button| {
            button.icon_width = (value != 40).then_some(value);
        });
    }));
    icon_height.connect_value_changed(glib::clone!(#[strong] editor, move |spin| {
        let value = spin.value() as i32;
        edit_button(&editor, layer_index, button_index, |button| {
            button.icon_height = (value != 40).then_some(value);
        });
    }));

    outer.upcast()
}

/// The per-kind fields for the current content kind of `button`.
fn rebuild_kind_fields(
    fields: &gtk4::Box,
    editor: &Rc<Editor>,
    layer_index: usize,
    button_index: usize,
    kind: ButtonKind,
    button: &dfr_config::ButtonConfig,
) {
    let mut child = fields.first_child();
    while let Some(widget) = child {
        child = widget.next_sibling();
        fields.remove(&widget);
    }

    match kind {
        ButtonKind::Text => {
            let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
            row.append(&gtk4::Label::new(Some("Label:")));
            let entry = gtk4::Entry::new();
            entry.set_text(button.text.as_deref().unwrap_or(""));
            entry.set_hexpand(true);
            row.append(&entry);
            fields.append(&row);
            entry.connect_changed(glib::clone!(#[strong] editor, move |entry| {
                let text = entry.text().to_string();
                edit_button(&editor, layer_index, button_index, |button| {
                    button.text = (!text.is_empty()).then(|| text.clone());
                });
            }));
        }
        ButtonKind::Icon => {
            let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
            row.append(&gtk4::Label::new(Some("Icon name:")));
            let entry = gtk4::Entry::new();
            entry.set_text(button.icon.as_deref().unwrap_or(""));
            entry.set_hexpand(true);
            row.append(&entry);
            fields.append(&row);
            entry.connect_changed(glib::clone!(#[strong] editor, move |entry| {
                let name = entry.text().to_string();
                edit_button(&editor, layer_index, button_index, |button| {
                    button.icon = (!name.is_empty()).then(|| name.clone());
                });
            }));

            let theme_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
            theme_row.append(&gtk4::Label::new(Some("XDG icon theme:")));
            let theme = gtk4::Entry::new();
            theme.set_text(button.theme.as_deref().unwrap_or(""));
            theme.set_placeholder_text(Some("e.g. Adwaita (optional)"));
            theme_row.append(&theme);
            fields.append(&theme_row);
            theme.connect_changed(glib::clone!(#[strong] editor, move |entry| {
                let name = entry.text().to_string();
                edit_button(&editor, layer_index, button_index, |button| {
                    button.theme = (!name.is_empty()).then(|| name.clone());
                });
            }));
        }
        ButtonKind::Time => {
            let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
            row.append(&gtk4::Label::new(Some("Format:")));
            let formats = ["24hr", "12hr", "custom"];
            let dropdown = gtk4::DropDown::from_strings(&formats);
            let current = button.time.as_deref().unwrap_or("24hr");
            let selected = if current == "24hr" || current == "12hr" {
                current
            } else {
                "custom"
            };
            dropdown
                .set_selected(formats.iter().position(|f| *f == selected).unwrap_or(0) as u32);
            row.append(&dropdown);
            let custom = gtk4::Entry::new();
            custom.set_placeholder_text(Some("%H:%M"));
            if selected == "custom" {
                custom.set_text(current);
            }
            custom.set_hexpand(true);
            row.append(&custom);
            fields.append(&row);

            fn write_format(
                editor: &Rc<Editor>,
                layer_index: usize,
                button_index: usize,
                format: String,
            ) {
                edit_button(editor, layer_index, button_index, |button| {
                    button.time = Some(format);
                });
            }
            dropdown.connect_selected_notify(
                glib::clone!(#[strong] custom, #[strong] editor, move |dropdown| {
                    let format = if dropdown.selected() == 2 {
                        let text = custom.text().to_string();
                        (!text.is_empty()).then(|| text).unwrap_or_else(|| "%H:%M".into())
                    } else {
                        formats[dropdown.selected() as usize].to_owned()
                    };
                    write_format(&editor, layer_index, button_index, format);
                }),
            );
            custom.connect_changed(glib::clone!(#[strong] dropdown, #[strong] editor, move |entry| {
                if dropdown.selected() == 2 {
                    let text = entry.text().to_string();
                    if !text.is_empty() {
                        write_format(&editor, layer_index, button_index, text);
                    }
                }
            }));

            let locale_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
            locale_row.append(&gtk4::Label::new(Some("Locale:")));
            let locale = gtk4::Entry::new();
            locale.set_text(button.locale.as_deref().unwrap_or(""));
            locale.set_placeholder_text(Some("e.g. en_US (optional)"));
            locale_row.append(&locale);
            fields.append(&locale_row);
            locale.connect_changed(glib::clone!(#[strong] editor, move |entry| {
                let name = entry.text().to_string();
                edit_button(&editor, layer_index, button_index, |button| {
                    button.locale = (!name.is_empty()).then(|| name.clone());
                });
            }));
        }
        ButtonKind::Battery => {
            let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
            row.append(&gtk4::Label::new(Some("Mode:")));
            let modes = ["both", "icon", "percentage"];
            let dropdown = gtk4::DropDown::from_strings(&modes);
            let current = button.battery.as_deref().unwrap_or("both");
            dropdown.set_selected(modes.iter().position(|m| *m == current).unwrap_or(0) as u32);
            row.append(&dropdown);
            fields.append(&row);
            dropdown.connect_selected_notify(glib::clone!(#[strong] editor, move |dropdown| {
                let mode = modes[dropdown.selected() as usize].to_owned();
                edit_button(&editor, layer_index, button_index, |button| button.battery = Some(mode.clone()));
            }));
        }
        ButtonKind::Waveform | ButtonKind::Visualizer => {
            let is_wave = kind == ButtonKind::Waveform;
            let style_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
            style_row.append(&gtk4::Label::new(Some("Style:")));
            let styles = ["line", "bars"];
            let dropdown = gtk4::DropDown::from_strings(&styles);
            let current = if is_wave {
                button.waveform_style.as_deref()
            } else {
                button.visualizer_style.as_deref()
            }
            .unwrap_or("line");
            dropdown.set_selected(styles.iter().position(|s| *s == current).unwrap_or(0) as u32);
            style_row.append(&dropdown);
            fields.append(&style_row);
            dropdown.connect_selected_notify(glib::clone!(#[strong] editor, move |dropdown| {
                let style = styles[dropdown.selected() as usize].to_owned();
                edit_button(&editor, layer_index, button_index, |button| {
                    if is_wave {
                        button.waveform_style = Some(style.clone());
                    } else {
                        button.visualizer_style = Some(style.clone());
                    }
                });
            }));

            let motion_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
            motion_row.append(&gtk4::Label::new(Some("Smoothing ms:")));
            let smoothing = gtk4::SpinButton::with_range(0.0, 5000.0, 50.0);
            smoothing.set_value(
                if is_wave {
                    button.waveform_smoothing
                } else {
                    button.visualizer_smoothing
                }
                .unwrap_or(0) as f64,
            );
            motion_row.append(&smoothing);
            motion_row.append(&gtk4::Label::new(Some("Lag ms:")));
            let lag = gtk4::SpinButton::with_range(0.0, 500.0, 10.0);
            lag.set_value(
                if is_wave {
                    button.waveform_lag
                } else {
                    button.visualizer_lag
                }
                .unwrap_or(0) as f64,
            );
            motion_row.append(&lag);
            fields.append(&motion_row);
            smoothing.connect_value_changed(glib::clone!(#[strong] editor, move |spin| {
                let value = spin.value() as u32;
                edit_button(&editor, layer_index, button_index, |button| {
                    if is_wave {
                        button.waveform_smoothing = (value > 0).then_some(value);
                    } else {
                        button.visualizer_smoothing = (value > 0).then_some(value);
                    }
                });
            }));
            lag.connect_value_changed(glib::clone!(#[strong] editor, move |spin| {
                let value = spin.value() as u32;
                edit_button(&editor, layer_index, button_index, |button| {
                    if is_wave {
                        button.waveform_lag = (value > 0).then_some(value);
                    } else {
                        button.visualizer_lag = (value > 0).then_some(value);
                    }
                });
            }));
        }
        ButtonKind::NowPlaying | ButtonKind::Spacer => {
            fields.append(&gtk4::Label::new(Some(
                "This content kind has no additional knobs.",
            )));
        }
    }
}

/// The action editor: one dropdown for the grammar's forms, per-form
/// fields, and the key-capture button for combo actions.
fn action_editor(
    editor: &Rc<Editor>,
    layer_index: usize,
    button_index: usize,
    action: &dfr_config::ButtonAction,
) -> gtk4::Widget {
    let outer = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
    let frame = gtk4::Frame::new(Some("Action"));
    frame.set_child(Some(&outer));

    let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    row.append(&gtk4::Label::new(Some("On press:")));
    let forms = [
        "None",
        "Key combo",
        "Switch to layer",
        "Run command",
        "Run command + switch layer",
        "Apply theme",
        "Apply theme + switch layer",
        "Volume slider",
        "Brightness slider",
        "Cycle themes",
    ];
    let dropdown = gtk4::DropDown::from_strings(&forms);
    let selected = match ActionForm::of(action) {
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
    dropdown.set_selected(selected as u32);
    row.append(&dropdown);
    outer.append(&row);

    let fields = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
    outer.append(&fields);

    let (layer_names, theme_names) = {
        let mut doc = editor.doc.borrow_mut();
        (
            doc.layers().iter().map(|l| l.name.clone()).collect::<Vec<_>>(),
            doc.known_themes(),
        )
    };

    let rebuild = {
        let editor = editor.clone();
        let layer_names = layer_names.clone();
        let theme_names = theme_names.clone();
        move |form: &ActionForm, fields: &gtk4::Box| {
            rebuild_action_fields(fields, &editor, layer_index, button_index, form, &layer_names, &theme_names);
        }
    };
    rebuild(&ActionForm::of(action), &fields);

    dropdown.connect_selected_notify(
        glib::clone!(#[strong] editor, #[strong] fields, #[strong] dropdown, move |_| {
            let form = match dropdown.selected() {
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
            edit_button(&editor, layer_index, button_index, |button| button.action = form.to_action());
            rebuild(&form, &fields);
        }),
    );

    frame.upcast()
}

fn rebuild_action_fields(
    fields: &gtk4::Box,
    editor: &Rc<Editor>,
    layer_index: usize,
    button_index: usize,
    form: &ActionForm,
    layer_names: &[String],
    theme_names: &[String],
) {
    let mut child = fields.first_child();
    while let Some(widget) = child {
        child = widget.next_sibling();
        fields.remove(&widget);
    }

    match form {
        ActionForm::None
        | ActionForm::VolumeSlider
        | ActionForm::BrightnessSlider
        | ActionForm::CycleTheme => {}
        ActionForm::Keys(keys) => {
            let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
            let capture = gtk4::ToggleButton::with_label("Capture key combo");
            let label = gtk4::Label::new(Some(&keys_label(keys)));
            label.set_hexpand(true);
            label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
            let clear = gtk4::Button::from_icon_name("edit-clear-symbolic");
            clear.set_tooltip_text(Some("Clear keys"));
            row.append(&capture);
            row.append(&label);
            row.append(&clear);
            fields.append(&row);

            // The capture toggle: while active, a key controller
            // records pressed keys (one event per physical key, deduped
            // in arrival order); toggling off commits the combo.
            let captured: Rc<RefCell<Vec<Key>>> = Rc::new(RefCell::new(keys.clone()));
            let controller = gtk4::EventControllerKey::new();
            controller.set_propagation_phase(gtk4::PropagationPhase::Capture);
            controller.connect_key_pressed(glib::clone!(#[strong] captured, #[strong] label, move |_, _, keycode, _| {
                match key_from_gdk(keycode) {
                    Some(key) => {
                        let mut keys = captured.borrow_mut();
                        if !keys.contains(&key) {
                            keys.push(key);
                            label.set_text(&keys_label(&keys));
                        }
                        glib::Propagation::Stop
                    }
                    None => glib::Propagation::Proceed,
                }
            }));
            capture.add_controller(controller);

            capture.connect_toggled(glib::clone!(#[strong] editor, #[strong] captured, move |toggle| {
                if !toggle.is_active() {
                    // Commit on release.
                    let keys = captured.borrow().clone();
                    edit_button(&editor, layer_index, button_index, |button| {
                        button.action = ActionForm::Keys(keys.clone()).to_action();
                    });
                }
            }));
            clear.connect_clicked(glib::clone!(#[strong] editor, #[strong] captured, #[strong] label, move |_| {
                captured.borrow_mut().clear();
                label.set_text(&keys_label(&[]));
                edit_button(&editor, layer_index, button_index, |button| {
                    button.action = ActionForm::None.to_action();
                });
            }));
        }
        ActionForm::SwitchLayer(_)
        | ActionForm::CommandLayer { .. }
        | ActionForm::ApplyThemeLayer { .. } => {
            let mut names: Vec<String> = layer_names.to_vec();
            names.push("pet".into());
            names.push("themes".into());
            let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
            row.append(&gtk4::Label::new(Some("Layer:")));
            let strings: Vec<&str> = names.iter().map(String::as_str).collect();
            let dropdown = gtk4::DropDown::from_strings(&strings);
            let current = form.layer_target().unwrap_or_default().to_owned();
            if let Some(index) = names.iter().position(|name| *name == current) {
                dropdown.set_selected(index as u32);
            }
            row.append(&dropdown);
            fields.append(&row);
            dropdown.connect_selected_notify(
                glib::clone!(#[strong] editor, #[strong] names, move |dropdown| {
                    let name = names[dropdown.selected() as usize].clone();
                    edit_button(&editor, layer_index, button_index, |button| {
                        button.action = patch_layer_target(&button.action, &name).to_action();
                    });
                }),
            );
        }
        _ => {}
    }

    if matches!(
        form,
        ActionForm::Command(_) | ActionForm::CommandLayer { .. }
    ) {
        fields.append(&command_field(
            editor,
            layer_index,
            button_index,
            form.command().unwrap_or_default(),
        ));
    }
    if matches!(
        form,
        ActionForm::ApplyTheme(_) | ActionForm::ApplyThemeLayer { .. }
    ) {
        fields.append(&theme_field(
            editor,
            layer_index,
            button_index,
            match form {
                ActionForm::ApplyTheme(theme) | ActionForm::ApplyThemeLayer { theme, .. } => theme,
                _ => "",
            },
            theme_names,
        ));
    }
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

fn command_field(
    editor: &Rc<Editor>,
    layer_index: usize,
    button_index: usize,
    command: &str,
) -> gtk4::Widget {
    let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    row.append(&gtk4::Label::new(Some("Command:")));
    let entry = gtk4::Entry::new();
    entry.set_text(command);
    entry.set_placeholder_text(Some("e.g. systemctl --user toggle night-light"));
    entry.set_hexpand(true);
    row.append(&entry);
    entry.connect_changed(glib::clone!(#[strong] editor, move |entry| {
        let command = entry.text().to_string();
        edit_button(&editor, layer_index, button_index, |button| {
            button.action = patch_command(&button.action, &command).to_action();
        });
    }));
    row.upcast()
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

fn theme_field(
    editor: &Rc<Editor>,
    layer_index: usize,
    button_index: usize,
    theme: &str,
    theme_names: &[String],
) -> gtk4::Widget {
    let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    row.append(&gtk4::Label::new(Some("Theme:")));
    let strings: Vec<&str> = theme_names.iter().map(String::as_str).collect();
    let dropdown = gtk4::DropDown::from_strings(&strings);
    if let Some(index) = theme_names.iter().position(|name| name == theme) {
        dropdown.set_selected(index as u32);
    }
    row.append(&dropdown);
    let theme_names = theme_names.to_vec();
    dropdown.connect_selected_notify(
        glib::clone!(#[strong] editor, #[strong] theme_names, move |dropdown| {
            let theme = theme_names[dropdown.selected() as usize].clone();
            edit_button(&editor, layer_index, button_index, |button| {
                button.action = patch_theme(&button.action, &theme).to_action();
            });
        }),
    );
    row.upcast()
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
