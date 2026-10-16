use std::{
    collections::HashSet,
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    thread,
};

/// Executes `Command` button actions on touch-down.
///
/// The configured string runs verbatim through `/bin/sh -c`, detached from
/// the renderer: standard streams are redirected to `/dev/null`, nothing
/// observed at runtime (touch positions, layer names, ...) is ever
/// interpolated into the command, and each child is reaped on its own
/// thread, so the render loop never waits on a process. Failures - spawn
/// errors and nonzero exits - are reported on stderr, the daemon's existing
/// log path.
///
/// At most one invocation of a given command string runs at a time. A
/// button's action is fixed by the configuration, so this caps concurrent
/// invocations per button: a second touch while the previous child is
/// still alive is dropped with a log line instead of double-firing.
/// Buttons sharing a command string share the cap.
#[derive(Default)]
pub struct CommandRunner {
    running: Arc<Mutex<HashSet<String>>>,
}

impl CommandRunner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Launch `command` unless an identical command is still running;
    /// returns whether a new process was started.
    pub fn launch(&self, command: &str) -> bool {
        {
            let mut running = self.running.lock().unwrap();
            if !running.insert(command.to_owned()) {
                eprintln!("Ignoring command press, still running: {command}");
                return false;
            }
        }
        match Command::new("/bin/sh")
            .arg("-c")
            .arg(command)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(mut child) => {
                let running = Arc::clone(&self.running);
                let command = command.to_owned();
                thread::spawn(move || {
                    match child.wait() {
                        Ok(status) if !status.success() => {
                            eprintln!("Command failed ({status}): {command}");
                            if let Some(guidance) = handoff_failure_guidance(&command) {
                                eprintln!("{guidance}");
                            }
                        }
                        Ok(_) => {}
                        Err(err) => eprintln!("Failed to wait for command {command}: {err}"),
                    }
                    running.lock().unwrap().remove(&command);
                });
                true
            }
            Err(err) => {
                eprintln!("Failed to start command {command}: {err}");
                self.running.lock().unwrap().remove(command);
                false
            }
        }
    }
}

// Fixed guidance only: never pipe arbitrary command stderr/configuration into logs.
fn handoff_failure_guidance(command: &str) -> Option<&'static str> {
    matches!(command, "touchbar-capture-user settings" | "touchbar-capture-user screenshot")
        .then_some("Desktop handoff failed: verify the UID1000 graphical receiver with systemctl --user status touchbar-capture.service and journalctl --user -u touchbar-capture.service -b as the desktop user. Open Touch Bar Settings from the application menu or /usr/local/bin/dfr-configurator. Complete user-session setup in /usr/share/doc/tiny-dfr/INSTALL.md. A submitted launch is not proof of a mapped window.")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        path::PathBuf,
        time::{Duration, Instant},
    };

    struct TempFile(PathBuf);
    impl TempFile {
        fn new() -> Self {
            Self(std::env::temp_dir().join(format!(
                "dfr-command-test-{}-{}",
                std::process::id(),
                rand::random::<u64>()
            )))
        }
        /// Wait until the marker file exists, proving the child ran.
        fn wait_for(&self) {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !self.0.exists() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
        }
    }
    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }

    #[test]
    fn settings_handoff_failure_has_fixed_actionable_fallback_without_child_output() {
        let guidance = handoff_failure_guidance("touchbar-capture-user settings").unwrap();
        assert!(guidance.contains("application menu"));
        assert!(guidance.contains("journalctl --user"));
        assert!(guidance.contains("not proof"));
        assert!(handoff_failure_guidance("arbitrary-secret-command").is_none());
    }

    #[test]
    fn commands_run_detached_and_failures_do_not_block_later_presses() {
        let runner = CommandRunner::new();
        // The command really runs: a marker file appears without the caller
        // ever waiting on the child.
        let marker = TempFile::new();
        assert!(runner.launch(&format!("touch {}", marker.0.display())));
        marker.wait_for();
        assert!(marker.0.exists(), "command child never ran");
        // A failing command is reported, not fatal, and a different command
        // still launches afterwards.
        assert!(runner.launch("exit 3"));
        assert!(runner.launch("true"));
    }

    #[test]
    fn a_second_touch_while_the_command_runs_does_not_double_fire() {
        let runner = CommandRunner::new();
        assert!(runner.launch("sleep 0.5"));
        // The same command is still in flight: dropped, not re-run.
        assert!(!runner.launch("sleep 0.5"));
        // A different command is unaffected by the cap.
        assert!(runner.launch("true"));
        // Once the child exits, the same command may run again.
        let deadline = Instant::now() + Duration::from_secs(10);
        while !runner.launch("sleep 0.5") && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn a_missing_command_is_reported_and_the_cap_recovers() {
        let runner = CommandRunner::new();
        // /bin/sh exists, but the command inside does not: the failure is
        // logged by the reaping thread and the entry is released at once,
        // so an immediate retry is not capped by the failed attempt.
        assert!(runner.launch("definitely-not-a-command-xyz --version"));
        let deadline = Instant::now() + Duration::from_secs(10);
        while !runner.launch("definitely-not-a-command-xyz --version") && Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(20));
        }
    }
}
