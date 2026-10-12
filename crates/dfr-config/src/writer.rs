//! The canonical TOML writer: serialize a configuration proxy (a user
//! overlay, or any fully populated model) into the one stable, commented
//! spelling of the file. Round-trip safe - parsing the output yields an
//! equal proxy - deterministic in field order, quoting, and comments,
//! so tools can diff or rewrite configurations without churn. The
//! daemon itself never writes configuration; this is the writer the
//! future configurator's save step and the tests share.

use crate::proxy::{ButtonAction, ButtonConfig, ConfigProxy};
use crate::registry;
use std::fmt::Write as _;

/// Render the canonical user-configuration document for `proxy`.
pub fn to_user_toml(proxy: &ConfigProxy) -> String {
    let mut out = String::new();
    out.push_str("# tiny-dfr configuration, written canonically by dfr-config.\n");
    out.push_str("# Installed as the user file, it overlays the shipped base per\n");
    out.push_str("# field; arrays replace wholesale. Unknown keys are rejected, so\n");
    out.push_str("# every knob below is spelled exactly as the daemon reads it.\n");
    out.push('\n');
    let mut first = true;
    for name in registry::global_knob_names() {
        if let Some(rendered) = global_scalar(proxy, name) {
            if !first {
                out.push('\n');
            }
            first = false;
            emit_doc(&mut out, name);
            let _ = writeln!(out, "{name} = {rendered}");
        }
    }
    // The legacy button lists and every table section follow the scalars,
    // as TOML requires: a key after a table header would belong to the table.
    for (name, keys) in [
        ("PrimaryLayerKeys", &proxy.primary_layer_keys),
        ("MediaLayerKeys", &proxy.media_layer_keys),
    ] {
        if let Some(keys) = keys {
            if !first {
                out.push('\n');
            }
            first = false;
            emit_doc(&mut out, name);
            emit_key_list(&mut out, name, keys);
        }
    }
    if let Some(bands) = &proxy.day_cycle_bands {
        if !first {
            out.push('\n');
        }
        first = false;
        emit_doc(&mut out, "DayCycleBands");
        for band in bands {
            let table = value_of(band);
            out.push_str("[[DayCycleBands]]\n");
            emit_table_in_order(&mut out, &table, &["Name", "StartHour", "Color"]);
        }
    }
    if let Some(themes) = &proxy.themes {
        if !first {
            out.push('\n');
        }
        first = false;
        emit_doc(&mut out, "Themes");
        for (name, theme) in themes {
            let _ = writeln!(out, "[Themes.{}]", key(name));
            if let Some(accents) = &theme.accents {
                let _ = writeln!(out, "[Themes.{}.Accents]", key(name));
                for (accent, color) in accents {
                    let _ = writeln!(out, "{} = {}", accent.name(), quote(color));
                }
            }
            if let Some(bands) = &theme.day_cycle_bands {
                for band in bands {
                    let _ = writeln!(out, "[[Themes.{}.DayCycleBands]]", key(name));
                    let table = value_of(band);
                    emit_table_in_order(&mut out, &table, &["Name", "StartHour", "Color"]);
                }
            }
        }
    }
    if let Some(layers) = &proxy.layers {
        if !first {
            out.push('\n');
        }
        for layer in layers {
            out.push_str("[[Layers]]\n");
            let _ = writeln!(out, "Name = {}", quote(&layer.name));
            emit_key_list(&mut out, "Keys", &layer.keys);
        }
    }
    out
}

