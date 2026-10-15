//! The author's real AeroSpace config (`tests/fixtures/aerospace.toml`, copied verbatim)
//! must load through dowm's parser as-is, with every binding meaning what it means in
//! AeroSpace.

#![cfg(target_os = "macos")]

use dowm::actor::reactor::{Command, MouseTarget};
use dowm::config::{self, Action, BindingModes, Config, Hotkey, KeyPreset, RootLayout, RootOrientation};
use dowm::layout::ops::{LayoutArg, ResizeDimension, SplitArg};
use dowm::model::workspace::{Direction, MonitorTarget};

const FIXTURE: &str = include_str!("fixtures/aerospace.toml");

fn load() -> Config {
    match config::parse(FIXTURE) {
        Ok(config) => config,
        Err(errors) => panic!("the AeroSpace config must load unchanged:\n{errors}"),
    }
}

fn key(chord: &str) -> Hotkey { Hotkey::parse(chord, KeyPreset::Qwerty).expect("valid chord") }

fn actions(config: &Config, mode: &str, chord: &str) -> Vec<Action> {
    config.modes[mode]
        .bindings
        .get(&key(chord))
        .unwrap_or_else(|| panic!("mode.{mode}.binding.{chord} is bound"))
        .actions
        .clone()
}

fn one(config: &Config, mode: &str, chord: &str) -> Action {
    let mut a = actions(config, mode, chord);
    assert_eq!(a.len(), 1, "{mode}.{chord} is a single command");
    a.remove(0)
}

fn reactor(c: Command) -> Action { Action::Reactor(c) }

#[test]
fn the_fixture_is_the_unmodified_aerospace_file() {
    // Guards against "fixing" the fixture to make the parser happy: it carries the
    // original's quirks (odd spacing, commented-out keys, a trailing space).
    assert!(FIXTURE.contains("accordion-padding = 3 \n"));
    assert!(FIXTURE.contains("inner.horizontal =0 \n"));
    assert!(FIXTURE.contains("# alt-b = 'workspace B'"));
    assert!(FIXTURE.contains("[[on-window-detected]]"));
}

#[test]
fn the_top_level_settings() {
    let c = load();
    assert!(c.after_login_command.is_empty());
    assert!(c.after_startup_command.is_empty());
    assert!(c.start_at_login);
    assert!(c.normalization.flatten_containers);
    assert!(c.normalization.opposite_orientation);
    assert_eq!(c.accordion_padding, 3);
    assert_eq!(c.default_root_layout, RootLayout::Tiles);
    assert_eq!(c.default_root_orientation, RootOrientation::Auto);
    assert_eq!(c.key_mapping, KeyPreset::Qwerty);
    assert!(c.warnings.is_empty(), "{:?}", c.warnings);
}

#[test]
fn the_gaps_are_all_zero() {
    assert_eq!(load().gaps, config::Gaps::default());
}

#[test]
fn workspaces_one_to_nine_bind_alt_n_and_alt_shift_n() {
    let c = load();
    for n in 1..=9 {
        assert_eq!(
            one(&c, "main", &format!("alt-{n}")),
            reactor(Command::Workspace(n.to_string())),
            "alt-{n}"
        );
        assert_eq!(
            one(&c, "main", &format!("alt-shift-{n}")),
            reactor(Command::MoveNodeToWorkspace { workspace: n.to_string(), window: None, follow: false }),
            "alt-shift-{n}"
        );
    }
}

#[test]
fn the_main_mode_navigation_layout_and_monitor_bindings() {
    let c = load();
    for (chord, d) in [("alt-h", Direction::Left), ("alt-j", Direction::Down), ("alt-k", Direction::Up), ("alt-l", Direction::Right)] {
        assert_eq!(one(&c, "main", chord), reactor(Command::Focus { direction: d, wrap: false }), "{chord}");
    }
    for (chord, d) in [("alt-shift-h", Direction::Left), ("alt-shift-j", Direction::Down), ("alt-shift-k", Direction::Up), ("alt-shift-l", Direction::Right)] {
        assert_eq!(one(&c, "main", chord), reactor(Command::Move(d)), "{chord}");
    }
    assert_eq!(
        one(&c, "main", "alt-slash"),
        reactor(Command::Layout(vec![LayoutArg::Tiles, LayoutArg::Horizontal, LayoutArg::Vertical]))
    );
    assert_eq!(
        one(&c, "main", "alt-comma"),
        reactor(Command::Layout(vec![LayoutArg::Accordion, LayoutArg::Horizontal, LayoutArg::Vertical]))
    );
    assert_eq!(one(&c, "main", "alt-shift-f"), reactor(Command::Fullscreen));
    assert_eq!(one(&c, "main", "alt-tab"), reactor(Command::WorkspaceBackAndForth));
    assert_eq!(
        one(&c, "main", "alt-shift-tab"),
        reactor(Command::MoveWorkspaceToMonitor(MonitorTarget::Next { wrap: true }))
    );
    assert_eq!(one(&c, "main", "alt-shift-semicolon"), Action::Mode("service".into()));
    assert_eq!(one(&c, "main", "alt-shift-r"), Action::Mode("resize".into()));
}

