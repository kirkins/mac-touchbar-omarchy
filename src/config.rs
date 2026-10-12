//! The daemon's configuration runtime. The whole configuration model -
//! wire types, merge, validation, defaults, the canonical writer, and
//! the `load_config` path that builds a render deck from a config -
//! lives in the shared crates: `dfr-config` for the contract and
//! `dfr-render` for the deck-building loader. This module owns what is
//! daemon-specific: the inotify watcher that hot-reloads the merged
//! base + user configuration, and the paths the watcher is armed on.

use anyhow::Result;
use dfr_render::config::{load_config, write_persisted_theme, THEME_STATE_PATH};
use nix::{
    errno::Errno,
    sys::inotify::{AddWatchFlags, InitFlags, Inotify, InotifyEvent, WatchDescriptor},
};
use std::{
    os::fd::AsFd,
    path::{Path, PathBuf},
};

pub use dfr_render::config::{BASE_CFG_PATH, USER_CFG_PATH};
pub use dfr_render::{Config, LayerDeck};

pub struct ConfigManager {
    inotify_fd: Inotify,
    dir_watch: Option<WatchDescriptor>,
    file_watch: Option<WatchDescriptor>,
    user_path: PathBuf,
    base_path: PathBuf,
    state_path: PathBuf,
}

fn arm_dir_watch(inotify_fd: &Inotify, user_path: &Path) -> Option<WatchDescriptor> {
    // Watch the parent to handle both direct writes and editors' atomic renames.
    let flags = AddWatchFlags::IN_MOVED_TO
        | AddWatchFlags::IN_MOVED_FROM
        | AddWatchFlags::IN_CLOSE_WRITE
        | AddWatchFlags::IN_DELETE
        | AddWatchFlags::IN_DELETE_SELF
        | AddWatchFlags::IN_MOVE_SELF;
    match inotify_fd.add_watch(user_path.parent().unwrap(), flags) {
        Ok(wd) => Some(wd),
        Err(Errno::ENOENT) => None,
        e => Some(e.unwrap()),
    }
}

fn arm_file_watch(inotify_fd: &Inotify, user_path: &Path) -> Option<WatchDescriptor> {
    let flags =
        AddWatchFlags::IN_CLOSE_WRITE | AddWatchFlags::IN_MOVE_SELF | AddWatchFlags::IN_DELETE_SELF;
    match inotify_fd.add_watch(user_path, flags) {
        Ok(wd) => Some(wd),
        Err(Errno::ENOENT) => None,
        e => Some(e.unwrap()),
    }
}

