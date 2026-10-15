//! Parse tests for every config key, plus the negative cases. Command, chord, gap and
//! rule syntax have their own tests next to their parsers; these cover the file layer.

use super::*;
use crate::actor::reactor::Command;
use crate::model::workspace::{Direction, MonitorPattern};

const MIN: &str = "[mode.main.binding]\nalt-h = 'focus left'\n";

fn ok(text: &str) -> Config {
    match parse(text) {
        Ok(c) => c,
        Err(e) => panic!("expected a valid config, got:\n{e}\n--- config ---\n{text}"),
    }
}

fn err(text: &str) -> ConfigErrors { parse(text).expect_err(&format!("should be rejected:\n{text}")) }

/// `MIN` plus extra top-level text placed before the mode tables.
fn with(extra: &str) -> String { format!("{extra}\n{MIN}") }

fn paths(e: &ConfigErrors) -> Vec<&str> { e.iter().map(|e| e.path.as_str()).collect() }

fn workspace(n: &str) -> Action { Action::Reactor(Command::Workspace(n.into())) }

// ---- defaults ----

#[test]
fn only_mode_main_is_required_and_everything_else_defaults() {
    let c = ok(MIN);
    assert!(c.after_login_command.is_empty() && c.after_startup_command.is_empty());
    assert!(!c.start_at_login);
    assert_eq!(c.normalization, NormalizeOptions { flatten_containers: true, opposite_orientation: true });
    assert_eq!(c.accordion_padding, 30);
    assert_eq!(c.default_root_layout, RootLayout::Tiles);
    assert_eq!(c.default_root_orientation, RootOrientation::Auto);
    assert_eq!(c.key_mapping, KeyPreset::Qwerty);
    assert!(c.on_focused_monitor_changed.is_empty() && c.on_focus_changed.is_empty());
    assert!(!c.focus_follows_mouse);
    assert_eq!(c.gaps, Gaps::default());
    assert!(c.window_rules.is_empty() && c.force_assignments.is_empty());
    assert!(!c.workspace_auto_back_and_forth);
    assert_eq!(c.workspaces, ["1", "2", "3", "4", "5", "6", "7", "8", "9"]);
    assert!(c.warnings.is_empty());
}

#[test]
fn the_builtin_default_config_is_valid_and_has_the_documented_bindings() {
    let c = Config::builtin().expect("the embedded default config parses");
    assert!(c.modes.contains_key("main") && c.modes.contains_key("resize"));
    let main = &c.modes["main"].bindings;
    let key = |s: &str| Hotkey::parse(s, KeyPreset::Qwerty).unwrap();
    assert_eq!(main[&key("alt-h")].actions, [Action::Reactor(Command::Focus { direction: Direction::Left, wrap: false })]);
    for n in 1..=9 {
        assert_eq!(main[&key(&format!("alt-{n}"))].actions, [workspace(&n.to_string())]);
    }
    assert_eq!(c.workspaces.len(), 9);
}

#[test]
fn an_empty_file_is_rejected_for_the_missing_main_mode() {
    let e = err("");
    assert_eq!(paths(&e), ["mode.main.binding"]);
}

// ---- every top-level key ----

#[test]
fn after_login_and_after_startup_commands_take_a_string_or_an_array() {
    let c = ok(&with("after-login-command = []\nafter-startup-command = 'workspace 2'"));
    assert!(c.after_login_command.is_empty());
    assert_eq!(c.after_startup_command, [workspace("2")]);
    let c = ok(&with("after-login-command = ['workspace 1', 'workspace 3']"));
    assert_eq!(c.after_login_command, [workspace("1"), workspace("3")]);
}

#[test]
fn start_at_login_is_parsed() {
    assert!(ok(&with("start-at-login = true")).start_at_login);
    assert!(!ok(&with("start-at-login = false")).start_at_login);
}

#[test]
fn the_normalization_flags() {
    let c = ok(&with("enable-normalization-flatten-containers = false"));
    assert_eq!(c.normalization, NormalizeOptions { flatten_containers: false, opposite_orientation: true });
    let c = ok(&with("enable-normalization-opposite-orientation-for-nested-containers = false"));
    assert_eq!(c.normalization, NormalizeOptions { flatten_containers: true, opposite_orientation: false });
    let c = ok(&with("enable-normalization-flatten-containers = false\nenable-normalization-opposite-orientation-for-nested-containers = false"));
    assert_eq!(c.normalization, NormalizeOptions { flatten_containers: false, opposite_orientation: false });
}

