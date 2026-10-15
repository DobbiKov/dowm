//! Hot-reload detection, as a pure function of the file system: [`WatchState::poll`].
//!
//! The platform shell calls it every ~500 ms from a plain thread. Each call reads the
//! file and compares a hash of its *content* with the last one it looked at, so it works
//! with editors that save by writing a temp file and renaming it over the original
//! (the inode changes, the path stays) and ignores a plain `touch`. A config files is a
//! few kilobytes, so reading it twice a second is cheaper than being clever about mtimes.
//!
//! A change must be seen with the *same* content on two consecutive polls before it is
//! parsed and applied, so an editor's half-written file is never applied (nor reported
//! as a syntax error). `reload-config` ([`WatchState::force`]) skips that wait.
//!
//! A file that is momentarily missing (mid-rename) is not a change: `poll` returns
//! `None` and tries again next time. A new content that fails to parse is reported once
//! as `Some(Err(..))`; the caller logs it and keeps its running config.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use super::{Config, ConfigErrors, ConfigSource, parse};

/// What the watcher has seen of the file so far.
#[derive(Debug)]
pub struct WatchState {
    source: ConfigSource,
    /// Hash of the content last reported (or seeded).
    last_hash: Option<u64>,
    /// A new content seen once, waiting to be seen again before it counts.
    pending: Option<u64>,
    /// `force` was called: the next poll reports without waiting.
    forced: bool,
}

