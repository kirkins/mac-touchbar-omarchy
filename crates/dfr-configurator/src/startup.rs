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

#[cfg(test)]
mod tests {
    use super::*;

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
