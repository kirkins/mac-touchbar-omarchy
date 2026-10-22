//! Headless launch options, shared by the desktop entry and session receiver.
use anyhow::{bail, Context, Result};
use std::path::PathBuf;

pub struct Options {
    pub base: PathBuf,
    pub user: PathBuf,
    pub draft: bool,
}

pub fn default_draft(config_home: Option<PathBuf>, home: Option<PathBuf>) -> Result<PathBuf> {
    let root = match config_home.filter(|path| !path.as_os_str().is_empty()) {
        Some(path) if path.is_absolute() => path,
        _ => home
            .filter(|path| path.is_absolute())
            .context("HOME must be an absolute path")?
            .join(".config"),
    };
    Ok(root.join("dfr-deck/config.toml"))
}

pub fn parse(
    args: impl IntoIterator<Item = String>,
    config_home: Option<PathBuf>,
    home: Option<PathBuf>,
) -> Result<Option<Options>> {
    let mut base = PathBuf::from("/usr/share/tiny-dfr/config.toml");
    let mut user = None;
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" | "-c" => {
                user = Some(PathBuf::from(
                    args.next().context("--config requires a path")?,
                ))
            }
            "--base" | "-b" => base = PathBuf::from(args.next().context("--base requires a path")?),
            "--help" | "-h" => return Ok(None),
            other => bail!("unknown option {other}; see --help"),
        }
    }
    let draft = user.is_none();
    Ok(Some(Options {
        base,
        user: user
            .map(Ok)
            .unwrap_or_else(|| default_draft(config_home, home))?,
        draft,
    }))
}

/// Do not put parser diagnostics in notifications, startup UI or the journal:
/// TOML errors may quote configuration contents (including command secrets).
#[derive(Debug)]
pub struct Problem {
    pub heading: &'static str,
    pub body: &'static str,
}

impl Problem {
    pub fn options() -> Self {
        Self {
            heading: "Cannot start Touch Bar Settings",
            body: "Invalid launch options or no absolute HOME. Run dfr-configurator --help in a terminal. Launch Settings as the ordinary graphical desktop user, never with sudo. See INSTALL.md (https://github.com/kirkins/mac-touchbar-omarchy).",
        }
    }
}

pub fn open_document(
    options: &Options,
    installed: &std::path::Path,
    example: &std::path::Path,
) -> Result<crate::model::Document, Problem> {
    let result = if options.draft {
        crate::model::Document::open_draft(&options.base, &options.user, installed, example)
    } else {
        crate::model::Document::open(&options.base, &options.user)
    };
    result.map_err(|_| Problem {
        heading: "Cannot open Touch Bar Settings",
        body: "The base configuration or selected draft could not be read or parsed, or the full-deck example is missing. No files were changed. Check the base (--base; default /usr/share/tiny-dfr/config.toml) and draft (--config; default $XDG_CONFIG_HOME/dfr-deck/config.toml or $HOME/.config/dfr-deck/config.toml). If there is no draft, check /etc/tiny-dfr/config.toml and the installed example share/tiny-dfr-deck/multibar.toml. Back up an invalid draft before repairing it; do not delete it blindly. Use tiny-dfr-deck check --base BASE DRAFT in a private terminal for detailed diagnostics. For installation and administrator repair steps see INSTALL.md (https://github.com/kirkins/mac-touchbar-omarchy); journalctl --user -b contains session launch failures.",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_or_invalid_startup_files_have_bounded_private_feedback() {
        let root = std::env::temp_dir().join(format!("dfr-startup-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let base = root.join("base.toml");
        let user = root.join("draft.toml");
        let seed = root.join("example.toml");
        let options = Options {
            base: base.clone(),
            user: user.clone(),
            draft: true,
        };
        let problem = open_document(&options, &seed, &seed).err().unwrap();
        assert!(problem.body.contains("No files were changed"));
        std::fs::write(&base, include_str!("../../../share/tiny-dfr/config.toml")).unwrap();
        std::fs::write(&seed, include_str!("../../../share/tiny-dfr/multibar.toml")).unwrap();
        assert!(open_document(&options, &seed, &seed).is_ok());
        assert!(!user.exists(), "opening must not create a draft");
        for path in [&base, &user] {
            std::fs::write(path, "PRIVATE_SENTINEL = [broken").unwrap();
            let problem = open_document(&options, &seed, &seed).err().unwrap();
            assert!(!problem.body.contains("PRIVATE_SENTINEL"));
            assert!(problem.body.len() < 1200);
            assert!(problem.body.contains("tiny-dfr-deck check"));
            std::fs::write(path, include_str!("../../../share/tiny-dfr/config.toml")).unwrap();
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn desktop_and_receiver_defaults_select_an_ordinary_user_draft() {
        let options = parse(
            [],
            Some("/fixture/xdg".into()),
            Some("/fixture/home".into()),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            options.user,
            PathBuf::from("/fixture/xdg/dfr-deck/config.toml")
        );
        assert!(options.draft);
        let options = parse([], Some("relative".into()), Some("/fixture/home".into()))
            .unwrap()
            .unwrap();
        assert_eq!(
            options.user,
            PathBuf::from("/fixture/home/.config/dfr-deck/config.toml")
        );
    }

    #[test]
    fn explicit_configuration_and_bad_usage_are_handled_without_gtk() {
        let options = parse(
            ["--config".into(), "/fixture/custom.toml".into()],
            None,
            None,
        )
        .unwrap()
        .unwrap();
        assert!(!options.draft);
        assert_eq!(options.user, PathBuf::from("/fixture/custom.toml"));
        assert!(parse(["--config".into()], None, None).is_err());
        assert!(parse(["--base".into()], None, None).is_err());
        assert!(parse([], None, None).is_err());
        assert!(parse(["--help".into()], None, None).unwrap().is_none());
    }
}