#[test]
fn accordion_padding() {
    assert_eq!(ok(&with("accordion-padding = 3")).accordion_padding, 3);
    assert_eq!(ok(&with("accordion-padding = 0")).accordion_padding, 0);
}

#[test]
fn default_root_container_layout() {
    assert_eq!(ok(&with("default-root-container-layout = 'tiles'")).default_root_layout, RootLayout::Tiles);
    assert_eq!(ok(&with("default-root-container-layout = 'accordion'")).default_root_layout, RootLayout::Accordion);
}

#[test]
fn default_root_container_orientation() {
    for (word, expected) in [
        ("horizontal", RootOrientation::Horizontal),
        ("vertical", RootOrientation::Vertical),
        ("auto", RootOrientation::Auto),
    ] {
        assert_eq!(ok(&with(&format!("default-root-container-orientation = '{word}'"))).default_root_orientation, expected);
    }
}

#[test]
fn key_mapping_preset_for_each_preset_and_the_keys_follow_it() {
    for preset in KeyPreset::ALL {
        let c = ok(&with(&format!("key-mapping.preset = '{}'", preset.name())));
        assert_eq!(c.key_mapping, preset);
    }
    let dvorak = ok("key-mapping.preset = 'dvorak'\n[mode.main.binding]\nalt-s = 'focus left'\n");
    let hotkey = Hotkey::parse("alt-s", KeyPreset::Dvorak).unwrap();
    assert!(dvorak.modes["main"].bindings.contains_key(&hotkey));
    let qwerty = ok("[mode.main.binding]\nalt-s = 'focus left'\n");
    assert!(!qwerty.modes["main"].bindings.contains_key(&hotkey));
    // The table form is equivalent.
    assert_eq!(ok("[key-mapping]\npreset = 'colemak'\n[mode.main.binding]\n").key_mapping, KeyPreset::Colemak);
}

#[test]
fn callbacks_accept_a_string_or_an_array() {
    let c = ok(&with("on-focused-monitor-changed = ['move-mouse monitor-lazy-center']\non-focus-changed = 'move-mouse window-lazy-center'"));
    assert_eq!(c.on_focused_monitor_changed.len(), 1);
    assert_eq!(c.on_focus_changed.len(), 1);
    let c = ok(&with("on-focus-changed = ['move-mouse window-lazy-center', 'workspace 2']\non-focused-monitor-changed = 'move-mouse monitor-force-center'"));
    assert_eq!(c.on_focus_changed.len(), 2);
    assert_eq!(c.on_focused_monitor_changed.len(), 1);
}

#[test]
fn focus_follows_mouse() {
    assert!(ok(&with("focus-follows-mouse = true")).focus_follows_mouse);
    assert!(!ok(&with("focus-follows-mouse = false")).focus_follows_mouse);
}

#[test]
fn animations_default_on_with_a_third_of_a_second() {
    let c = ok(MIN);
    assert!(c.animations);
    assert_eq!(c.animation_duration, 0.3);
}

#[test]
fn animations_can_be_turned_off_and_timed() {
    assert!(!ok(&with("animations = false")).animations);
    let c = ok(&with("animation-duration = 0.5"));
    assert_eq!(c.animation_duration, 0.5);
    assert_eq!(ok(&with("animation-duration = 1")).animation_duration, 1.0, "an integer is fine");
}

#[test]
fn bad_animation_values_name_their_keys() {
    for (text, path) in [
        ("animations = 'no'", "animations"),
        ("animations = 0", "animations"),
        ("animation-duration = 'fast'", "animation-duration"),
        ("animation-duration = 0", "animation-duration"),
        ("animation-duration = -0.2", "animation-duration"),
        ("animation-duration = 60", "animation-duration"),
    ] {
        assert_eq!(paths(&err(&with(text))), vec![path], "{text}");
    }
}

