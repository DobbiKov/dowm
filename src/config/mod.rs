//! dowm's configuration: AeroSpace-compatible TOML, parsed into a typed, validated
//! [`Config`].
//!
//! Everything here is pure (no AX, no Cocoa); the platform shell that watches the file,
//! installs the hotkey tap and runs effects lives in `actor::input` and friends.
//!
//! - [`parse`] turns text into a [`Config`], or reports *every* problem with its key path.
//!   Unknown keys are errors (AeroSpace is strict), except a short list of AeroSpace keys
//!   dowm does not implement, which are accepted with a warning ([`IGNORED_KEYS`]).
//!   Every command string is parsed at load time, so a typo is a config error, and `mode
//!   <name>` must name a declared mode.
//! - The **workspace pool** is derived from the config: `1`..`9`, plus every other
//!   workspace name the config refers to (in a `workspace`/`move-node-to-workspace`
//!   command, an `on-window-detected` rule's `if.workspace`, a force-assignment key or
//!   `persistent-workspaces`), after the numbers, numeric names ascending then the
//!   rest alphabetically. So AeroSpace configs that send windows to workspace `T` or
//!   `B` work unchanged.
//! - [`resolve_source`] finds the file: `--config <path>`, else
//!   `~/.config/dowm/dowm.toml`, else `~/.dowm.toml`, else the built-in default.

pub mod command;
pub mod error;
pub mod gaps;
pub mod hotkey;
pub mod keys;
pub mod modes;
pub mod monitor;
pub mod rules;
pub(crate) mod schema;
pub mod watch;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

pub use command::{Action, CommandParseError, parse_command};
pub use error::{ConfigError, ConfigErrors};
pub use gaps::{GapValue, Gaps};
pub use hotkey::{Hotkey, HotkeyError, Mods};
pub use keys::KeyPreset;
pub use modes::{BindingModes, MAIN_MODE};
pub use rules::{WindowFacts, WindowRule, rules_to_run};

use self::error::join;
use self::monitor::parse_monitor_pattern;
use self::schema::Ctx;
use crate::model::tree::NormalizeOptions;
use crate::model::workspace::{ForceAssignment, MonitorPattern, WorkspaceConfig};

/// The embedded default config, used when no file exists.
pub const DEFAULT_CONFIG: &str = include_str!("default.toml");

/// Seconds one animation lasts unless configured.
pub const DEFAULT_ANIMATION_DURATION: f64 = 0.3;

/// Longer than this is a typo, not a style.
const MAX_ANIMATION_DURATION: f64 = 5.0;

/// AeroSpace's default `accordion-padding`.
pub const DEFAULT_ACCORDION_PADDING: u32 = 30;

/// AeroSpace keys that dowm accepts and ignores (with a warning in
/// [`Config::warnings`]), so a config written for AeroSpace still loads.
pub const IGNORED_KEYS: &[&str] = &[
    "config-version",
    "automatically-unhide-macos-hidden-apps",
    "exec-on-workspace-change",
    "exec",
];

