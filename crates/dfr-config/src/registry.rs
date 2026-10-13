//! The knob registry: one table describing every knob a configuration
//! author can write - its name, type, default, constraints, and a
//! one-line purpose. It is the single source of truth the generated
//! README reference section renders from and the future configurator's
//! property model. Tests pin it against the wire types in both
//! directions: every registered name must parse, and every field the
//! types serialize must be registered.

use crate::appearance::ButtonAccent;
use crate::defaults;

/// Which table shape a knob belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// A top-level key of the configuration document.
    Global,
    /// A field of one button entry inside a layer's `Keys` list.
    Button,
}

/// One knob's metadata.
#[derive(Clone, Copy, Debug)]
pub struct Knob {
    /// The exact TOML key (`Svg` is additionally accepted for `Icon`).
    pub name: &'static str,
    /// The knob's wire type, spelled for authors.
    pub kind: &'static str,
    pub scope: Scope,
    /// The group heading the reference sorts the knob under.
    pub group: &'static str,
    /// The default used when neither base nor user file sets it.
    pub default: &'static str,
    /// The enforced or documented constraint, if any.
    pub constraint: Option<&'static str>,
    /// One line saying what the knob does.
    pub doc: &'static str,
}

const ACCENTS: &str = "neutral, cyan, amber, blue, violet, orange, red, green";

