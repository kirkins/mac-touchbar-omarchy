//! GDK-keycode to `input_linux::Key` mapping and the key's wire name,
//! so the capture widget records exactly the spelling the
//! configuration grammar serializes (`LeftCtrl`, `F1`, ...).

use input_linux::Key;
use serde::Serialize as _;
use std::collections::HashMap;
use std::sync::OnceLock;

/// Translate a GDK hardware keycode (evdev code + 8 on Linux) into the
/// configuration's key. `None` for codes outside the kernel's key map.
pub fn key_from_gdk(keycode: u32) -> Option<Key> {
    keycode
        .checked_sub(8)
        .and_then(|code| Key::from_code(code as u16).ok())
}

/// The key's wire name: the serde variant name the untagged `Action`
/// grammar accepts and the canonical writer emits.
pub fn key_name(key: Key) -> String {
    key.serialize(VariantName)
        .expect("keys serialize as their variant names")
}

/// The full key name table, for combo pickers and tests.
#[allow(dead_code)]
pub fn key_table() -> &'static HashMap<u16, String> {
    static TABLE: OnceLock<HashMap<u16, String>> = OnceLock::new();
    TABLE.get_or_init(|| {
        Key::iter()
            .map(|key: Key| (key.code(), key_name(key)))
            .collect()
    })
}

/// Find a key by its wire name (case-sensitive), as parsed configs do.
#[allow(dead_code)]
pub fn key_by_name(name: &str) -> Option<Key> {
    Key::from_code(
        key_table()
            .iter()
            .find(|(_, wire)| wire.as_str() == name)
            .map(|(code, _)| *code)?,
    )
    .ok()
}

/// A serializer that accepts exactly what a unit-only enum emits: its
/// variant name, returned as an owned `String` (the same minimal shape
/// `dfr-config`'s action grammar uses internally).
struct VariantName;

#[derive(Debug)]
enum KeyError {
    Unsupported,
}

impl std::fmt::Display for KeyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("only unit variant names can be captured")
    }
}

impl std::error::Error for KeyError {}

impl serde::ser::Error for KeyError {
    fn custom<T: std::fmt::Display>(_: T) -> Self {
        KeyError::Unsupported
    }
}

impl serde::Serializer for VariantName {
    type Ok = String;
    type Error = KeyError;
    type SerializeSeq = serde::ser::Impossible<String, KeyError>;
    type SerializeTuple = serde::ser::Impossible<String, KeyError>;
    type SerializeTupleStruct = serde::ser::Impossible<String, KeyError>;
    type SerializeTupleVariant = serde::ser::Impossible<String, KeyError>;
    type SerializeMap = serde::ser::Impossible<String, KeyError>;
    type SerializeStruct = serde::ser::Impossible<String, KeyError>;
    type SerializeStructVariant = serde::ser::Impossible<String, KeyError>;

    fn serialize_unit_variant(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
    ) -> Result<String, KeyError> {
        Ok(variant.to_owned())
    }

    fn serialize_str(self, value: &str) -> Result<String, KeyError> {
        Ok(value.to_owned())
    }

    fn serialize_bool(self, _: bool) -> Result<String, KeyError> {
        Err(KeyError::Unsupported)
    }

    fn serialize_i8(self, _: i8) -> Result<String, KeyError> {
        Err(KeyError::Unsupported)
    }

    fn serialize_i16(self, _: i16) -> Result<String, KeyError> {
        Err(KeyError::Unsupported)
    }

    fn serialize_i32(self, _: i32) -> Result<String, KeyError> {
        Err(KeyError::Unsupported)
    }

    fn serialize_i64(self, _: i64) -> Result<String, KeyError> {
        Err(KeyError::Unsupported)
    }

    fn serialize_u8(self, _: u8) -> Result<String, KeyError> {
        Err(KeyError::Unsupported)
    }

    fn serialize_u16(self, _: u16) -> Result<String, KeyError> {
        Err(KeyError::Unsupported)
    }

    fn serialize_u32(self, _: u32) -> Result<String, KeyError> {
        Err(KeyError::Unsupported)
    }

    fn serialize_u64(self, _: u64) -> Result<String, KeyError> {
        Err(KeyError::Unsupported)
    }

    fn serialize_f32(self, _: f32) -> Result<String, KeyError> {
        Err(KeyError::Unsupported)
    }