#[test]
fn workspace_auto_back_and_forth() {
    assert!(ok(&with("workspace-auto-back-and-forth = true")).workspace_auto_back_and_forth);
    assert!(ok(&with("workspace-auto-back-and-forth = true")).workspace_config().auto_back_and_forth);
}

#[test]
fn the_gaps_table_is_parsed_into_the_config() {
    let c = ok(&format!("[gaps]\ninner.horizontal = 4\nouter.top = [{{ monitor.main = 16 }}, 8]\n{MIN}"));
    assert_eq!(c.gaps.inner_horizontal, GapValue::constant(4));
    assert_eq!(c.gaps.outer_top, GapValue::per_monitor(vec![(MonitorPattern::Main, 16)], 8));
}

#[test]
fn on_window_detected_rules_are_parsed_in_order() {
    let c = ok(&format!(
        "[[on-window-detected]]\nif.app-id = 'a'\nrun = 'move-node-to-workspace 4'\n[[on-window-detected]]\nif.app-id = 'b'\nrun = 'fullscreen'\n{MIN}"
    ));
    assert_eq!(c.window_rules.len(), 2);
    assert_eq!(c.window_rules[0].app_id.as_deref(), Some("a"));
    assert_eq!(c.window_rules[1].app_id.as_deref(), Some("b"));
}

#[test]
fn force_assignments_take_one_monitor_or_a_list() {
    let c = ok(&format!(
        "[workspace-to-monitor-force-assignment]\n1 = 'main'\n2 = ['secondary', 'main']\n3 = 2\n4 = 'DELL.*'\n{MIN}"
    ));
    let find = |name: &str| c.force_assignments.iter().find(|(n, _)| n == name).map(|(_, m)| m.clone());
    assert_eq!(find("1"), Some(vec![MonitorPattern::Main]));
    assert_eq!(find("2"), Some(vec![MonitorPattern::Secondary, MonitorPattern::Main]));
    assert_eq!(find("3"), Some(vec![MonitorPattern::Sequence(2)]), "a bare integer is a sequence number");
    assert_eq!(find("4"), Some(vec![MonitorPattern::Name("DELL.*".into())]));
}

#[test]
fn force_assignments_become_workspace_config_by_pool_position() {
    let c = ok(&format!("[workspace-to-monitor-force-assignment]\n3 = 'main'\nT = ['secondary', 'main']\n{MIN}"));
    let wc = c.workspace_config();
    assert_eq!(wc.names, c.workspaces);
    let t_position = c.workspaces.iter().position(|w| w == "T").unwrap() as u32 + 1;
    assert!(wc.force_assignments.iter().any(|f| f.workspace == 3 && f.monitors == [MonitorPattern::Main]));
    assert!(wc.force_assignments.iter().any(|f| f.workspace == t_position && f.monitors.len() == 2));
    // The model accepts it.
    crate::model::workspace::WorkspaceStore::new(&wc).expect("a valid workspace config");
}

// ---- modes and bindings ----

#[test]
fn arbitrary_named_modes_are_supported() {
    let c = ok("[mode.main.binding]\nalt-g = 'mode gaming'\n[mode.gaming.binding]\nesc = 'mode main'\n[mode.'my mode'.binding]\nesc = 'mode main'\n");
    assert_eq!(c.modes.keys().map(String::as_str).collect::<Vec<_>>(), ["gaming", "main", "my mode"]);
}

#[test]
fn a_binding_takes_a_string_or_an_array_of_commands() {
    let c = ok("[mode.main.binding]\nalt-a = 'workspace 1'\nalt-b = ['workspace 2', 'workspace 3']\nalt-c = []\n");
    let b = &c.modes["main"].bindings;
    let key = |s: &str| Hotkey::parse(s, KeyPreset::Qwerty).unwrap();
    assert_eq!(b[&key("alt-a")].actions.len(), 1);
    assert_eq!(b[&key("alt-b")].actions, [workspace("2"), workspace("3")]);
    assert!(b[&key("alt-c")].actions.is_empty());
    assert_eq!(b[&key("alt-b")].chord, "alt-b");
}

#[test]
fn every_key_name_can_be_bound() {
    let mut text = String::from("[mode.main.binding]\n");
    for &(name, _) in keys::KEYS {
        text.push_str(&format!("alt-{name} = 'focus left'\n"));
    }
    assert_eq!(ok(&text).modes["main"].bindings.len(), keys::KEYS.len());
}