/// The workspaces every config has.
pub const DEFAULT_WORKSPACES: std::ops::RangeInclusive<u32> = 1..=9;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RootLayout {
    #[default]
    Tiles,
    Accordion,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RootOrientation {
    Horizontal,
    Vertical,
    /// Wide monitors get horizontal roots, tall ones vertical.
    #[default]
    Auto,
}

/// One hotkey's commands, and the chord as the user wrote it.
#[derive(Clone, Debug, PartialEq)]
pub struct Binding {
    pub chord: String,
    pub actions: Vec<Action>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Mode {
    pub bindings: BTreeMap<Hotkey, Binding>,
}

/// A parsed, validated config.
#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    /// Parsed and validated, never run: dowm installs no login item (see README).
    pub after_login_command: Vec<Action>,
    /// Run once, after start-up has settled.
    pub after_startup_command: Vec<Action>,
    /// Parsed only; dowm does not manage launchd.
    pub start_at_login: bool,
    pub normalization: NormalizeOptions,
    pub accordion_padding: u32,
    pub default_root_layout: RootLayout,
    pub default_root_orientation: RootOrientation,
    pub key_mapping: KeyPreset,
    pub on_focused_monitor_changed: Vec<Action>,
    pub on_focus_changed: Vec<Action>,
    /// dowm-only: focus the window under the mouse.
    pub focus_follows_mouse: bool,
    /// dowm-only: slide windows on workspace switches and layout changes. Off: they jump.
    pub animations: bool,
    /// dowm-only: seconds one animation lasts.
    pub animation_duration: f64,
    pub gaps: Gaps,
    /// Every binding mode; `main` is always present.
    pub modes: BTreeMap<String, Mode>,
    pub window_rules: Vec<WindowRule>,
    /// The derived workspace pool, in order. See the module docs.
    pub workspaces: Vec<String>,
    /// `(workspace name, monitors tried in order)`.
    pub force_assignments: Vec<(String, Vec<MonitorPattern>)>,
    /// dowm-only (`workspace-auto-back-and-forth`).
    pub workspace_auto_back_and_forth: bool,
    /// Accepted but ignored keys, and the like. Not errors.
    pub warnings: Vec<String>,
}

impl Config {
    /// The built-in default config.
    pub fn builtin() -> Result<Config, ConfigErrors> { parse(DEFAULT_CONFIG) }

    /// What the workspace store is built from.
    pub fn workspace_config(&self) -> WorkspaceConfig {
        WorkspaceConfig {
            names: self.workspaces.clone(),
            auto_back_and_forth: self.workspace_auto_back_and_forth,
            force_assignments: self.force_assignments(),
        }
    }

    /// The force-assignments as the workspace model wants them (by pool position).
    pub fn force_assignments(&self) -> Vec<ForceAssignment> {
        self.force_assignments
            .iter()
            .filter_map(|(name, monitors)| {
                let position = self.workspaces.iter().position(|w| w == name)?;
                Some(ForceAssignment { workspace: position as u32 + 1, monitors: monitors.clone() })
            })
            .collect()
    }

    /// What `new` changes that a running dowm cannot apply live. Empty means everything
    /// applies. Today that is only the workspace pool.
    pub fn restart_required(&self, new: &Config) -> Vec<String> {
        let mut out = Vec::new();
        if self.workspaces != new.workspaces {
            out.push(format!(
                "the workspace pool changed ({} -> {}); this requires restart, keeping the old pool",
                self.workspaces.join(" "),
                new.workspaces.join(" ")
            ));
        }
        out
    }
}

/// Where the config comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigSource {
    File(PathBuf),
    BuiltIn,
}

impl ConfigSource {
    pub fn path(&self) -> Option<&Path> {
        match self {
            ConfigSource::File(p) => Some(p),
            ConfigSource::BuiltIn => None,
        }
    }
}

/// The config file to use: `cli` (even if it does not exist: loading it then fails
/// loudly), else `<home>/.config/dowm/dowm.toml`, else `<home>/.dowm.toml`, else the
/// built-in default. `exists` is the file-system check.
pub fn resolve_source(cli: Option<&Path>, home: Option<&Path>, exists: &dyn Fn(&Path) -> bool) -> ConfigSource {
    if let Some(path) = cli {
        return ConfigSource::File(path.to_path_buf());
    }
    let Some(home) = home else { return ConfigSource::BuiltIn };
    [home.join(".config").join("dowm").join("dowm.toml"), home.join(".dowm.toml")]
        .into_iter()
        .find(|p| exists(p))
        .map_or(ConfigSource::BuiltIn, ConfigSource::File)
}

#[derive(Debug)]
pub enum LoadError {
    Io { path: PathBuf, error: std::io::Error },
    Invalid(ConfigErrors),
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoadError::Io { path, error } => write!(f, "cannot read {}: {error}", path.display()),
            LoadError::Invalid(errors) => write!(f, "{errors}"),
        }
    }
}

impl std::error::Error for LoadError {}

/// Read and parse `source`.
pub fn load(source: &ConfigSource) -> Result<Config, LoadError> { load_text(source).map(|(config, _)| config) }