    fn serialize_f64(self, _: f64) -> Result<String, KeyError> {
        Err(KeyError::Unsupported)
    }

    fn serialize_char(self, _: char) -> Result<String, KeyError> {
        Err(KeyError::Unsupported)
    }

    fn serialize_bytes(self, _: &[u8]) -> Result<String, KeyError> {
        Err(KeyError::Unsupported)
    }

    fn serialize_none(self) -> Result<String, KeyError> {
        Err(KeyError::Unsupported)
    }

    fn serialize_some<T: ?Sized + serde::Serialize>(self, _: &T) -> Result<String, KeyError> {
        Err(KeyError::Unsupported)
    }

    fn serialize_unit(self) -> Result<String, KeyError> {
        Err(KeyError::Unsupported)
    }

    fn serialize_unit_struct(self, _: &'static str) -> Result<String, KeyError> {
        Err(KeyError::Unsupported)
    }

    fn serialize_newtype_struct<T: ?Sized + serde::Serialize>(
        self,
        _: &'static str,
        _: &T,
    ) -> Result<String, KeyError> {
        Err(KeyError::Unsupported)
    }

    fn serialize_newtype_variant<T: ?Sized + serde::Serialize>(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        _: &T,
    ) -> Result<String, KeyError> {
        Err(KeyError::Unsupported)
    }

    fn serialize_seq(self, _: Option<usize>) -> Result<Self::SerializeSeq, KeyError> {
        Err(KeyError::Unsupported)
    }

    fn serialize_tuple(self, _: usize) -> Result<Self::SerializeTuple, KeyError> {
        Err(KeyError::Unsupported)
    }

    fn serialize_tuple_struct(
        self,
        _: &'static str,
        _: usize,
    ) -> Result<Self::SerializeTupleStruct, KeyError> {
        Err(KeyError::Unsupported)
    }

    fn serialize_tuple_variant(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        _: usize,
    ) -> Result<Self::SerializeTupleVariant, KeyError> {
        Err(KeyError::Unsupported)
    }

    fn serialize_map(self, _: Option<usize>) -> Result<Self::SerializeMap, KeyError> {
        Err(KeyError::Unsupported)
    }

    fn serialize_struct(
        self,
        _: &'static str,
        _: usize,
    ) -> Result<Self::SerializeStruct, KeyError> {
        Err(KeyError::Unsupported)
    }

    fn serialize_struct_variant(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        _: usize,
    ) -> Result<Self::SerializeStructVariant, KeyError> {
        Err(KeyError::Unsupported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gdk_keycodes_map_to_wire_keys_and_back() {
        // KEY_ESC=1, KEY_F1=59, KEY_LEFTCTRL=29, KEY_L=38.
        let esc = key_from_gdk(1 + 8).unwrap();
        assert_eq!(key_name(esc), "Esc");
        assert_eq!(key_from_gdk(59 + 8).map(key_name), Some("F1".into()));
        assert_eq!(key_from_gdk(29 + 8).map(key_name), Some("LeftCtrl".into()));
        assert_eq!(key_from_gdk(38 + 8).map(key_name), Some("L".into()));
        assert!(key_from_gdk(7).is_none(), "below the evdev range");
        assert!(key_from_gdk(900).is_none(), "far outside the key range");
        assert_eq!(key_by_name("LeftMeta").map(key_name), Some("LeftMeta".into()));
        assert_eq!(key_by_name("NotAKey"), None);
    }

    #[test]
    fn captured_names_reparse_through_the_action_grammar() {
        // The names the capture widget records are exactly the names a
        // configuration's Action list accepts.
        for name in ["Esc", "F1", "LeftCtrl", "LeftMeta", "Space", "MicMute"] {
            let key = key_by_name(name).unwrap_or_else(|| panic!("{name} missing"));
            let doc = format!(
                "DefaultLayer = \"a\"\n[[Layers]]\nName = \"a\"\nKeys = [{{ Text = \"x\", Action = [\"{name}\"] }}]\n"
            );
            let parsed: dfr_config::ConfigProxy =
                toml::from_str(&doc).unwrap_or_else(|err| panic!("{name} rejected: {err}"));
            let action = &parsed.layers.as_ref().unwrap()[0].keys[0].action;
            assert_eq!(
                action,
                &dfr_config::ButtonAction::Keys(vec![key]),
                "{name} mismatched"
            );
        }
    }
}