/// Every knob, in reference order.
pub const KNOBS: &[Knob] = &[
    // ---- Global: appearance
    Knob { name: "ShowButtonOutlines", kind: "bool", scope: Scope::Global, group: "Appearance",
        default: "true", constraint: None,
        doc: "Draw the outline around each button; false leaves only labels and glyphs." },
    Knob { name: "EnablePixelShift", kind: "bool", scope: Scope::Global, group: "Appearance",
        default: "false", constraint: None,
        doc: "Slowly shift the strip contents sideways to spread pixel wear." },
    Knob { name: "FontTemplate", kind: "string", scope: Scope::Global, group: "Appearance",
        default: "\":bold\"", constraint: Some("a fontconfig pattern that matches an installed font"),
        doc: "Fontconfig pattern selecting the font for text labels (see the fontconfig font-names guide)." },
    Knob { name: "ActiveBrightness", kind: "u32", scope: Scope::Global, group: "Appearance",
        default: "128", constraint: Some("0-255"),
        doc: "Static brightness with adaptive off; the curve's maximum with it on." },
    Knob { name: "AdaptiveBrightness", kind: "bool", scope: Scope::Global, group: "Appearance",
        default: "true", constraint: None,
        doc: "Follow the primary display's backlight instead of a fixed brightness." },
    // ---- Global: input
    Knob { name: "DoublePressSwitchLayers", kind: "u32", scope: Scope::Global, group: "Input",
        default: "0", constraint: Some("0 disables; otherwise a millisecond window"),
        doc: "Double-press Fn within this many milliseconds to swap the default layer." },
    // ---- Global: battery
    Knob { name: "BatteryCycleEnabled", kind: "bool", scope: Scope::Global, group: "Battery",
        default: "true", constraint: None,
        doc: "Tap a battery widget to cycle from the system battery through device batteries." },
    Knob { name: "BatteryCycleTimeoutSecs", kind: "u32", scope: Scope::Global, group: "Battery",
        default: "15", constraint: Some("1-3600 seconds"),
        doc: "Return to the system battery this long after the last cycling tap." },
    Knob { name: "BatteryWarningThreshold", kind: "u32", scope: Scope::Global, group: "Battery",
        default: "25", constraint: Some("0-100, at least the critical threshold"),
        doc: "Below this charge percent (and not charging) the widget paints the warning color." },
    Knob { name: "BatteryCriticalThreshold", kind: "u32", scope: Scope::Global, group: "Battery",
        default: "10", constraint: Some("0-100, at most the warning threshold"),
        doc: "Below this charge percent the widget paints the critical color." },
    Knob { name: "BatteryWarningColor", kind: "enum", scope: Scope::Global, group: "Battery",
        default: "orange", constraint: Some(ACCENTS),
        doc: "Accent painted below the warning threshold while not charging." },
    Knob { name: "BatteryCriticalColor", kind: "enum", scope: Scope::Global, group: "Battery",
        default: "red", constraint: Some(ACCENTS),
        doc: "Accent painted below the critical threshold while not charging." },
    // ---- Global: pet
    Knob { name: "PetEnabled", kind: "bool", scope: Scope::Global, group: "Pet",
        default: "true", constraint: None,
        doc: "Keep the pet layer and its home-bar Pet button; false removes both." },
    Knob { name: "PetVariant", kind: "enum", scope: Scope::Global, group: "Pet",
        default: "cat", constraint: Some("cat, fox"),
        doc: "The pet's sprite: an orange tabby or a red fox." },
    Knob { name: "PetEnergy", kind: "enum", scope: Scope::Global, group: "Pet",
        default: "calm", constraint: Some("calm, rowdy"),
        doc: "Rowdy pets dash more and get hungry sooner; calm ones stroll and doze." },
    // ---- Global: background
    Knob { name: "DayCycleEnabled", kind: "bool", scope: Scope::Global, group: "Background",
        default: "true", constraint: None,
        doc: "Tint the strip by time of day; false keeps the plain black background." },
    Knob { name: "DayCycleBands", kind: "list of tables", scope: Scope::Global, group: "Background",
        default: "four shipped bands", constraint: Some("at least one band; unique names and 0-23 hours"),
        doc: "Replace the default bands: each [[DayCycleBands]] entry has Name, StartHour, and a \"#RRGGBB\" Color." },
    // ---- Global: themes
    Knob { name: "ThemesEnabled", kind: "bool", scope: Scope::Global, group: "Themes",
        default: "true", constraint: None,
        doc: "Enable the color-theme system and picker layer; add CycleTheme or a themes-layer button explicitly to show theme controls." },
    Knob { name: "ActiveTheme", kind: "string", scope: Scope::Global, group: "Themes",
        default: "the remembered choice", constraint: Some("must name a defined theme"),
        doc: "Pin the startup theme instead of the last choice made on the glass." },
    Knob { name: "Themes", kind: "table map", scope: Scope::Global, group: "Themes",
        default: "the eight built-ins", constraint: Some("at most 21 themes in total"),
        doc: "Define or override themes: [Themes.Name] with an Accents map of accent names to \"#RRGGBB\" and optional [[...DayCycleBands]]." },
    // ---- Global: layers
    Knob { name: "MediaLayerDefault", kind: "bool", scope: Scope::Global, group: "Layers",
        default: "false", constraint: Some("legacy syntax only"),
        doc: "Legacy syntax: show the media keys without Fn pressed." },
    Knob { name: "PrimaryLayerKeys", kind: "list of tables", scope: Scope::Global, group: "Layers",
        default: "the shipped F1-F12 bar", constraint: Some("legacy syntax; mutually exclusive with Layers"),
        doc: "Legacy syntax: the primary layer's buttons." },
    Knob { name: "MediaLayerKeys", kind: "list of tables", scope: Scope::Global, group: "Layers",
        default: "the shipped media bar", constraint: Some("legacy syntax; mutually exclusive with Layers"),
        doc: "Legacy syntax: the media layer's buttons." },
    Knob { name: "Layers", kind: "list of tables", scope: Scope::Global, group: "Layers",
        default: "none (legacy syntax)", constraint: Some("named syntax; requires DefaultLayer; unique nonempty names"),
        doc: "The named bars: each [[Layers]] entry has a Name and a nonempty Keys list of button tables." },
    Knob { name: "DefaultLayer", kind: "string", scope: Scope::Global, group: "Layers",
        default: "primary (legacy)", constraint: Some("must name a Layers entry when Layers is used"),
        doc: "The layer the bar starts on and returns to." },
    Knob { name: "FnLayer", kind: "string", scope: Scope::Global, group: "Layers",
        default: "the other legacy layer", constraint: Some("optional; must name a Layers entry"),
        doc: "The layer held while Fn is pressed." },
    // ---- Button knobs
    Knob { name: "Icon", kind: "string", scope: Scope::Button, group: "Button content",
        default: "none", constraint: Some("Svg is an accepted alias; a file must resolve"),
        doc: "Icon name, without extension: the fixed icon directories, or the XDG theme when Theme is set." },
    Knob { name: "Text", kind: "string", scope: Scope::Button, group: "Button content",
        default: "none", constraint: None,
        doc: "The button's text label." },
    Knob { name: "Theme", kind: "string", scope: Scope::Button, group: "Button content",
        default: "none", constraint: Some("an installed XDG icon theme"),
        doc: "The XDG icon theme to resolve this button's Icon in (not a color theme)." },
    Knob { name: "Time", kind: "string", scope: Scope::Button, group: "Button content",
        default: "none", constraint: Some("24hr, 12hr, or a chrono strftime format string"),
        doc: "Show a clock; the value is a shorthand or a strftime format." },
    Knob { name: "Locale", kind: "string", scope: Scope::Button, group: "Button content",
        default: "POSIX", constraint: Some("a locale chrono knows"),
        doc: "Locale for the Time button's names; an unknown locale falls back to POSIX." },
    Knob { name: "Battery", kind: "enum", scope: Scope::Button, group: "Button content",
        default: "none", constraint: Some("icon, percentage, both"),
        doc: "Show the battery as an icon ladder, a percentage, or both." },
    Knob { name: "Waveform", kind: "bool", scope: Scope::Button, group: "Waveform and visualizer",
        default: "false", constraint: None,
        doc: "Make this button the live microphone waveform widget." },
    Knob { name: "WaveformStyle", kind: "enum", scope: Scope::Button, group: "Waveform and visualizer",
        default: "line", constraint: Some("line, bars"),
        doc: "The waveform's graph style." },
    Knob { name: "WaveformSmoothing", kind: "u32", scope: Scope::Button, group: "Waveform and visualizer",
        default: "0", constraint: Some("0-5000 ms"),
        doc: "Milliseconds of level decay for calmer waveform motion; the attack is a third of it." },
    Knob { name: "WaveformLag", kind: "u32", scope: Scope::Button, group: "Waveform and visualizer",
        default: "0", constraint: Some("0-500 ms"),
        doc: "Milliseconds of visual delay behind the measured level." },
    Knob { name: "Visualizer", kind: "bool", scope: Scope::Button, group: "Waveform and visualizer",
        default: "false", constraint: None,
        doc: "Make this button the opt-in music visualizer; a tap expands it to the full bar." },
    Knob { name: "VisualizerStyle", kind: "enum", scope: Scope::Button, group: "Waveform and visualizer",
        default: "line", constraint: Some("line, bars"),
        doc: "The visualizer's graph style." },
    Knob { name: "VisualizerSmoothing", kind: "u32", scope: Scope::Button, group: "Waveform and visualizer",
        default: "0", constraint: Some("0-5000 ms"),
        doc: "Visualizer motion decay, as WaveformSmoothing." },
    Knob { name: "VisualizerLag", kind: "u32", scope: Scope::Button, group: "Waveform and visualizer",
        default: "0", constraint: Some("0-500 ms"),
        doc: "Visualizer visual delay, as WaveformLag." },
    Knob { name: "NowPlaying", kind: "bool", scope: Scope::Button, group: "Button content",
        default: "false", constraint: None,
        doc: "Make this button the now-playing row: the current track's title and artist." },
    // ---- Button: action
    Knob { name: "Action", kind: "string, list, or table", scope: Scope::Button, group: "Button action",
        default: "none", constraint: Some("a key name, a key list, a slider name, CycleTheme, or a Layer/Command/Theme table"),
        doc: "What a press does: emit keys, run a command, switch layers, open a slider, or apply a theme." },
    // ---- Button: size and color
    Knob { name: "Stretch", kind: "usize", scope: Scope::Button, group: "Button size and color",
        default: "1", constraint: Some("at least 1"),
        doc: "Button spaces this button occupies." },
    Knob { name: "IconWidth", kind: "i32", scope: Scope::Button, group: "Button size and color",
        default: "40", constraint: Some("8-256 px"),
        doc: "Icon width; match PNG pixel size for best results." },
    Knob { name: "IconHeight", kind: "i32", scope: Scope::Button, group: "Button size and color",
        default: "40", constraint: Some("8-256 px"),
        doc: "Icon height; match PNG pixel size for best results." },
    Knob { name: "Accent", kind: "enum", scope: Scope::Button, group: "Button size and color",
        default: "the action's family", constraint: Some(ACCENTS),
        doc: "The semantic accent color for this button's fill and outline." },
];