/// Like [`load`], also returning the exact text that was parsed (`None` for the built-in
/// config), so a watcher can be seeded from it without reading the file a second time.
pub fn load_text(source: &ConfigSource) -> Result<(Config, Option<String>), LoadError> {
    match source {
        ConfigSource::BuiltIn => Ok((Config::builtin().map_err(LoadError::Invalid)?, None)),
        ConfigSource::File(path) => {
            let text = std::fs::read_to_string(path)
                .map_err(|error| LoadError::Io { path: path.clone(), error })?;
            let config = parse(&text).map_err(LoadError::Invalid)?;
            Ok((config, Some(text)))
        }
    }
}

/// Parse and validate a config. Every problem is reported, each with its key path.
pub fn parse(text: &str) -> Result<Config, ConfigErrors> {
    let table: toml::Table = text.parse().map_err(|e: toml::de::Error| {
        ConfigErrors(vec![ConfigError { path: String::new(), message: format!("TOML syntax error: {e}") }])
    })?;
    let mut ctx = Ctx::default();
    let mut warnings = Vec::new();

    // The key preset decides what every chord means, so it is read first.
    let key_mapping = table.get("key-mapping").map_or(KeyPreset::Qwerty, |v| parse_key_mapping(v, &mut ctx));

    let mut config = Config {
        after_login_command: Vec::new(),
        after_startup_command: Vec::new(),
        start_at_login: false,
        normalization: NormalizeOptions::default(),
        accordion_padding: DEFAULT_ACCORDION_PADDING,
        default_root_layout: RootLayout::default(),
        default_root_orientation: RootOrientation::default(),
        key_mapping,
        on_focused_monitor_changed: Vec::new(),
        on_focus_changed: Vec::new(),
        focus_follows_mouse: false,
        animations: true,
        animation_duration: DEFAULT_ANIMATION_DURATION,
        gaps: Gaps::default(),
        modes: BTreeMap::new(),
        window_rules: Vec::new(),
        workspaces: Vec::new(),
        force_assignments: Vec::new(),
        workspace_auto_back_and_forth: false,
        warnings: Vec::new(),
    };
    let mut persistent = BTreeSet::new();
    let mut modes_seen = false;

    for (key, value) in &table {
        match key.as_str() {
            "key-mapping" => {}
            "after-login-command" => config.after_login_command = ctx.actions(value, key),
            "after-startup-command" => config.after_startup_command = ctx.actions(value, key),
            "start-at-login" => config.start_at_login = ctx.bool(value, key).unwrap_or(false),
            "enable-normalization-flatten-containers" => {
                config.normalization.flatten_containers =
                    ctx.bool(value, key).unwrap_or(config.normalization.flatten_containers);
            }
            "enable-normalization-opposite-orientation-for-nested-containers" => {
                config.normalization.opposite_orientation =
                    ctx.bool(value, key).unwrap_or(config.normalization.opposite_orientation);
            }
            "accordion-padding" => {
                config.accordion_padding = parse_padding(value, key, &mut ctx).unwrap_or(config.accordion_padding);
            }
            "default-root-container-layout" => {
                if let Some(word) = ctx.string(value, key) {
                    match word {
                        "tiles" => config.default_root_layout = RootLayout::Tiles,
                        "accordion" => config.default_root_layout = RootLayout::Accordion,
                        other => ctx.errors.push(key, format!("bad value `{other}`, expected tiles or accordion")),
                    }
                }
            }
            "default-root-container-orientation" => {
                if let Some(word) = ctx.string(value, key) {
                    match word {
                        "horizontal" => config.default_root_orientation = RootOrientation::Horizontal,
                        "vertical" => config.default_root_orientation = RootOrientation::Vertical,
                        "auto" => config.default_root_orientation = RootOrientation::Auto,
                        other => ctx
                            .errors
                            .push(key, format!("bad value `{other}`, expected horizontal, vertical or auto")),
                    }
                }
            }
            "on-focused-monitor-changed" => config.on_focused_monitor_changed = ctx.actions(value, key),
            "on-focus-changed" => config.on_focus_changed = ctx.actions(value, key),
            "focus-follows-mouse" => config.focus_follows_mouse = ctx.bool(value, key).unwrap_or(false),
            "animations" => config.animations = ctx.bool(value, key).unwrap_or(true),
            "animation-duration" => {
                let seconds = value.as_float().or_else(|| value.as_integer().map(|i| i as f64));
                match seconds {
                    Some(d) if d > 0.0 && d <= MAX_ANIMATION_DURATION => config.animation_duration = d,
                    Some(_) => ctx.errors.push(
                        key,
                        format!("must be above 0 and at most {MAX_ANIMATION_DURATION} seconds"),
                    ),
                    None => ctx
                        .errors
                        .push(key, format!("expected a number of seconds, found {}", value.type_str())),
                }
            }
            "workspace-auto-back-and-forth" => {
                config.workspace_auto_back_and_forth = ctx.bool(value, key).unwrap_or(false);
            }
            "gaps" => match value.as_table() {
                Some(t) => config.gaps = gaps::parse_gaps(t, key, &mut ctx.errors),
                None => ctx.errors.push(key, format!("expected a table, found {}", value.type_str())),
            },
            "mode" => {
                modes_seen = true;
                config.modes = parse_modes(value, key, config.key_mapping, &mut ctx);
            }
            "on-window-detected" => config.window_rules = rules::parse_rules(value, key, &mut ctx),
            "workspace-to-monitor-force-assignment" => {
                config.force_assignments = parse_force_assignments(value, key, &mut ctx);
            }
            "persistent-workspaces" => persistent = parse_workspace_list(value, key, &mut ctx),
            other if IGNORED_KEYS.contains(&other) => {
                warnings.push(format!("{other}: not supported by dowm, ignored"));
            }
            other => ctx.errors.push(other, "unknown key"),
        }
    }

    if !modes_seen || !config.modes.contains_key(MAIN_MODE) {
        ctx.errors.push("mode.main.binding", "the `main` binding mode is required");
    }
    for (path, name) in std::mem::take(&mut ctx.mode_refs) {
        if !config.modes.contains_key(&name) {
            ctx.errors.push(path, format!("unknown binding mode `{name}`"));
        }
    }
    for (name, _) in &config.force_assignments {
        ctx.workspace_refs.insert(name.clone());
    }
    ctx.workspace_refs.extend(persistent);
    config.workspaces = derive_pool(&ctx.workspace_refs);
    config.warnings = warnings;
    ctx.errors.finish()?;
    Ok(config)
}