#[test]
fn every_modifier_combination_can_be_bound() {
    let mods = ["cmd", "alt", "ctrl", "shift"];
    let mut text = String::from("[mode.main.binding]\n");
    let mut count = 0;
    for mask in 0u8..16 {
        let mut parts: Vec<&str> = (0..4).filter(|i| mask & (1 << i) != 0).map(|i| mods[i]).collect();
        parts.push("h");
        text.push_str(&format!("{} = 'focus left'\n", parts.join("-")));
        count += 1;
    }
    assert_eq!(ok(&text).modes["main"].bindings.len(), count);
}

// ---- pool derivation ----

#[test]
fn the_pool_is_one_to_nine_by_default() {
    assert_eq!(ok(MIN).workspaces.len(), 9);
}

#[test]
fn referenced_workspaces_extend_the_pool_deterministically() {
    let a = ok("[mode.main.binding]\nalt-t = 'workspace T'\nalt-b = 'move-node-to-workspace B'\nalt-1 = 'workspace 12'\nalt-2 = 'workspace 10'\n");
    assert_eq!(a.workspaces[9..], ["10", "12", "B", "T"], "numbers ascending, then names alphabetically, after 1-9");
    // The same references in another order and place give the same pool.
    let b = ok("[[on-window-detected]]\nif.app-id = 'x'\nrun = 'move-node-to-workspace T'\n[[on-window-detected]]\nif.workspace = '12'\nrun = 'move-node-to-workspace B'\n[mode.main.binding]\nalt-2 = 'workspace 10'\n");
    assert_eq!(b.workspaces, a.workspaces);
}

#[test]
fn rules_force_assignments_and_persistent_workspaces_all_reach_the_pool() {
    let c = ok(&format!(
        "persistent-workspaces = ['P']\n[workspace-to-monitor-force-assignment]\nF = 'main'\n[[on-window-detected]]\nif.app-id = 'x'\nrun = 'move-node-to-workspace R'\n{MIN}"
    ));
    assert_eq!(c.workspaces[9..], ["F", "P", "R"]);
}

#[test]
fn a_workspace_referenced_twice_or_inside_one_to_nine_is_listed_once() {
    let c = ok("[mode.main.binding]\nalt-a = 'workspace 5'\nalt-b = 'workspace T'\nalt-c = 'workspace T'\n");
    assert_eq!(c.workspaces.len(), 10);
}

#[test]
fn restart_is_required_only_for_a_changed_pool() {
    let old = ok(MIN);
    assert!(old.restart_required(&old).is_empty());
    let new = ok("accordion-padding = 5\n[mode.main.binding]\nalt-t = 'workspace T'\n");
    let reasons = old.restart_required(&new);
    assert_eq!(reasons.len(), 1);
    assert!(reasons[0].contains("requires restart") && reasons[0].contains('T'), "{reasons:?}");
    let same_pool = ok("accordion-padding = 5\n[mode.main.binding]\nalt-j = 'focus down'\n");
    assert!(old.restart_required(&same_pool).is_empty());
}

// ---- ignored keys ----

#[test]
fn unsupported_aerospace_keys_are_accepted_with_a_warning() {
    let c = ok(&with("config-version = 2\nautomatically-unhide-macos-hidden-apps = true\nexec-on-workspace-change = ['/bin/echo']\n[exec]\ninherit-env-vars = true"));
    assert_eq!(c.warnings.len(), 4, "{:?}", c.warnings);
    assert!(c.warnings.iter().any(|w| w.starts_with("config-version")));
    assert!(c.warnings.iter().all(|w| w.contains("ignored")));
}

// ---- negative cases: every error names its key path ----

#[test]
fn an_unknown_top_level_key_is_an_error_with_its_path() {
    let e = err(&with("colour-scheme = 'blue'"));
    assert!(e.mentions("colour-scheme"), "{e}");
}

#[test]
fn a_missing_main_mode_is_an_error() {
    let e = err("[mode.resize.binding]\nesc = 'mode resize'\n");
    assert!(e.mentions("mode.main.binding"), "{e}");
}