/// The one-line documentation for a knob, for the canonical writer's
/// comments.
pub fn doc_for(name: &str) -> Option<&'static str> {
    KNOBS
        .iter()
        .find(|knob| knob.name == name)
        .map(|knob| knob.doc)
}

/// The registered global knob names, in registry order.
pub fn global_knob_names() -> Vec<&'static str> {
    KNOBS
        .iter()
        .filter(|knob| knob.scope == Scope::Global)
        .map(|knob| knob.name)
        .collect()
}

/// The registered button knob names, in registry order.
pub fn button_knob_names() -> Vec<&'static str> {
    KNOBS
        .iter()
        .filter(|knob| knob.scope == Scope::Button)
        .map(|knob| knob.name)
        .collect()
}

/// Render the README reference section: the stable, generated block the
/// README carries between the marker comments. A test pins the README
/// copy to this output, so the two cannot drift.
pub fn reference_markdown() -> String {
    let mut out = String::new();
    out.push_str("The knob registry in `crates/dfr-config/src/registry.rs` is the single\n");
    out.push_str("source of truth for this table; a test regenerates it and fails when the\n");
    out.push_str("README copy drifts. Every knob rejects unknown siblings: a typo in a\n");
    out.push_str("name rejects the candidate configuration naming the line, not a silent\n");
    out.push_str("ignore. Defaults are code-level: a stripped base file still loads.\n");
    out.push('\n');
    for (scope, heading) in [
        (Scope::Global, "Global knobs"),
        (Scope::Button, "Button knobs"),
    ] {
        out.push_str("### ");
        out.push_str(heading);
        out.push('\n');
        out.push('\n');
        out.push_str("| Knob | Type | Default | Constraints | Purpose |\n");
        out.push_str("|---|---|---|---|---|\n");
        let mut group = "";
        for knob in KNOBS.iter().filter(|knob| knob.scope == scope) {
            if knob.group != group {
                group = knob.group;
                out.push_str(&format!("| **{}** | | | | |\n", group));
            }
            out.push_str(&format!(
                "| `{}` | {} | `{}` | {} | {} |\n",
                knob.name,
                knob.kind,
                knob.default,
                knob.constraint.unwrap_or("-"),
                knob.doc
            ));
        }
        out.push('\n');
    }
    out.push_str("Reserved layer names: `pet` and `themes` name the daemon's synthesized\n");
    out.push_str("layers; a user layer with the name replaces the synthesized one. The\n");
    out.push_str("button `Theme` selects an XDG *icon* theme; the action table's `Theme`\n");
    out.push_str("applies a *color* theme. No `Version` key exists yet; one would be\n");
    out.push_str("additive if a future migration needs a hook.\n");
    out
}