fn parse_key_mapping(value: &toml::Value, ctx: &mut Ctx) -> KeyPreset {
    let Some(table) = value.as_table() else {
        ctx.errors.push("key-mapping", format!("expected a table, found {}", value.type_str()));
        return KeyPreset::Qwerty;
    };
    let mut preset = KeyPreset::Qwerty;
    for (key, v) in table {
        let path = join("key-mapping", key);
        if key != "preset" {
            ctx.errors.push(path, "unknown key (expected preset)");
            continue;
        }
        if let Some(name) = ctx.string(v, &path) {
            match KeyPreset::from_name(name) {
                Some(p) => preset = p,
                None => ctx.errors.push(path, format!("unknown preset `{name}`, expected qwerty, dvorak or colemak")),
            }
        }
    }
    preset
}

fn parse_padding(value: &toml::Value, path: &str, ctx: &mut Ctx) -> Option<u32> {
    match value.as_integer() {
        Some(n) => match u32::try_from(n) {
            Ok(n) => Some(n),
            Err(_) => {
                ctx.errors.push(path, format!("{n} is out of range (0 or more pixels)"));
                None
            }
        },
        None => {
            ctx.errors.push(path, format!("expected an integer, found {}", value.type_str()));
            None
        }
    }
}

fn parse_modes(value: &toml::Value, path: &str, preset: KeyPreset, ctx: &mut Ctx) -> BTreeMap<String, Mode> {
    let mut modes = BTreeMap::new();
    let Some(table) = value.as_table() else {
        ctx.errors.push(path, format!("expected a table of binding modes, found {}", value.type_str()));
        return modes;
    };
    for (name, mode_value) in table {
        let mode_path = join(path, name);
        let Some(mode_table) = mode_value.as_table() else {
            ctx.errors.push(mode_path, format!("expected a table, found {}", mode_value.type_str()));
            continue;
        };
        let mut mode = Mode::default();
        for (key, binding_value) in mode_table {
            let binding_path = join(&mode_path, key);
            if key != "binding" {
                ctx.errors.push(binding_path, "unknown key (expected binding)");
                continue;
            }
            let Some(bindings) = binding_value.as_table() else {
                ctx.errors.push(binding_path, format!("expected a table, found {}", binding_value.type_str()));
                continue;
            };
            parse_bindings(bindings, &binding_path, preset, &mut mode, ctx);
        }
        modes.insert(name.clone(), mode);
    }
    modes
}

