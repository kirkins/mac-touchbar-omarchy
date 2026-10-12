//! The offline validation path behind `tiny-dfr check`: run the exact
//! parse, validate, and resource-resolution steps against a candidate
//! file and collect every diagnostic. Scripts and CI share this with
//! the daemon's own load path, so "valid" means the same thing
//! everywhere.

use crate::defaults;
use crate::icons::IconSearch;
use crate::model;
use std::path::Path;

/// Validate `user` (defaulting semantics: it is the user overlay) over
/// `base`, resolving icons through `search` and matching the font
/// pattern. Returns every problem found, in a stable order; an empty
/// vector means the configuration is exactly as loadable as the
/// daemon's own path allows.
pub fn check_user_config(base: &Path, user: &Path, width: u16, search: &IconSearch) -> Vec<String> {
    let mut diagnostics = Vec::new();
    let loaded = match model::parse_config_files(base, Some(user), width) {
        Ok(loaded) => loaded,
        Err(err) => {
            diagnostics.push(format!("{err:#}"));
            return diagnostics;
        }
    };
    if let Err(err) = search.validate_layout(&loaded.layout) {
        diagnostics.push(format!("{err:#}"));
    }
    let template = loaded
        .proxy
        .font_template
        .clone()
        .unwrap_or_else(|| defaults::FONT_TEMPLATE.to_owned());
    if let Err(err) = crate::fonts::match_font(&template) {
        diagnostics.push(format!("FontTemplate: {err}"));
    }
    diagnostics
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn repo(relative: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(relative)
    }

    const BASE: &str = "../../share/tiny-dfr/config.toml";
    const EXAMPLE: &str = "../../share/tiny-dfr/multibar.toml";
    const ICONS: &str = "../../share/tiny-dfr";

    fn tempdir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("dfr-config-check-{}-{name}", std::process::id()))
    }

    #[test]
    fn the_shipped_example_checks_clean_against_the_repo_share_directory() {
        let base = repo(BASE);
        let user = repo(EXAMPLE);
        let icons = IconSearch::new([repo(ICONS)]);
        let diagnostics = check_user_config(&base, &user, 2008, &icons);
        assert!(diagnostics.is_empty(), "{:?}", diagnostics);
        // The wide bar synthesizes the same buttons plus esc.
        let diagnostics = check_user_config(&base, &user, 2170, &icons);
        assert!(diagnostics.is_empty(), "{:?}", diagnostics);
    }

    #[test]
    fn an_invalid_knob_reports_the_file_and_the_knob() {
        let dir = tempdir("invalid-knob");
        fs::create_dir_all(&dir).unwrap();
        let base = dir.join("base.toml");
        let user = dir.join("config.toml");
        fs::write(&base, include_str!("../../../share/tiny-dfr/config.toml")).unwrap();
        fs::write(&user, "ShowButtonOutline = false\n").unwrap();
        let icons = IconSearch::new([dir.as_os_str()]);
        let diagnostics = check_user_config(&base, &user, 2008, &icons);
        assert_eq!(diagnostics.len(), 1);
        let joined = diagnostics.join("\n");
        assert!(joined.contains("config.toml"), "{joined}");
        assert!(joined.contains("ShowButtonOutlines"), "{joined}");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_unresolvable_icon_is_a_diagnostic_not_a_crash() {
        let dir = tempdir("missing-icon");
        fs::create_dir_all(&dir).unwrap();
        let base = dir.join("base.toml");
        let user = dir.join("config.toml");
        fs::write(&base, include_str!("../../../share/tiny-dfr/config.toml")).unwrap();
        fs::write(
            &user,
            "Layers = [{ Name = \"home\", Keys = [{ Icon = \"definitely_missing\" }] }]\n\
             DefaultLayer = \"home\"\n",
        )
        .unwrap();
        let icons = IconSearch::new([dir.join("icons")]);
        let diagnostics = check_user_config(&base, &user, 2008, &icons);
        let joined = diagnostics.join("\n");
        assert!(joined.contains("definitely_missing"), "{joined}");
        fs::remove_dir_all(&dir).unwrap();
    }
}