fn hash_of(text: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

impl WatchState {
    /// A watcher that reports the file's current content on the first poll.
    pub fn new(source: ConfigSource) -> Self { WatchState { source, last_hash: None, pending: None, forced: false } }

    /// A watcher that treats the file's current content as already applied (the daemon
    /// has just loaded it), so only later changes are reported.
    pub fn seeded(source: ConfigSource) -> Self {
        let last_hash = source
            .path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map(|text| hash_of(&text));
        WatchState { source, last_hash, pending: None, forced: false }
    }

    /// A watcher that treats `text`, which the daemon has just parsed, as already applied.
    pub fn seeded_from_text(source: ConfigSource, text: &str) -> Self {
        WatchState { source, last_hash: Some(hash_of(text)), pending: None, forced: false }
    }

    pub fn source(&self) -> &ConfigSource { &self.source }

    /// Make the next poll report the file even if it did not change (`reload-config`).
    pub fn force(&mut self) {
        self.last_hash = None;
        self.forced = true;
    }

    /// `Some(result)` when the file's content differs from the last one reported; `None`
    /// when nothing changed, the file cannot be read right now, or the config is the
    /// built-in one (which never changes).
    pub fn poll(&mut self) -> Option<Result<Config, ConfigErrors>> {
        let path = self.source.path()?;
        let text = std::fs::read_to_string(path).ok()?;
        let hash = hash_of(&text);
        if self.last_hash == Some(hash) {
            self.pending = None;
            return None;
        }
        if !self.forced && self.pending != Some(hash) {
            self.pending = Some(hash);
            return None;
        }
        self.forced = false;
        self.pending = None;
        self.last_hash = Some(hash);
        Some(parse(&text))
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    /// A poll that waits for the content to settle: the second poll of a new content.
    fn settle(state: &mut WatchState) -> Option<Result<Config, ConfigErrors>> {
        state.poll().or_else(|| state.poll())
    }

    const VALID: &str = "[mode.main.binding]\nalt-h = 'focus left'\n";
    const VALID_2: &str = "[mode.main.binding]\nalt-j = 'focus down'\n";

    /// A scratch directory removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            static N: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir()
                .join(format!("dowm-watch-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
            std::fs::create_dir_all(&dir).expect("create scratch dir");
            Scratch(dir)
        }

        fn file(&self) -> PathBuf { self.0.join("dowm.toml") }

        fn write(&self, text: &str) { std::fs::write(self.file(), text).expect("write config"); }

        fn state(&self) -> WatchState { WatchState::new(ConfigSource::File(self.file())) }
    }

    impl Drop for Scratch {
        fn drop(&mut self) { _ = std::fs::remove_dir_all(&self.0); }
    }

    #[test]
    fn the_first_poll_reports_the_current_content() {
        let dir = Scratch::new();
        dir.write(VALID);
        let mut state = dir.state();
        let config = settle(&mut state).expect("reported").expect("valid");
        assert!(config.modes.contains_key("main"));
        assert!(settle(&mut state).is_none(), "nothing changed since");
    }

    #[test]
    fn a_seeded_watcher_reports_only_later_changes() {
        let dir = Scratch::new();
        dir.write(VALID);
        let mut state = WatchState::seeded(ConfigSource::File(dir.file()));
        assert!(settle(&mut state).is_none());
        dir.write(VALID_2);
        assert!(settle(&mut state).expect("changed").is_ok());
        assert!(settle(&mut state).is_none());
    }

    #[test]
    fn an_edit_is_reported_once_and_a_touch_is_not() {
        let dir = Scratch::new();
        dir.write(VALID);
        let mut state = WatchState::seeded(ConfigSource::File(dir.file()));
        dir.write(VALID); // same content, new mtime
        assert!(settle(&mut state).is_none(), "unchanged content is not a reload");
        dir.write(VALID_2);
        assert!(settle(&mut state).is_some());
        assert!(settle(&mut state).is_none());
    }

    #[test]
    fn an_atomic_rename_save_is_detected() {
        let dir = Scratch::new();
        dir.write(VALID);
        let mut state = WatchState::seeded(ConfigSource::File(dir.file()));
        // What many editors do: write a sibling, then rename it over the original.
        let temp = dir.0.join("dowm.toml.tmp");
        std::fs::write(&temp, VALID_2).unwrap();
        std::fs::rename(&temp, dir.file()).unwrap();
        let config = settle(&mut state).expect("reported").expect("valid");
        assert!(config.modes["main"].bindings.len() == 1);
        assert_eq!(config.modes["main"].bindings.values().next().unwrap().chord, "alt-j");
    }

    #[test]
    fn a_missing_file_is_not_a_change_and_recovers() {
        let dir = Scratch::new();
        dir.write(VALID);
        let mut state = WatchState::seeded(ConfigSource::File(dir.file()));
        std::fs::remove_file(dir.file()).unwrap();
        assert!(settle(&mut state).is_none(), "mid-rename: try again later");
        dir.write(VALID);
        assert!(settle(&mut state).is_none(), "back with the same content: still no change");
        dir.write(VALID_2);
        assert!(settle(&mut state).is_some());
    }

    #[test]
    fn an_invalid_config_is_reported_once_with_its_errors() {
        let dir = Scratch::new();
        dir.write(VALID);
        let mut state = WatchState::seeded(ConfigSource::File(dir.file()));
        dir.write("[mode.main.binding]\nalt-h = 'bogus-command'\n");
        let errors = settle(&mut state).expect("reported").expect_err("invalid");
        assert!(errors.mentions("mode.main.binding.alt-h"), "{errors}");
        assert!(settle(&mut state).is_none(), "the same broken content is not re-reported every tick");
        dir.write(VALID_2);
        assert!(settle(&mut state).expect("fixed").is_ok(), "fixing the file reloads it");
    }

    #[test]
    fn force_makes_the_next_poll_report_unchanged_content() {
        let dir = Scratch::new();
        dir.write(VALID);
        let mut state = WatchState::seeded(ConfigSource::File(dir.file()));
        assert!(settle(&mut state).is_none());
        state.force();
        assert!(settle(&mut state).expect("forced").is_ok());
        assert!(settle(&mut state).is_none());
    }

    #[test]
    fn the_builtin_config_never_changes() {
        let mut state = WatchState::new(ConfigSource::BuiltIn);
        assert!(settle(&mut state).is_none());
        assert!(settle(&mut WatchState::seeded(ConfigSource::BuiltIn)).is_none());
        assert_eq!(*state.source(), ConfigSource::BuiltIn);
    }

    #[test]
    fn a_syntax_error_is_an_error_not_a_panic() {
        let dir = Scratch::new();
        dir.write("this is = = not toml [[[");
        let mut state = dir.state();
        let errors = settle(&mut state).unwrap().unwrap_err();
        assert!(errors.to_string().contains("syntax"), "{errors}");
    }

    #[test]
    fn a_new_content_must_be_seen_twice_before_it_counts() {
        let dir = Scratch::new();
        dir.write(VALID);
        let mut state = WatchState::seeded(ConfigSource::File(dir.file()));
        dir.write(VALID_2);
        assert!(state.poll().is_none(), "first sighting: wait");
        assert!(state.poll().expect("second sighting").is_ok());
    }

    #[test]
    fn a_half_written_file_is_never_applied_or_reported() {
        let dir = Scratch::new();
        dir.write(VALID);
        let mut state = WatchState::seeded(ConfigSource::File(dir.file()));
        dir.write("[mode.main.binding]\nalt-j = 'foc"); // the editor is mid-write
        assert!(state.poll().is_none());
        dir.write(VALID_2); // finished before the next poll
        assert!(state.poll().is_none(), "different content again: still waiting");
        assert!(state.poll().expect("settled").is_ok(), "only the complete file is parsed");
    }

    #[test]
    fn reverting_before_the_second_sighting_cancels_the_wait() {
        let dir = Scratch::new();
        dir.write(VALID);
        let mut state = WatchState::seeded(ConfigSource::File(dir.file()));
        dir.write(VALID_2);
        assert!(state.poll().is_none());
        dir.write(VALID);
        assert!(state.poll().is_none());
        dir.write(VALID_2);
        assert!(state.poll().is_none(), "the earlier sighting no longer counts");
    }

    #[test]
    fn force_reports_at_once_without_waiting() {
        let dir = Scratch::new();
        dir.write(VALID);
        let mut state = WatchState::seeded(ConfigSource::File(dir.file()));
        dir.write(VALID_2);
        state.force();
        assert!(state.poll().expect("immediate").is_ok());
    }

    #[test]
    fn seeding_from_text_takes_the_parsed_text_not_a_second_read() {
        let dir = Scratch::new();
        dir.write(VALID_2); // the file changed after the daemon parsed VALID
        let mut state = WatchState::seeded_from_text(ConfigSource::File(dir.file()), VALID);
        assert!(settle(&mut state).expect("the change since the parse is reported").is_ok());
    }
}