/// The scalar spelling of one global knob, or `None` when it is unset
/// (omitted from the canonical output) or a table section.
fn global_scalar(proxy: &ConfigProxy, name: &str) -> Option<String> {
    let int = |number: u32| toml::Value::Integer(i64::from(number));
    let value = match name {
        "ShowButtonOutlines" => proxy.show_button_outlines?.into(),
        "EnablePixelShift" => proxy.enable_pixel_shift?.into(),
        "FontTemplate" => return proxy.font_template.as_deref().map(quote),
        "AdaptiveBrightness" => proxy.adaptive_brightness?.into(),
        "ActiveBrightness" => int(proxy.active_brightness?),
        "DoublePressSwitchLayers" => int(proxy.double_press_switch_layers?),
        "BatteryCycleEnabled" => proxy.battery_cycle_enabled?.into(),
        "BatteryCycleTimeoutSecs" => int(proxy.battery_cycle_timeout_secs?),
        "BatteryWarningThreshold" => int(proxy.battery_warning_threshold?),
        "BatteryCriticalThreshold" => int(proxy.battery_critical_threshold?),
        "BatteryWarningColor" => {
            return proxy
                .battery_warning_color
                .map(|accent| quote(accent.name()))
        }
        "BatteryCriticalColor" => {
            return proxy
                .battery_critical_color
                .map(|accent| quote(accent.name()))
        }
        "PetEnabled" => proxy.pet_enabled?.into(),
        "PetVariant" => return proxy.pet_variant.as_deref().map(quote),
        "PetEnergy" => return proxy.pet_energy.as_deref().map(quote),
        "DayCycleEnabled" => proxy.day_cycle_enabled?.into(),
        "ThemesEnabled" => proxy.themes_enabled?.into(),
        "ActiveTheme" => return proxy.active_theme.as_deref().map(quote),
        "MediaLayerDefault" => proxy.media_layer_default?.into(),
        "DefaultLayer" => return proxy.default_layer.as_deref().map(quote),
        "FnLayer" => return proxy.fn_layer.as_deref().map(quote),
        // Table-valued knobs are emitted as sections, never scalars.
        "DayCycleBands" | "Themes" | "PrimaryLayerKeys" | "MediaLayerKeys" | "Layers" => {
            return None
        }
        other => unreachable!("registry names an unknown knob {other:?}"),
    };
    Some(render_value(&value))
}

fn emit_doc(out: &mut String, name: &str) {
    if let Some(doc) = registry::doc_for(name) {
        let _ = writeln!(out, "# {doc}");
    }
}

fn emit_key_list(out: &mut String, name: &str, keys: &[ButtonConfig]) {
    let _ = writeln!(out, "{name} = [");
    for button in keys {
        let _ = writeln!(out, "    {},", format_button(button));
    }
    out.push_str("]\n");
}

/// One button as an inline table, fields in registry order, unset knobs
/// omitted, and a do-nothing Action omitted entirely.
fn format_button(button: &ButtonConfig) -> String {
    let table = value_of(button);
    let mut parts = Vec::new();
    for name in registry::button_knob_names() {
        if name == "Action" && button.action == ButtonAction::default() {
            continue;
        }
        if let Some(value) = table.get(name) {
            parts.push(format!("{} = {}", name, render_value(value)));
        }
    }
    if parts.is_empty() {
        "{}".to_owned()
    } else {
        format!("{{ {} }}", parts.join(", "))
    }
}

fn emit_table_in_order(out: &mut String, table: &toml::Value, order: &[&str]) {
    let table = table.as_table().expect("band tables serialize as tables");
    for name in order {
        if let Some(value) = table.get(*name) {
            let _ = writeln!(out, "{name} = {}", render_value(value));
        }
    }
}

/// Serialize a fully concrete value (no `None` inside) into a `toml::Value`.
fn value_of<T: serde::Serialize>(value: &T) -> toml::Value {
    toml::Value::try_from(value).expect("concrete configuration values serialize")
}