#[test]
fn the_resize_mode() {
    let c = load();
    let resize = |dimension, amount| reactor(Command::Resize { dimension, amount });
    assert_eq!(one(&c, "resize", "h"), resize(ResizeDimension::Width, -50));
    assert_eq!(one(&c, "resize", "j"), resize(ResizeDimension::Height, 50));
    assert_eq!(one(&c, "resize", "k"), resize(ResizeDimension::Height, -50));
    assert_eq!(one(&c, "resize", "l"), resize(ResizeDimension::Width, 50));
    assert_eq!(one(&c, "resize", "b"), reactor(Command::BalanceSizes));
    assert_eq!(one(&c, "resize", "minus"), resize(ResizeDimension::Smart, -50));
    assert_eq!(one(&c, "resize", "equal"), resize(ResizeDimension::Smart, 50));
    assert_eq!(one(&c, "resize", "enter"), Action::Mode("main".into()));
    assert_eq!(one(&c, "resize", "esc"), Action::Mode("main".into()));
    assert_eq!(c.modes["resize"].bindings.len(), 9);
}

#[test]
fn the_service_mode_including_multi_command_arrays() {
    let c = load();
    let main = Action::Mode("main".into());
    assert_eq!(actions(&c, "service", "esc"), vec![Action::ReloadConfig, main.clone()]);
    assert_eq!(actions(&c, "service", "r"), vec![reactor(Command::FlattenWorkspaceTree), main.clone()]);
    assert_eq!(
        actions(&c, "service", "f"),
        vec![reactor(Command::Layout(vec![LayoutArg::Floating, LayoutArg::Tiling])), main.clone()]
    );
    assert_eq!(actions(&c, "service", "backspace"), vec![reactor(Command::CloseAllWindowsButCurrent), main.clone()]);
    for (chord, d) in [("alt-shift-h", Direction::Left), ("alt-shift-j", Direction::Down), ("alt-shift-k", Direction::Up), ("alt-shift-l", Direction::Right)] {
        assert_eq!(actions(&c, "service", chord), vec![reactor(Command::JoinWith(d)), main.clone()], "{chord}");
    }
    assert_eq!(c.modes["service"].bindings.len(), 8);
}

#[test]
fn the_on_window_detected_rules() {
    let c = load();
    assert_eq!(c.window_rules.len(), 2);
    let wez = &c.window_rules[0];
    assert_eq!(wez.app_id.as_deref(), Some("com.github.wez.wezterm"));
    assert_eq!(wez.run, vec![reactor(Command::MoveNodeToWorkspace { workspace: "T".into(), window: None, follow: false })]);
    let chrome = &c.window_rules[1];
    assert_eq!(chrome.app_id.as_deref(), Some("com.google.Chrome"));
    assert_eq!(chrome.run, vec![reactor(Command::MoveNodeToWorkspace { workspace: "B".into(), window: None, follow: false })]);
    assert!(!wez.check_further_callbacks && !chrome.check_further_callbacks);
}

#[test]
fn the_callbacks_in_both_string_and_array_form() {
    let c = load();
    assert_eq!(
        c.on_focused_monitor_changed,
        vec![reactor(Command::MoveMouse { target: MouseTarget::Monitor, lazy: true })],
        "an array of one command"
    );
    assert_eq!(
        c.on_focus_changed,
        vec![reactor(Command::MoveMouse { target: MouseTarget::Window, lazy: true })],
        "a plain string"
    );
}

#[test]
fn the_workspace_pool_includes_t_and_b_so_the_rules_work_unchanged() {
    let c = load();
    assert_eq!(c.workspaces, ["1", "2", "3", "4", "5", "6", "7", "8", "9", "B", "T"]);
    let store = dowm::model::workspace::WorkspaceStore::new(&c.workspace_config()).expect("the pool is valid");
    assert!(store.id_by_name("T").is_ok() && store.id_by_name("B").is_ok());
    assert_eq!(store.len(), 11);
}

#[test]
fn the_commented_out_force_assignments_are_not_active() {
    assert!(load().force_assignments.is_empty());
}

#[test]
fn binding_modes_work_end_to_end_on_the_fixture() {
    let c = load();
    let mut modes = BindingModes::new(&c);
    assert_eq!(modes.current_mode(), "main");
    assert!(modes.dispatch(key("h")).is_none(), "h is a resize-mode key only");
    modes.set_mode("resize").unwrap();
    assert!(modes.dispatch(key("h")).is_some());
    assert!(modes.dispatch(key("alt-h")).is_none(), "main's bindings are not live in resize");
    modes.set_mode("service").unwrap();
    assert_eq!(modes.dispatch(key("esc")).unwrap().len(), 2);
    modes.set_mode("main").unwrap();
    assert!(modes.dispatch(key("alt-h")).is_some());
    // A reload of the same file keeps working and keeps the mode.
    modes.set_mode("service").unwrap();
    modes.reload(&load());
    assert_eq!(modes.current_mode(), "service");
}

#[test]
fn a_reactor_can_be_built_from_the_fixture() {
    use dowm::actor::reactor::Reactor;
    use std::sync::Arc;
    let reactor = Reactor::with_config(None, Arc::new(load())).expect("the pool is valid");
    assert_eq!(reactor.workspaces().len(), 11);
    assert_eq!(reactor.config().expect("applied").accordion_padding, 3);
}

#[test]
fn split_and_the_remaining_command_forms_round_trip_through_a_config() {
    // Commands the fixture does not use, parsed through the same file path.
    let text = "[mode.main.binding]\nalt-a = ['split horizontal', 'split vertical', 'split opposite']\nalt-b = 'focus-monitor --wrap-around left'\n";
    let c = config::parse(text).unwrap();
    assert_eq!(
        actions(&c, "main", "alt-a"),
        vec![
            reactor(Command::Split(SplitArg::Horizontal)),
            reactor(Command::Split(SplitArg::Vertical)),
            reactor(Command::Split(SplitArg::Opposite)),
        ]
    );
    assert_eq!(
        one(&c, "main", "alt-b"),
        reactor(Command::FocusMonitor(MonitorTarget::Direction { direction: Direction::Left, wrap: true }))
    );
}