#[test]
fn a_mode_command_naming_an_unknown_mode_is_an_error_at_its_key() {
    let e = err("[mode.main.binding]\nalt-r = 'mode resizee'\n");
    assert!(e.mentions("mode.main.binding.alt-r") && e.to_string().contains("resizee"), "{e}");
    let e = err("[mode.main.binding]\nalt-r = ['workspace 1', 'mode nowhere']\n");
    assert!(e.mentions("mode.main.binding.alt-r[1]"), "{e}");
    let e = err(&with("on-focus-changed = 'mode nowhere'"));
    assert!(e.mentions("on-focus-changed"), "{e}");
    let e = err(&format!("[[on-window-detected]]\nrun = 'mode nowhere'\n{MIN}"));
    assert!(e.mentions("on-window-detected[0].run"), "{e}");
}

#[test]
fn a_mode_command_to_a_declared_mode_is_fine_even_when_declared_later() {
    ok("[mode.main.binding]\nalt-r = 'mode zzz'\n[mode.zzz.binding]\nesc = 'mode main'\n");
}

#[test]
fn duplicate_chords_are_rejected_even_under_different_spellings() {
    let e = err("[mode.main.binding]\nalt-shift-h = 'focus left'\nshift-alt-h = 'focus right'\n");
    assert_eq!(e.len(), 1, "{e}");
    assert!(e.mentions("mode.main.binding.shift-alt-h") && e.to_string().contains("alt-shift-h"), "{e}");
    let e = err("[mode.main.binding]\ncmd-ctrl-alt-shift-space = 'fullscreen'\nshift-alt-ctrl-cmd-space = 'fullscreen'\n");
    assert_eq!(e.len(), 1, "{e}");
}

#[test]
fn the_same_chord_in_two_modes_is_not_a_duplicate() {
    ok("[mode.main.binding]\nesc = 'mode resize'\n[mode.resize.binding]\nesc = 'mode main'\n");
}

#[test]
fn an_empty_or_modifiers_only_chord_is_an_error_at_its_key() {
    let e = err("[mode.main.binding]\nalt-shift = 'focus left'\n");
    assert!(e.mentions("mode.main.binding.alt-shift") && e.to_string().contains("needs a key"), "{e}");
    let e = err("[mode.main.binding]\n'' = 'focus left'\n");
    assert!(!e.is_empty(), "{e}");
    let e = err("[mode.main.binding]\nalt-hh = 'focus left'\n");
    assert!(e.mentions("mode.main.binding.alt-hh") && e.to_string().contains("unknown key"), "{e}");
    let e = err("[mode.main.binding]\nopt-h = 'focus left'\n");
    assert!(e.to_string().contains("unknown modifier"), "{e}");
}

#[test]
fn an_unknown_or_malformed_command_is_an_error_at_its_key() {
    let e = err("[mode.main.binding]\nalt-a = 'wrkspace 1'\n");
    assert!(e.mentions("mode.main.binding.alt-a") && e.to_string().contains("wrkspace"), "{e}");
    let e = err("[mode.main.binding]\nalt-a = 'resize width abc'\n");
    assert!(e.mentions("mode.main.binding.alt-a"), "{e}");
    let e = err("[mode.main.binding]\nalt-a = 3\n");
    assert!(e.mentions("mode.main.binding.alt-a"), "{e}");
    let e = err("[mode.main.binding]\nalt-a = ['workspace 1', 5]\n");
    assert!(e.mentions("mode.main.binding.alt-a[1]"), "{e}");
}