/// Render a `toml::Value` in its canonical inline spelling.
fn render_value(value: &toml::Value) -> String {
    match value {
        toml::Value::String(text) => quote(text),
        toml::Value::Integer(number) => number.to_string(),
        toml::Value::Boolean(flag) => flag.to_string(),
        toml::Value::Array(items) => format!(
            "[{}]",
            items
                .iter()
                .map(render_value)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        toml::Value::Table(table) => format!(
            "{{ {} }}",
            table
                .iter()
                .map(|(name, value)| format!("{} = {}", key(name), render_value(value)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        other => unreachable!("configuration never renders {other:?}"),
    }
}

/// A TOML basic string with minimal, deterministic escaping.
fn quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                let _ = write!(out, "\\u{:04X}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A table key: bare when it is a bare key, quoted otherwise.
fn key(text: &str) -> String {
    let bare = !text.is_empty()
        && text
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-');
    if bare {
        text.to_owned()
    } else {
        quote(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{parse_config, LoadedModel};
    use crate::proxy::ConfigProxy;

    const BASE: &str = include_str!("../../../share/tiny-dfr/config.toml");
    const KITCHEN_SINK: &str = include_str!("../fixtures/kitchen-sink.toml");
    const LEGACY: &str = include_str!("../fixtures/legacy.toml");
    const MULTIBAR: &str = include_str!("../../../share/tiny-dfr/multibar.toml");

    fn parse(text: &str) -> ConfigProxy {
        toml::from_str(text).expect("fixture parses")
    }

    #[test]
    fn kitchen_sink_round_trips_through_the_canonical_writer() {
        let proxy = parse(KITCHEN_SINK);
        let written = to_user_toml(&proxy);
        let reparsed = parse(&written);
        assert_eq!(
            proxy, reparsed,
            "canonical output re-parses to an equal model"
        );
        // Writing the reparsed model again is byte-identical: the format
        // is a fixed point.
        assert_eq!(written, to_user_toml(&reparsed));
    }

    #[test]
    fn legacy_syntax_round_trips_too() {
        let proxy = parse(LEGACY);
        let written = to_user_toml(&proxy);
        assert_eq!(proxy, parse(&written));
    }

    #[test]
    fn every_shipped_configuration_round_trips() {
        // The shipped base, parsed alone as a document.
        let base = parse(BASE);
        assert_eq!(base, parse(&to_user_toml(&base)));
        // The shipped showcase example as a user overlay.
        let multibar = parse(MULTIBAR);
        assert_eq!(multibar, parse(&to_user_toml(&multibar)));
        // The merged model over the base, written as one document.
        let LoadedModel { proxy: merged, .. } = parse_config(BASE, Some(MULTIBAR), 2008).unwrap();
        assert_eq!(merged, parse(&to_user_toml(&merged)));
    }

    #[test]
    fn canonical_output_is_pinned_by_a_golden_file() {
        let written = to_user_toml(&parse(KITCHEN_SINK));
        let golden = include_str!("../fixtures/kitchen-sink.canonical.toml");
        assert_eq!(
            written, golden,
            "the canonical format drifted; update the golden file deliberately"
        );
    }

    #[test]
    fn the_writer_names_every_registry_knob_and_comments_it() {
        // The two syntaxes are mutually exclusive, so between the named
        // kitchen sink and the legacy fixture every registry knob must
        // appear in at least one canonical output.
        let mut both = to_user_toml(&parse(KITCHEN_SINK));
        both.push_str(&to_user_toml(&parse(LEGACY)));
        for name in crate::registry::global_knob_names() {
            assert!(both.contains(name), "missing global knob {name}");
        }
        for name in crate::registry::button_knob_names() {
            assert!(
                both.contains(&format!("{name} =")),
                "missing button knob {name}"
            );
        }
        // Every emitted scalar carries its one-line documentation.
        let doc = crate::registry::doc_for("ShowButtonOutlines").unwrap();
        assert!(both.contains(&format!("# {doc}")));
    }

    #[test]
    fn unset_knobs_are_omitted_and_set_knobs_survive() {
        // A minimal user overlay writes only what it sets.
        let minimal = parse("ActiveBrightness = 99\n");
        let written = to_user_toml(&minimal);
        let emitted = written
            .lines()
            .filter(|line| !line.starts_with('#') && !line.trim().is_empty());
        assert_eq!(emitted.count(), 1, "{written}");
        assert!(written.contains("ActiveBrightness = 99"));
        assert!(!written.contains("ShowButtonOutlines"));
    }
}