/// The marker comments framing the generated block in the README.
pub const REFERENCE_BEGIN: &str = "<!-- dfr-config knob reference: begin -->";
pub const REFERENCE_END: &str = "<!-- dfr-config knob reference: end -->";

/// The shipped default of a global knob as a display string, for tools
/// that want the default without re-deriving it (the canonical source
/// remains [`defaults`]).
pub fn default_of(name: &str) -> Option<&'static str> {
    KNOBS
        .iter()
        .find(|knob| knob.name == name)
        .map(|knob| knob.default)
}

/// The battery escalation color defaults as accents, for tooling.
pub fn battery_default_colors() -> (ButtonAccent, ButtonAccent) {
    (
        defaults::BATTERY_WARNING_COLOR,
        defaults::BATTERY_CRITICAL_COLOR,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const KITCHEN_SINK: &str = include_str!("../fixtures/kitchen-sink.toml");

    #[test]
    fn every_registered_name_parses_as_a_real_knob() {
        // With unknown fields denied, a document built from the registry's
        // own names proves every registered name is live: a stale entry
        // would be rejected here.
        let doc = [
            ("ShowButtonOutlines", "false"),
            ("EnablePixelShift", "true"),
            ("FontTemplate", "':bold'"),
            ("AdaptiveBrightness", "false"),
            ("ActiveBrightness", "128"),
            ("DoublePressSwitchLayers", "200"),
            ("BatteryCycleEnabled", "false"),
            ("BatteryCycleTimeoutSecs", "15"),
            ("BatteryWarningThreshold", "25"),
            ("BatteryCriticalThreshold", "10"),
            ("BatteryWarningColor", "'orange'"),
            ("BatteryCriticalColor", "'red'"),
            ("PetEnabled", "false"),
            ("PetVariant", "'fox'"),
            ("PetEnergy", "'rowdy'"),
            ("DayCycleEnabled", "false"),
            ("ThemesEnabled", "false"),
            ("ActiveTheme", "'Default'"),
            ("MediaLayerDefault", "true"),
        ]
        .iter()
        .map(|(name, value)| format!("{name} = {value}"))
        .collect::<Vec<_>>()
        .join("\n");
        toml::from_str::<crate::ConfigProxy>(&doc).expect("every registry name is a real knob");
        // The list-and-table knobs parse through their document forms;
        // the array keys must precede the table sections, as TOML scoping
        // requires (and as the canonical writer emits them).
        let sections =
            "\nPrimaryLayerKeys = [{ Text = \"F1\" }]\nMediaLayerKeys = [{ Text = \"V\" }]\n\
[[DayCycleBands]]\nName = \"day\"\nStartHour = 12\nColor = \"#1A1B1E\"\n\
[Themes.Mine.Accents]\ncyan = \"#40E0D0\"\n";
        toml::from_str::<crate::ConfigProxy>(&(doc + sections))
            .expect("every registry table knob is a real knob");
        // The Layers knobs parse in the named form.
        toml::from_str::<crate::ConfigProxy>(
            "Layers = [{ Name = \"home\", Keys = [{ Text = \"F1\" }] }]\nDefaultLayer = \"home\"\n",
        )
        .expect("Layers and DefaultLayer are real knobs");
    }

    #[test]
    fn every_real_knob_is_registered() {
        // The reverse direction: a document with every field set
        // serializes to exactly the registry's name set, so a knob added
        // to the wire types without a registry entry fails here.
        let proxy: crate::ConfigProxy = toml::from_str(KITCHEN_SINK).unwrap();
        let legacy: crate::ConfigProxy =
            toml::from_str(include_str!("../fixtures/legacy.toml")).unwrap();
        let merged = {
            let mut merged = proxy.clone();
            merged.merge(legacy);
            merged
        };
        let table = toml::Value::try_from(&merged).expect("a fully set model serializes");
        let mut written: Vec<&str> = table
            .as_table()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        written.sort_unstable();
        let mut registered = global_knob_names();
        registered.sort_unstable();
        assert_eq!(written, registered);
        // The same pin for the button knobs: one button with every field
        // set (the proxy accepts all content knobs at once; priority is a
        // render-time rule) serializes to exactly the button registry.
        let doc = "button = { Icon = \"search\", Text = \"T\", Theme = \"Adwaita\", Time = \"24hr\", Battery = \"both\", Waveform = true, WaveformStyle = \"bars\", WaveformSmoothing = 10, WaveformLag = 20, Visualizer = true, VisualizerStyle = \"line\", VisualizerSmoothing = 30, VisualizerLag = 40, NowPlaying = true, Locale = \"en_US\", Action = \"F1\", Stretch = 2, IconWidth = 40, IconHeight = 40, Accent = \"cyan\" }";
        #[derive(serde::Deserialize)]
        struct WrappedButton {
            button: crate::ButtonConfig,
        }
        let button = toml::from_str::<WrappedButton>(doc).unwrap().button;
        let button_table = toml::Value::try_from(&button).unwrap();
        let mut button_names: Vec<&str> = button_table
            .as_table()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        button_names.sort_unstable();
        let mut registered_buttons = button_knob_names();
        registered_buttons.sort_unstable();
        assert_eq!(button_names, registered_buttons);
    }

    #[test]
    fn the_readme_reference_section_stays_in_sync() {
        let readme = include_str!("../../../README.md");
        let begin = readme
            .find(REFERENCE_BEGIN)
            .expect("the README carries the generated reference block");
        let end = readme.find(REFERENCE_END).expect("the block is closed");
        let body = &readme[begin + REFERENCE_BEGIN.len()..end];
        let expected = reference_markdown();
        assert_eq!(
            body.trim(),
            expected.trim(),
            "the README reference drifted from the knob registry; regenerate it"
        );
        // The reference itself renders for every knob.
        assert!(expected.contains("| `ShowButtonOutlines` |"));
        assert!(expected.contains("| `Stretch` |"));
    }

    #[test]
    fn near_miss_names_find_their_knobs() {
        assert_eq!(nearest_matches("Tex"), vec!["Text"]);
        assert_eq!(
            nearest_matches("ShowButtonOutline"),
            vec!["ShowButtonOutlines"]
        );
        assert_eq!(
            nearest_matches("BateryCycleTimeoutSecs"),
            vec!["BatteryCycleTimeoutSecs"]
        );
        assert!(nearest_matches("Zzzzz").is_empty());
    }

    fn nearest_matches(typo: &str) -> Vec<&'static str> {
        crate::proxy::nearest_knob_name(typo).into_iter().collect()
    }
}