#[test]
fn bad_numbers_and_types_name_their_keys() {
    for (text, path) in [
        ("accordion-padding = -1", "accordion-padding"),
        ("accordion-padding = 'wide'", "accordion-padding"),
        ("accordion-padding = 1.5", "accordion-padding"),
        ("start-at-login = 'yes'", "start-at-login"),
        ("focus-follows-mouse = 1", "focus-follows-mouse"),
        ("enable-normalization-flatten-containers = 'no'", "enable-normalization-flatten-containers"),
        ("enable-normalization-opposite-orientation-for-nested-containers = 0", "enable-normalization-opposite-orientation-for-nested-containers"),
        ("workspace-auto-back-and-forth = 'x'", "workspace-auto-back-and-forth"),
        ("default-root-container-layout = 'bsp'", "default-root-container-layout"),
        ("default-root-container-layout = 3", "default-root-container-layout"),
        ("default-root-container-orientation = 'diagonal'", "default-root-container-orientation"),
        ("key-mapping.preset = 'azerty'", "key-mapping.preset"),
        ("key-mapping.preset = 3", "key-mapping.preset"),
        ("key-mapping.layout = 'x'", "key-mapping.layout"),
        ("key-mapping = 'qwerty'", "key-mapping"),
        ("on-focus-changed = 'nonsense'", "on-focus-changed"),
        ("on-focused-monitor-changed = ['move-mouse nowhere']", "on-focused-monitor-changed[0]"),
        ("after-startup-command = 'nonsense'", "after-startup-command"),
        ("after-login-command = 5", "after-login-command"),
        ("gaps = 3", "gaps"),
        ("gaps.inner.horizontal = -4", "gaps.inner.horizontal"),
        ("persistent-workspaces = 'A'", "persistent-workspaces"),
        ("persistent-workspaces = ['a b']", "persistent-workspaces[0]"),
        ("persistent-workspaces = [3]", "persistent-workspaces[0]"),
        ("workspace-to-monitor-force-assignment = 3", "workspace-to-monitor-force-assignment"),
        ("workspace-to-monitor-force-assignment.1 = '('", "workspace-to-monitor-force-assignment.1"),
        ("workspace-to-monitor-force-assignment.1 = ['main', '(']", "workspace-to-monitor-force-assignment.1[1]"),
        ("workspace-to-monitor-force-assignment.1 = 0", "workspace-to-monitor-force-assignment.1"),
        ("workspace-to-monitor-force-assignment.1 = true", "workspace-to-monitor-force-assignment.1"),
        ("workspace-to-monitor-force-assignment.'a b' = 'main'", "workspace-to-monitor-force-assignment.a b"),
        ("on-window-detected = 4", "on-window-detected"),
    ] {
        // `gaps.` and `key-mapping.` dotted keys need to come before any table header.
        let e = err(&with(text));
        assert!(e.mentions(path), "`{text}` should be reported at `{path}`, got:\n{e}");
    }
}

#[test]
fn bad_mode_tables_name_their_keys() {
    let e = err("mode = 3");
    assert!(e.mentions("mode"), "{e}");
    let e = err("[mode]\nmain = 3\n");
    assert!(e.mentions("mode.main"), "{e}");
    let e = err("[mode.main]\nbindings = 1\n[mode.main.binding]\n");
    assert!(e.mentions("mode.main.bindings"), "{e}");
    let e = err("[mode.main]\nbinding = 'x'\n");
    assert!(e.mentions("mode.main.binding"), "{e}");
}

#[test]
fn all_errors_are_reported_together() {
    let e = err(
        "accordion-padding = -1\nbogus = 1\n[gaps]\nouter.top = 'x'\n[mode.main.binding]\nalt-a = 'nope'\nalt-b = 'mode ghost'\nxx-h = 'focus left'\n",
    );
    for path in ["accordion-padding", "bogus", "gaps.outer.top", "mode.main.binding.alt-a", "mode.main.binding.alt-b", "mode.main.binding.xx-h"] {
        assert!(e.mentions(path), "missing {path} in:\n{e}");
    }
    assert!(e.len() >= 6);
}

#[test]
fn a_syntax_error_is_one_file_level_error() {
    let e = err("[mode.main.binding\nalt-h = 'focus left'");
    assert_eq!(e.len(), 1);
    assert!(e.iter().next().unwrap().path.is_empty() && e.to_string().contains("syntax"), "{e}");
    let e = err("a = ");
    assert!(e.to_string().contains("syntax"));
}