fn parse_bindings(table: &toml::Table, path: &str, preset: KeyPreset, mode: &mut Mode, ctx: &mut Ctx) {
    for (chord, value) in table {
        let at = join(path, chord);
        let hotkey = match Hotkey::parse(chord, preset) {
            Ok(h) => h,
            Err(e) => {
                ctx.errors.push(&at, e.to_string());
                // Still check the commands, so one pass reports everything.
                ctx.actions(value, &at);
                continue;
            }
        };
        let actions = ctx.actions(value, &at);
        if let Some(first) = mode.bindings.get(&hotkey) {
            ctx.errors.push(at, format!("duplicate binding: `{}` is the same chord", first.chord));
            continue;
        }
        mode.bindings.insert(hotkey, Binding { chord: chord.clone(), actions });
    }
}

fn parse_force_assignments(value: &toml::Value, path: &str, ctx: &mut Ctx) -> Vec<(String, Vec<MonitorPattern>)> {
    let mut out = Vec::new();
    let Some(table) = value.as_table() else {
        ctx.errors.push(path, format!("expected a table, found {}", value.type_str()));
        return out;
    };
    for (workspace, monitors) in table {
        let at = join(path, workspace);
        if workspace.is_empty() || workspace.contains(char::is_whitespace) {
            ctx.errors.push(&at, "workspace names must be non-empty and contain no whitespace");
            continue;
        }
        let descriptions: Vec<(String, &toml::Value)> = match monitors {
            toml::Value::Array(items) => items.iter().enumerate().map(|(i, v)| (format!("{at}[{i}]"), v)).collect(),
            single => vec![(at.clone(), single)],
        };
        let mut patterns = Vec::new();
        for (item_path, item) in descriptions {
            let text = match item {
                toml::Value::String(text) => text.clone(),
                toml::Value::Integer(n) => n.to_string(),
                other => {
                    ctx.errors.push(item_path, format!("expected a monitor description, found {}", other.type_str()));
                    continue;
                }
            };
            match parse_monitor_pattern(&text) {
                Ok(p) => patterns.push(p),
                Err(why) => ctx.errors.push(item_path, why),
            }
        }
        out.push((workspace.clone(), patterns));
    }
    out
}

fn parse_workspace_list(value: &toml::Value, path: &str, ctx: &mut Ctx) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let Some(items) = value.as_array() else {
        ctx.errors.push(path, format!("expected an array of workspace names, found {}", value.type_str()));
        return out;
    };
    for (i, item) in items.iter().enumerate() {
        let at = format!("{path}[{i}]");
        if let Some(name) = ctx.string(item, &at) {
            if name.is_empty() || name.contains(char::is_whitespace) {
                ctx.errors.push(at, "workspace names must be non-empty and contain no whitespace");
            } else {
                out.insert(name.to_owned());
            }
        }
    }
    out
}

/// `1`..`9`, then every referenced name not already there: numbers ascending, then the
/// rest alphabetically. Independent of the order things appear in the file.
fn derive_pool(referenced: &BTreeSet<String>) -> Vec<String> {
    let mut pool: Vec<String> = DEFAULT_WORKSPACES.map(|n| n.to_string()).collect();
    let mut extras: Vec<&String> = referenced.iter().filter(|n| !pool.contains(n)).collect();
    extras.sort_by_key(|name| match name.parse::<u32>() {
        Ok(n) if n.to_string() == **name => (0, n, String::new()),
        _ => (1, 0, (*name).clone()),
    });
    pool.extend(extras.into_iter().cloned());
    pool
}

#[cfg(test)]
mod tests;