impl ConfigManager {
    pub fn new() -> ConfigManager {
        Self::with_paths(
            PathBuf::from(BASE_CFG_PATH),
            PathBuf::from(USER_CFG_PATH),
            PathBuf::from(THEME_STATE_PATH),
        )
    }
    fn with_paths(base_path: PathBuf, user_path: PathBuf, state_path: PathBuf) -> ConfigManager {
        let inotify_fd = Inotify::init(InitFlags::IN_NONBLOCK).unwrap();
        let dir_watch = arm_dir_watch(&inotify_fd, &user_path);
        let file_watch = arm_file_watch(&inotify_fd, &user_path);
        ConfigManager {
            inotify_fd,
            dir_watch,
            file_watch,
            user_path,
            base_path,
            state_path,
        }
    }
    /// Persist a theme chosen on the glass for the next start; atomic
    /// and best-effort, exactly like the load-time persistence.
    pub fn save_theme(&self, name: &str) -> std::io::Result<()> {
        write_persisted_theme(&self.state_path, name)
    }
    pub fn load_config(&self, width: u16) -> (Config, LayerDeck) {
        load_config(
            width,
            &self.base_path,
            Some(&self.user_path),
            Some(&self.state_path),
        )
        .unwrap_or_else(|err| {
            eprintln!("Ignoring user configuration: {err:#}");
            load_config(width, &self.base_path, None, Some(&self.state_path))
                .expect("failed to load base configuration")
        })
    }
    pub fn update_config(&mut self, width: u16) -> Option<(Config, LayerDeck)> {
        if !self.pending_reload() {
            return None;
        }
        match load_config(
            width,
            &self.base_path,
            Some(&self.user_path),
            Some(&self.state_path),
        ) {
            Ok(parts) => Some(parts),
            Err(err) => {
                eprintln!("Keeping current configuration after reload error: {err:#}");
                None
            }
        }
    }
    fn pending_reload(&mut self) -> bool {
        let mut pending = false;
        if self.dir_watch.is_none() {
            self.dir_watch = arm_dir_watch(&self.inotify_fd, &self.user_path);
            pending = self.dir_watch.is_some();
        }
        if self.file_watch.is_none() {
            self.file_watch = arm_file_watch(&self.inotify_fd, &self.user_path);
            pending |= self.file_watch.is_some();
        }
        match self.inotify_fd.read_events() {
            Err(Errno::EAGAIN) => pending,
            r => self.handle_events(r) || pending,
        }
    }
    #[cold]
    fn handle_events(&mut self, evts: Result<Vec<InotifyEvent>, Errno>) -> bool {
        let mut ret = false;
        for evt in evts.unwrap() {
            if Some(evt.wd) == self.dir_watch {
                if evt.mask.intersects(
                    AddWatchFlags::IN_IGNORED
                        | AddWatchFlags::IN_DELETE_SELF
                        | AddWatchFlags::IN_MOVE_SELF,
                ) {
                    self.dir_watch = None;
                }
                if evt.name.as_deref() == self.user_path.file_name() {
                    self.rearm_file_watch();
                    ret = true;
                }
            } else if Some(evt.wd) == self.file_watch {
                if evt
                    .mask
                    .intersects(AddWatchFlags::IN_MOVE_SELF | AddWatchFlags::IN_DELETE_SELF)
                {
                    self.rearm_file_watch();
                    ret = true;
                } else if evt.mask.intersects(AddWatchFlags::IN_IGNORED) {
                    self.file_watch = arm_file_watch(&self.inotify_fd, &self.user_path);
                } else if evt.mask.intersects(AddWatchFlags::IN_CLOSE_WRITE) {
                    ret = true;
                }
            }
        }
        ret
    }
    fn rearm_file_watch(&mut self) {
        if let Some(wd) = self.file_watch.take() {
            let _ = self.inotify_fd.rm_watch(wd);
        }
        self.file_watch = arm_file_watch(&self.inotify_fd, &self.user_path);
    }
    pub fn fd(&self) -> &impl AsFd {
        &self.inotify_fd
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use input_linux::Key;
    use std::{fs, os::unix::fs::symlink, path::PathBuf};

    const BASE: &str = include_str!("../share/tiny-dfr/config.toml");
    const NAMED: &str = r#"
DefaultLayer = "home"
FnLayer = "function"
[[Layers]]
Name = "home"
Keys = [{ Text = "Functions", Action = { Layer = "function" } }]
[[Layers]]
Name = "function"
Keys = [{ Text = "F1", Action = "F1" }]
"#;

    struct TempDir(PathBuf);
    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "dfr-config-manager-test-{}-{}",
                std::process::id(),
                rand::random::<u64>()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    fn text_only_base() -> String {
        let mut base: toml::Value = toml::from_str(BASE).unwrap();
        let keys: toml::Value = toml::from_str(
            r#"
PrimaryLayerKeys = [{ Text = "F1", Action = "F1" }]
MediaLayerKeys = [{ Text = "Volume", Action = "VolumeUp" }]
"#,
        )
        .unwrap();
        for (name, value) in keys.as_table().unwrap() {
            base[name] = value.clone();
        }
        toml::to_string(&base).unwrap()
    }

    #[test]
    fn watcher_handles_direct_writes_atomic_saves_rename_away_and_deletion() {
        let tmp = TempDir::new();
        let path = tmp.0.join("config.toml");
        let mut manager = ConfigManager::with_paths(
            tmp.0.join("base.toml"),
            path.clone(),
            tmp.0.join("active-theme"),
        );
        assert!(!manager.pending_reload());
        fs::write(tmp.0.join("unrelated"), "ignored").unwrap();
        assert!(!manager.pending_reload());
        fs::write(&path, NAMED).unwrap();
        assert!(manager.pending_reload());
        assert!(!manager.pending_reload());
        for _ in 0..2 {
            let stage = tmp.0.join("config.new");
            fs::write(&stage, NAMED).unwrap();
            fs::rename(stage, &path).unwrap();
            assert!(manager.pending_reload());
            assert!(!manager.pending_reload());
        }
        let backup = tmp.0.join("config.backup");
        fs::rename(&path, &backup).unwrap();
        assert!(manager.pending_reload());
        assert!(!manager.pending_reload());
        fs::rename(backup, &path).unwrap();
        assert!(manager.pending_reload());
        fs::remove_file(path).unwrap();
        assert!(manager.pending_reload());
        assert!(!manager.pending_reload());
    }

    #[test]
    fn symlinked_config_reloads_in_place_writes_and_target_replacements() {
        let tmp = TempDir::new();
        let base = tmp.0.join("base.toml");
        fs::write(&base, text_only_base()).unwrap();
        let dotfiles = tmp.0.join("dotfiles");
        fs::create_dir(&dotfiles).unwrap();
        let target = dotfiles.join("config.toml");
        fs::write(&target, NAMED).unwrap();
        let path = tmp.0.join("config.toml");
        symlink(&target, &path).unwrap();

        let mut manager = ConfigManager::with_paths(base, path.clone(), tmp.0.join("active-theme"));
        let (_, mut deck) = manager.load_config(2008);
        assert!(!deck.legacy);
        assert_eq!(deck.layers.len(), 4);
        let mut events = Vec::new();
        let mut emit = |keys: &[Key], active| events.push((keys.to_vec(), active));
        deck.switch_named("function", &mut emit);
        deck.touch_down(0, 1, 1500.0, 30.0, &mut emit, &mut |_| {}, &mut |_| None);
        drop(emit);
        assert_eq!(events, vec![(vec![Key::F1], true)]);

        fs::write(&path, NAMED.replace("F1", "F5")).unwrap();
        let (_, replacement) = manager.update_config(2008).unwrap();
        let mut emit = |keys: &[Key], active| events.push((keys.to_vec(), active));
        deck.replace(replacement, &mut emit);
        drop(emit);
        assert_eq!(events, vec![(vec![Key::F1], true), (vec![Key::F1], false)]);
        events.clear();
        let mut emit = |keys: &[Key], active| events.push((keys.to_vec(), active));
        deck.switch_named("function", &mut emit);
        deck.touch_down(0, 1, 1500.0, 30.0, &mut emit, &mut |_| {}, &mut |_| None);
        drop(emit);
        assert_eq!(events, vec![(vec![Key::F5], true)]);

        let stage = dotfiles.join("config.new");
        fs::write(&stage, NAMED.replace("F1", "F6")).unwrap();
        fs::rename(&stage, &target).unwrap();
        let (_, replacement) = manager.update_config(2008).unwrap();
        let mut emit = |keys: &[Key], active| events.push((keys.to_vec(), active));
        deck.replace(replacement, &mut emit);
        drop(emit);
        assert_eq!(events, vec![(vec![Key::F5], true), (vec![Key::F5], false)]);
        events.clear();
        let mut emit = |keys: &[Key], active| events.push((keys.to_vec(), active));
        deck.switch_named("function", &mut emit);
        deck.touch_down(0, 1, 1500.0, 30.0, &mut emit, &mut |_| {}, &mut |_| None);
        drop(emit);
        assert_eq!(events, vec![(vec![Key::F6], true)]);
    }

    #[test]
    fn invalid_reload_keeps_current_deck_and_next_valid_save_recovers() {
        let tmp = TempDir::new();
        let base = tmp.0.join("base.toml");
        fs::write(&base, text_only_base()).unwrap();
        let path = tmp.0.join("config.toml");
        fs::write(&path, NAMED).unwrap();
        let mut manager = ConfigManager::with_paths(base, path.clone(), tmp.0.join("active-theme"));
        let (_, mut deck) = manager.load_config(2008);
        deck.switch_named("function", &mut |_, _| {});
        let mut keys = Vec::new();
        let mut emit = |codes: &[Key], active| keys.push((codes.to_vec(), active));
        deck.touch_down(7, 1, 1500.0, 30.0, &mut emit, &mut |_| {}, &mut |_| None);
        fs::write(
            &path,
            NAMED.replace("Layer = \"function\"", "Layer = \"missing\""),
        )
        .unwrap();
        assert!(manager.update_config(2008).is_none());
        assert_eq!(deck.active, 1);
        assert!(deck.current().buttons[1].1.active);
        fs::write(&path, NAMED).unwrap();
        let (_, replacement) = manager.update_config(2008).unwrap();
        deck.replace(replacement, &mut emit);
        assert_eq!(deck.active, deck.home);
        assert!(deck.touches.is_empty());
        assert_eq!(keys, vec![(vec![Key::F1], true), (vec![Key::F1], false)]);
    }

    #[test]
    fn invalid_startup_and_deleted_user_config_use_the_base_layers() {
        let tmp = TempDir::new();
        let base = tmp.0.join("base.toml");
        fs::write(&base, text_only_base()).unwrap();
        let path = tmp.0.join("config.toml");
        fs::write(
            &path,
            NAMED.replace("Layer = \"function\"", "Layer = \"missing\""),
        )
        .unwrap();
        let mut manager = ConfigManager::with_paths(base, path.clone(), tmp.0.join("active-theme"));
        let (_, mut deck) = manager.load_config(2008);
        assert!(deck.legacy);
        assert_eq!(deck.active, 0);
        assert_eq!(deck.layers.len(), 4);

        fs::write(&path, NAMED).unwrap();
        let (_, replacement) = manager.update_config(2008).unwrap();
        let mut events = Vec::new();
        let mut emit = |keys: &[Key], active| events.push((keys.to_vec(), active));
        deck.replace(replacement, &mut emit);
        assert!(!deck.legacy);
        deck.switch_named("function", &mut emit);
        deck.touch_down(0, 1, 1500.0, 30.0, &mut emit, &mut |_| {}, &mut |_| None);

        fs::remove_file(path).unwrap();
        let (_, replacement) = manager.update_config(2008).unwrap();
        deck.replace(replacement, &mut emit);
        assert!(deck.legacy);
        assert_eq!(deck.active, deck.home);
        assert_eq!(deck.layers.len(), 4);
        assert!(deck.touches.is_empty());
        deck.touch_up(0, &mut emit);
        assert_eq!(events, vec![(vec![Key::F1], true), (vec![Key::F1], false)]);
    }
}