#[test]
fn garbage_never_panics() {
    let mut seed = 0x1234_5678_9abc_def0_u64;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let alphabet: Vec<char> = "[]{}=.,'\"-_ \n\t#abcdefghijklmnopqrstuvwxyz0123456789+$\\/\0é💥".chars().collect();
    for _ in 0..3000 {
        let len = (next() % 80) as usize;
        let text: String = (0..len).map(|_| alphabet[(next() % alphabet.len() as u64) as usize]).collect();
        let _ = parse(&text);
        let _ = parse(&format!("[mode.main.binding]\n{text}\n"));
        let _ = parse(&format!("[mode.main.binding]\nalt-h = '{}'\n", text.replace(['\n', '\''], " ")));
    }
    for chunk in ["[[", "]]", "[mode.main.binding]\n[mode.main.binding]\n", "[gaps]\ninner = [[[[", "on-window-detected = [[]]", "\u{feff}", "\0"] {
        let _ = parse(chunk);
    }
}

#[test]
fn mutated_valid_configs_never_panic() {
    let base = include_str!("default.toml");
    let bytes: Vec<char> = base.chars().collect();
    let mut seed = 42_u64;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for _ in 0..400 {
        let mut chars = bytes.clone();
        for _ in 0..(1 + next() % 6) {
            let at = (next() % chars.len() as u64) as usize;
            match next() % 3 {
                0 => {
                    chars.remove(at);
                }
                1 => chars.insert(at, ['=', '[', ']', '\'', '-', 'x', '\n'][(next() % 7) as usize]),
                _ => chars[at] = ['0', '.', '"', ' ', '#'][(next() % 5) as usize],
            }
        }
        let _ = parse(&chars.iter().collect::<String>());
    }
}

// ---- path resolution ----

mod sources {
    use std::path::{Path, PathBuf};

    use super::*;

    fn exists_in(files: &'static [&'static str]) -> impl Fn(&Path) -> bool {
        move |p| files.iter().any(|f| Path::new(f) == p)
    }

    #[test]
    fn the_cli_flag_wins_even_when_the_file_does_not_exist() {
        let source = resolve_source(Some(Path::new("/x/my.toml")), Some(Path::new("/home/u")), &exists_in(&["/home/u/.config/dowm/dowm.toml"]));
        assert_eq!(source, ConfigSource::File(PathBuf::from("/x/my.toml")));
    }

    #[test]
    fn then_the_xdg_style_path_then_the_dotfile_then_the_builtin() {
        let home = Some(Path::new("/home/u"));
        let both = exists_in(&["/home/u/.config/dowm/dowm.toml", "/home/u/.dowm.toml"]);
        assert_eq!(resolve_source(None, home, &both), ConfigSource::File("/home/u/.config/dowm/dowm.toml".into()));
        let dot = exists_in(&["/home/u/.dowm.toml"]);
        assert_eq!(resolve_source(None, home, &dot), ConfigSource::File("/home/u/.dowm.toml".into()));
        let none = exists_in(&[]);
        assert_eq!(resolve_source(None, home, &none), ConfigSource::BuiltIn);
        assert_eq!(resolve_source(None, None, &both), ConfigSource::BuiltIn, "no home directory: built-in");
    }

    #[test]
    fn the_aerospace_config_path_is_not_consulted() {
        let aero = exists_in(&["/home/u/.config/aerospace/aerospace.toml", "/home/u/.aerospace.toml"]);
        assert_eq!(resolve_source(None, Some(Path::new("/home/u")), &aero), ConfigSource::BuiltIn);
    }

    #[test]
    fn load_reads_a_file_and_reports_io_and_parse_failures() {
        assert!(load(&ConfigSource::BuiltIn).is_ok());
        let missing = ConfigSource::File("/definitely/not/here/dowm.toml".into());
        let e = load(&missing).unwrap_err();
        assert!(matches!(e, LoadError::Io { .. }) && e.to_string().contains("/definitely/not/here"), "{e}");

        let dir = std::env::temp_dir().join(format!("dowm-load-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let good = dir.join("good.toml");
        let bad = dir.join("bad.toml");
        std::fs::write(&good, MIN).unwrap();
        std::fs::write(&bad, "nonsense = 1\n").unwrap();
        assert!(load(&ConfigSource::File(good)).is_ok());
        let e = load(&ConfigSource::File(bad)).unwrap_err();
        assert!(matches!(e, LoadError::Invalid(_)) && e.to_string().contains("nonsense"), "{e}");
        _ = std::fs::remove_dir_all(&dir);
        assert_eq!(ConfigSource::BuiltIn.path(), None);
    }
}
