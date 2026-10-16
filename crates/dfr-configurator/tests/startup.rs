//! Executable startup coverage with no usable display/backend/session bus.
//! Never run against a live graphical session, installed config or credentials.
use std::path::Path;
use std::process::{Command, Output};

fn launch(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_dfr-configurator"))
        .args(args)
        .env_clear()
        .env("HOME", root)
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_RUNTIME_DIR", root.join("runtime"))
        .env(
            "DBUS_SESSION_BUS_ADDRESS",
            format!("unix:path={}/no-session-bus", root.display()),
        )
        // An unsupported backend cannot contact X11/Wayland, even if inherited
        // display settings were accidentally restored in a future test change.
        .env("GDK_BACKEND", "dfr-test-no-display")
        .env("GTK_A11Y", "none")
        .env("GSETTINGS_BACKEND", "memory")
        .output()
        .expect("isolated configurator subprocess")
}

#[test]
fn help_and_startup_failures_are_safe_without_a_graphical_session() {
    let root = std::env::temp_dir().join(format!("dfr-configurator-launch-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let base = root.join("base.toml");
    let draft = root.join("draft.toml");
    let help = launch(&root, &["--help"]);
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("user draft"));

    let args = [
        "--base",
        base.to_str().unwrap(),
        "--config",
        draft.to_str().unwrap(),
    ];
    for source in [None, Some("PRIVATE_SENTINEL = [broken")] {
        if let Some(source) = source {
            std::fs::write(&base, source).unwrap();
        }
        let output = launch(&root, &args);
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("No files were changed"), "{stderr}");
        assert!(stderr.contains("journalctl --user"), "{stderr}");
        assert!(stderr.contains("tiny-dfr check"), "{stderr}");
        assert!(!stderr.contains("PRIVATE_SENTINEL"), "{stderr}");
        assert!(stderr.len() < 8192, "startup feedback must be bounded");
        assert!(!draft.exists());
    }
    std::fs::write(&base, include_str!("../../../share/tiny-dfr/config.toml")).unwrap();
    std::fs::write(&draft, "PRIVATE_DRAFT_SENTINEL = [broken").unwrap();
    let output = launch(&root, &args);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(stderr.contains("No files were changed"), "{stderr}");
    assert!(!stderr.contains("PRIVATE_DRAFT_SENTINEL"), "{stderr}");
    assert_eq!(
        std::fs::read_to_string(&draft).unwrap(),
        "PRIVATE_DRAFT_SENTINEL = [broken"
    );
    std::fs::remove_dir_all(root).unwrap();
}
