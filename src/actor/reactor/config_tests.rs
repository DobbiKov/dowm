//! M4 scenarios driven by synthetic events (see `testing`): gaps, hot reload, the
//! `on-window-detected` rules, the focus callbacks, action lists and focus-follows-mouse.
//!
//! One 900x600 display `D` unless a test says otherwise.

use std::collections::BTreeMap;
use std::sync::Arc;

use objc2_core_foundation::CGRect;

use super::effects::Effect;
use super::testing::{Harness, W, rect, w, wid};
use super::{Command, CommandError, Event};
use crate::actor::app::{self, Quiet, WindowId};
use crate::config::{self, Action, Config};
use crate::layout::ops::{LayoutArg, ResizeDimension, TilingError};
use crate::model::workspace::{Direction, WorkspaceError, WorkspaceId};

const PID: i32 = 100;
const WEZ_PID: i32 = 200;
const CHROME_PID: i32 = 300;
const WEZTERM: &str = "com.github.wez.wezterm";
const CHROME: &str = "com.google.Chrome";
const AEROSPACE_FIXTURE: &str = include_str!("../../../tests/fixtures/aerospace.toml");

const MAIN: &str = "[mode.main.binding]\nalt-h = 'focus left'\n";

fn cfg(extra: &str) -> String { format!("{extra}\n{MAIN}") }

fn parse(text: &str) -> Arc<Config> {
    Arc::new(config::parse(text).unwrap_or_else(|e| panic!("bad test config:\n{e}")))
}

fn ws(n: u32) -> WorkspaceId { WorkspaceId::new(n) }

/// The pool position of a workspace by name.
fn ws_named(h: &Harness, name: &str) -> WorkspaceId { h.reactor.workspaces().id_by_name(name).expect("workspace in the pool") }

fn one_display(text: &str) -> Harness {
    let mut h = Harness::with_config_text(text);
    h.screens(&[("D", 1, rect(0., 0., 900., 600.))]);
    h.effects();
    h
}

/// `n` windows (indexes 1..=n, WindowServer ids 10+idx) of one app, on a 900x600 display.
fn with_windows(text: &str, n: u32) -> Harness {
    let mut h = one_display(text);
    let windows: Vec<W> = (1..=n).map(|i| w(i, 10 + i, rect(10., 10., 100., 100.))).collect();
    h.launch(PID, &windows);
    h.effects();
    h
}

fn focus(h: &mut Harness, idx: u32) {
    h.send(Event::ApplicationMainWindowChanged(PID, Some(wid(PID, idx)), Quiet::No));
}

fn finish_startup(h: &mut Harness) {
    h.send(Event::StartupFinished);
    h.effects();
}

fn frames(h: &Harness, ws_number: u32) -> BTreeMap<WindowId, CGRect> {
    h.reactor.workspace_frames(ws(ws_number)).expect("workspace has a display").into_iter().collect()
}

/// (x, y, width, height) of window `idx` of PID on workspace `n`.
fn px(h: &Harness, n: u32, idx: u32) -> (i64, i64, i64, i64) {
    let f = frames(h, n)[&wid(PID, idx)];
    (f.origin.x.round() as i64, f.origin.y.round() as i64, f.size.width.round() as i64, f.size.height.round() as i64)
}

fn reload(h: &mut Harness, text: &str) { h.send(Event::ConfigReloaded(parse(text))); }

fn mouse_effects(effects: &[Effect]) -> Vec<(CGRect, bool)> {
    effects
        .iter()
        .filter_map(|e| match e {
            Effect::MoveMouse { rect, lazy } => Some((*rect, *lazy)),
            _ => None,
        })
        .collect()
}

// ---- gaps ----

const GAPS: &str = "[gaps]\ninner.horizontal = 10\ninner.vertical = 10\nouter.left = 20\nouter.right = 20\nouter.top = 30\nouter.bottom = 40\n";

#[test]
fn gaps_shrink_and_separate_tiled_windows() {
    let h = with_windows(&cfg(GAPS), 2);
    assert_eq!(px(&h, 1, 1), (20, 30, 425, 530));
    assert_eq!(px(&h, 1, 2), (455, 30, 425, 530));
}

#[test]
fn without_gaps_the_layout_is_unchanged() {
    let h = with_windows(&cfg(""), 2);
    assert_eq!(px(&h, 1, 1), (0, 0, 450, 600));
    assert_eq!(px(&h, 1, 2), (450, 0, 450, 600));
}

#[test]
fn only_outer_or_only_inner_gaps_work_alone() {
    let h = with_windows(&cfg("[gaps]\nouter.left = 50\n"), 1);
    assert_eq!(px(&h, 1, 1), (50, 0, 850, 600));
    let h = with_windows(&cfg("[gaps]\ninner.horizontal = 20\n"), 2);
    assert_eq!(px(&h, 1, 1), (0, 0, 440, 600));
    assert_eq!(px(&h, 1, 2), (460, 0, 440, 600));
}

#[test]
fn per_monitor_gaps_resolve_by_the_display_the_workspace_is_on() {
    let text = cfg("[gaps]\nouter.top = [{ monitor.main = 16 }, { monitor.\"Side\" = 40 }, 8]\n");
    let mut h = Harness::with_config_text(&text);
    h.screens_named(&[
        ("L", 1, rect(0., 0., 1000., 600.), Some("Main Display")),
        ("R", 2, rect(1000., 0., 1000., 600.), Some("Side Display")),
    ]);
    h.launch(PID, &[w(1, 11, rect(10., 10., 100., 100.)), w(2, 12, rect(1100., 10., 100., 100.))]);
    assert_eq!(px(&h, 1, 1), (0, 16, 1000, 584), "workspace 1 is on the main display");
    assert_eq!(px(&h, 2, 2), (1000, 40, 1000, 560), "workspace 2 is on the display named Side");
}

#[test]
fn per_monitor_gaps_fall_back_to_the_default_when_nothing_matches() {
    let text = cfg("[gaps]\nouter.top = [{ monitor.\"Nope\" = 99 }, 8]\n");
    let h = with_windows(&text, 1);
    assert_eq!(px(&h, 1, 1), (0, 8, 900, 592));
}

#[test]
fn floating_and_fullscreen_windows_ignore_gaps() {
    let mut h = with_windows(&cfg(GAPS), 2);
    focus(&mut h, 1);
    h.tile(Command::Fullscreen).unwrap();
    assert_eq!(px(&h, 1, 1), (0, 0, 900, 600));
    h.tile(Command::Fullscreen).unwrap();
    h.tile(Command::Layout(vec![LayoutArg::Floating])).unwrap();
    assert_eq!(px(&h, 1, 1), (10, 10, 100, 100), "a floating window keeps its own frame");
}

// ---- applying a config ----

#[test]
fn a_reload_changes_gaps_without_a_restart() {
    let mut h = with_windows(&cfg(""), 2);
    assert_eq!(px(&h, 1, 1), (0, 0, 450, 600));
    reload(&mut h, &cfg(GAPS));
    assert_eq!(px(&h, 1, 1), (20, 30, 425, 530));
    reload(&mut h, &cfg(""));
    assert_eq!(px(&h, 1, 1), (0, 0, 450, 600));
}

#[test]
fn applying_a_config_emits_one_config_applied_effect_with_that_config() {
    let mut h = one_display(&cfg(""));
    let next = parse(&cfg("accordion-padding = 7"));
    h.send(Event::ConfigReloaded(Arc::clone(&next)));
    let applied: Vec<_> = h.effects().into_iter().filter(|e| matches!(e, Effect::ConfigApplied(_))).collect();
    assert_eq!(applied.len(), 1);
    assert_eq!(applied[0], Effect::ConfigApplied(Arc::clone(&next)));
    assert_eq!(h.reactor.config(), Some(&next));
}

#[test]
fn a_reactor_built_from_a_config_starts_with_it_applied() {
    let mut h = Harness::with_config_text(&cfg("accordion-padding = 12\nfocus-follows-mouse = true"));
    assert_eq!(h.reactor.config().unwrap().accordion_padding, 12);
    assert!(h.effects().iter().any(|e| matches!(e, Effect::ConfigApplied(c) if c.focus_follows_mouse)));
}

#[test]
fn accordion_padding_is_applied_live() {
    let mut h = with_windows(&cfg("accordion-padding = 30"), 3);
    focus(&mut h, 1);
    h.tile(Command::Layout(vec![LayoutArg::Accordion])).unwrap();
    assert_eq!(px(&h, 1, 2), (30, 0, 840, 600));
    reload(&mut h, &cfg("accordion-padding = 3"));
    assert_eq!(px(&h, 1, 2), (3, 0, 894, 600));
    reload(&mut h, &cfg("accordion-padding = 0"));
    assert_eq!(px(&h, 1, 2), (0, 0, 900, 600));
}

#[test]
fn normalization_flags_are_applied_live() {
    let mut h = with_windows(&cfg(""), 3);
    assert!(h.reactor.trees().options().flatten_containers);
    reload(&mut h, &cfg("enable-normalization-flatten-containers = false\nenable-normalization-opposite-orientation-for-nested-containers = false"));
    let options = h.reactor.trees().options();
    assert!(!options.flatten_containers && !options.opposite_orientation);
    reload(&mut h, &cfg(""));
    assert!(h.reactor.trees().options().flatten_containers && h.reactor.trees().options().opposite_orientation);
}

#[test]
fn the_default_root_layout_and_orientation_apply_to_empty_workspaces_on_reload() {
    let mut h = with_windows(&cfg("default-root-container-orientation = 'horizontal'"), 1);
    let desc = |h: &Harness, n: u32| h.reactor.trees().get(ws(n)).unwrap().describe();
    assert_eq!(desc(&h, 2), "h[]");
    reload(&mut h, &cfg("default-root-container-orientation = 'vertical'\ndefault-root-container-layout = 'accordion'"));
    assert_eq!(desc(&h, 2), "v[]", "an empty workspace gets the new root (accordion is now the default, so unmarked)");
    let empty = h.reactor.trees().get(ws(2)).unwrap();
    assert_eq!(empty.layout(empty.root()), Some(crate::layout::ACCORDION));
    assert_eq!(desc(&h, 1), "h[1]", "a populated workspace keeps its tree");
}

#[test]
fn auto_orientation_follows_the_monitor_shape() {
    let mut h = Harness::with_config_text(&cfg("default-root-container-orientation = 'auto'"));
    h.screens(&[("T", 1, rect(0., 0., 600., 900.)), ("W", 2, rect(600., 0., 900., 600.))]);
    let desc = |h: &Harness, n: u32| h.reactor.trees().get(ws(n)).unwrap().describe();
    assert_eq!(desc(&h, 1), "v[]", "workspace 1 is on the tall monitor");
    assert_eq!(desc(&h, 2), "h[]", "workspace 2 is on the wide one");
    h.launch(PID, &[w(1, 11, rect(10., 10., 50., 50.)), w(2, 12, rect(700., 10., 50., 50.))]);
    assert_eq!(desc(&h, 1), "v[1]");
    assert_eq!(desc(&h, 2), "h[2]");
}

#[test]
fn explicit_orientation_ignores_the_monitor_shape() {
    let mut h = Harness::with_config_text(&cfg("default-root-container-orientation = 'vertical'"));
    h.screens(&[("W", 1, rect(0., 0., 900., 600.))]);
    assert_eq!(h.reactor.trees().get(ws(1)).unwrap().describe(), "v[]");
}

#[test]
fn force_assignments_are_applied_live_to_the_workspace_model() {
    let mut h = Harness::with_config_text(&cfg(""));
    h.screens_named(&[("L", 1, rect(0., 0., 1000., 600.), Some("A")), ("R", 2, rect(1000., 0., 1000., 600.), Some("B"))]);
    let forced = |h: &Harness, n: u32| h.reactor.workspaces().forced_display(ws(n), h.reactor.displays());
    assert_eq!(forced(&h, 3), None);
    reload(&mut h, &cfg("[workspace-to-monitor-force-assignment]\n3 = 'secondary'\n"));
    assert_eq!(forced(&h, 3).map(|d| d.as_str().to_owned()), Some("R".to_owned()));
    reload(&mut h, &cfg(""));
    assert_eq!(forced(&h, 3), None);
}

#[test]
fn auto_back_and_forth_is_applied_live() {
    let mut h = one_display(&cfg(""));
    h.cmd(Command::Workspace("2".into())).unwrap();
    reload(&mut h, &cfg("workspace-auto-back-and-forth = true"));
    h.cmd(Command::Workspace("2".into())).unwrap();
    assert_eq!(h.active("D"), Some(1), "`workspace 2` on workspace 2 went back to 1");
}

#[test]
fn a_changed_workspace_pool_requires_restart_and_keeps_the_old_pool() {
    let mut h = with_windows(&cfg(""), 1);
    let before = h.reactor.workspaces().names();
    reload(&mut h, &format!("[mode.main.binding]\nalt-t = 'workspace T'\n{GAPS}"));
    assert_eq!(h.reactor.workspaces().names(), before, "the running pool is untouched");
    assert_eq!(px(&h, 1, 1), (20, 30, 860, 530), "every other setting applied");
    assert!(h.reactor.config().unwrap().workspaces.contains(&"T".to_owned()));
    // The command for the missing workspace fails cleanly instead of crashing.
    let e = h.cmd(Command::Workspace("T".into())).unwrap_err();
    assert_eq!(e, WorkspaceError::NoSuchWorkspace("T".into()));
}

#[test]
fn force_assignments_for_workspaces_outside_the_running_pool_are_skipped_not_fatal() {
    let mut h = one_display(&cfg(""));
    reload(&mut h, &cfg("[workspace-to-monitor-force-assignment]\nZ = 'main'\n3 = 'main'\n"));
    assert!(h.reactor.workspaces().forced_display(ws(3), h.reactor.displays()).is_some());
}

#[test]
fn an_invalid_reload_through_the_watcher_leaves_the_running_config_untouched() {
    use crate::config::watch::WatchState;
    let dir = std::env::temp_dir().join(format!("dowm-reactor-reload-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("dowm.toml");
    std::fs::write(&path, cfg("")).unwrap();
    let mut watcher = WatchState::seeded(config::ConfigSource::File(path.clone()));

    let mut h = with_windows(&cfg(""), 2);
    let applied = Arc::clone(h.reactor.config().unwrap());
    h.effects();

    // The edit is broken: the watcher reports errors, nothing reaches the reactor.
    std::fs::write(&path, cfg(&format!("bogus-key = 1\n{GAPS}"))).unwrap();
    let errors = { watcher.poll(); watcher.poll() }.expect("a change").expect_err("invalid");
    assert!(errors.mentions("bogus-key"), "{errors}");
    assert_eq!(h.reactor.config(), Some(&applied));
    assert_eq!(px(&h, 1, 1), (0, 0, 450, 600));
    assert!(h.effects().is_empty());

    // Fixing it applies it, live.
    std::fs::write(&path, cfg(GAPS)).unwrap();
    let fixed = { watcher.poll(); watcher.poll() }.expect("a change").expect("valid");
    h.send(Event::ConfigReloaded(Arc::new(fixed)));
    assert_eq!(px(&h, 1, 1), (20, 30, 425, 530));
    _ = std::fs::remove_dir_all(&dir);
}

// ---- action lists ----

#[test]
fn actions_run_in_order_and_meta_commands_become_effects() {
    let mut h = one_display(&cfg("[mode.service.binding]\nesc = 'mode main'\n"));
    let actions = [
        Action::Reactor(Command::Workspace("2".into())),
        Action::ExecAndForget("echo hi".into()),
        Action::ReloadConfig,
        Action::Mode("service".into()),
    ];
    h.send(Event::Actions(actions.to_vec()));
    assert_eq!(h.active("D"), Some(2));
    assert_eq!(h.effects(), vec![
        Effect::Exec("echo hi".into()),
        Effect::ReloadConfig,
        Effect::SetMode("service".into()),
    ]);
}

#[test]
fn a_failing_action_stops_the_rest_of_the_list() {
    let mut h = one_display(&cfg(""));
    h.send(Event::Actions(vec![
        Action::ExecAndForget("first".into()),
        Action::Reactor(Command::Workspace("nonexistent".into())),
        Action::Mode("main".into()),
        Action::ExecAndForget("never".into()),
    ]));
    assert_eq!(
        h.effects(),
        vec![Effect::Exec("first".into()), Effect::SetMode("main".into())],
        "commands after the failure are skipped, but the mode switch still runs"
    );
    let result = h.reactor.run_actions(&[Action::Reactor(Command::Workspace("nonexistent".into()))], None);
    assert_eq!(result, Err(CommandError::Workspace(WorkspaceError::NoSuchWorkspace("nonexistent".into()))));
}

#[test]
fn a_trailing_mode_switch_runs_even_when_an_earlier_command_fails() {
    // The service-mode `backspace` binding from the AeroSpace config, on a workspace with
    // no windows: `close-all-windows-but-current` fails, `mode main` must still run.
    let c = config::parse(AEROSPACE_FIXTURE).unwrap();
    let key = config::Hotkey::parse("backspace", config::KeyPreset::Qwerty).unwrap();
    let actions = c.modes["service"].bindings[&key].actions.clone();
    assert_eq!(actions.len(), 2);
    let mut h = one_display(AEROSPACE_FIXTURE);
    let result = h.reactor.run_actions(&actions, None);
    assert_eq!(result, Err(CommandError::Tiling(TilingError::NoFocusedWindow)), "the failure is still reported");
    assert_eq!(h.effects(), vec![Effect::SetMode("main".into())]);
}

#[test]
fn the_reload_then_mode_main_chain_from_the_service_mode() {
    let mut h = one_display(&cfg(""));
    h.send(Event::Actions(vec![Action::ReloadConfig, Action::Mode("main".into())]));
    assert_eq!(h.effects(), vec![Effect::ReloadConfig, Effect::SetMode("main".into())]);
}

#[test]
fn after_startup_command_runs_once_when_startup_finishes() {
    let mut h = one_display(&cfg("after-startup-command = ['workspace 3', 'exec-and-forget say hello']"));
    assert_eq!(h.active("D"), Some(1), "not before start-up finishes");
    h.send(Event::StartupFinished);
    assert_eq!(h.active("D"), Some(3));
    assert!(h.effects().contains(&Effect::Exec("say hello".into())));
    h.cmd(Command::Workspace("1".into())).unwrap();
    h.send(Event::StartupFinished);
    assert_eq!(h.active("D"), Some(1), "a second StartupFinished does not rerun it");
}

#[test]
fn after_login_command_is_never_run() {
    let mut h = one_display(&cfg("after-login-command = ['workspace 4']"));
    h.send(Event::StartupFinished);
    assert_eq!(h.active("D"), Some(1));
}

// ---- on-window-detected ----

#[test]
fn the_aerospace_rules_send_wezterm_to_t_and_chrome_to_b() {
    let mut h = one_display(AEROSPACE_FIXTURE);
    h.launch_app(WEZ_PID, WEZTERM, "WezTerm", &[w(1, 21, rect(0., 0., 300., 300.))]);
    h.launch_app(CHROME_PID, CHROME, "Google Chrome", &[w(1, 31, rect(0., 0., 300., 300.))]);
    let t = ws_named(&h, "T");
    let b = ws_named(&h, "B");
    assert_eq!(h.reactor.workspaces().workspace_of(wid(WEZ_PID, 1)), Some(t));
    assert_eq!(h.reactor.workspaces().workspace_of(wid(CHROME_PID, 1)), Some(b));
    // Both are hidden with their workspaces: the visible one is still 1.
    assert_eq!(h.active("D"), Some(1));
    assert_eq!(h.reactor.trees().get(t).unwrap().tiled_windows(), vec![wid(WEZ_PID, 1)]);
}

#[test]
fn a_window_created_later_is_detected_too() {
    let mut h = one_display(AEROSPACE_FIXTURE);
    h.launch_app(WEZ_PID, WEZTERM, "WezTerm", &[]);
    h.ax_created_as(WEZ_PID, w(1, 21, rect(0., 0., 300., 300.)), "new window", WEZTERM);
    assert_eq!(h.reactor.workspaces().workspace_of(wid(WEZ_PID, 1)), Some(ws_named(&h, "T")));
}

#[test]
fn an_app_no_rule_matches_stays_where_it_landed() {
    let mut h = one_display(AEROSPACE_FIXTURE);
    h.launch_app(PID, "com.apple.Safari", "Safari", &[w(1, 11, rect(0., 0., 300., 300.))]);
    assert_eq!(h.workspace_of(wid(PID, 1)), Some(1));
}

#[test]
fn the_rule_acts_on_the_detected_window_not_the_focused_one() {
    let mut h = one_display(AEROSPACE_FIXTURE);
    h.launch(PID, &[w(1, 11, rect(0., 0., 300., 300.))]);
    focus(&mut h, 1);
    h.launch_app(WEZ_PID, WEZTERM, "WezTerm", &[w(1, 21, rect(0., 0., 300., 300.))]);
    assert_eq!(h.workspace_of(wid(PID, 1)), Some(1), "the focused window stays");
    assert_eq!(h.reactor.workspaces().workspace_of(wid(WEZ_PID, 1)), Some(ws_named(&h, "T")));
    assert_eq!(h.reactor.focused_window(), Some(wid(PID, 1)));
}

#[test]
fn a_rule_runs_once_per_window_not_on_every_update() {
    let mut h = one_display(AEROSPACE_FIXTURE);
    h.launch_app(WEZ_PID, WEZTERM, "WezTerm", &[w(1, 21, rect(0., 0., 300., 300.))]);
    h.cmd(Command::MoveNodeToWorkspace { workspace: "2".into(), window: Some(wid(WEZ_PID, 1)), follow: false }).unwrap();
    // The app reports the same window again (an inventory refresh): not a new window.
    h.ax_created_as(WEZ_PID, w(1, 21, rect(0., 0., 300., 300.)), "again", WEZTERM);
    assert_eq!(h.workspace_of(wid(WEZ_PID, 1)), Some(2), "the user's move is not undone");
}

#[test]
fn layout_floating_floats_the_detected_window() {
    let text = "[[on-window-detected]]\nif.app-id = 'com.example.float'\nrun = 'layout floating'\n[mode.main.binding]\n";
    let mut h = one_display(text);
    h.launch(PID, &[w(1, 11, rect(0., 0., 300., 300.))]);
    h.launch_app(WEZ_PID, "com.example.float", "Floaty", &[w(1, 21, rect(5., 5., 100., 100.))]);
    let tree = h.reactor.trees().get(ws(1)).unwrap();
    assert!(tree.is_floating(wid(WEZ_PID, 1)));
    assert!(tree.is_tiled(wid(PID, 1)), "the other window stays tiled");
}

#[test]
fn multi_command_rules_follow_the_window_to_its_new_workspace() {
    let text = "[[on-window-detected]]\nif.app-id = 'com.example.x'\nrun = ['move-node-to-workspace 3', 'layout floating']\n[mode.main.binding]\n";
    let mut h = one_display(text);
    h.launch_app(PID, "com.example.x", "X", &[w(1, 11, rect(5., 5., 100., 100.))]);
    assert_eq!(h.workspace_of(wid(PID, 1)), Some(3));
    assert!(h.reactor.trees().get(ws(3)).unwrap().is_floating(wid(PID, 1)), "the second command saw the window on workspace 3");
}

#[test]
fn rules_match_app_name_and_window_title_regexes() {
    let text = "[[on-window-detected]]\nif.app-name-regex-substring = 'wez'\nrun = 'move-node-to-workspace 4'\n\
                [[on-window-detected]]\nif.window-title-regex-substring = 'scratch'\nrun = 'move-node-to-workspace 5'\n[mode.main.binding]\n";
    let mut h = one_display(text);
    h.launch_app(WEZ_PID, "org.x", "WezTerm", &[w(1, 21, rect(0., 0., 300., 300.))]);
    h.launch_app(PID, "org.y", "Notes", &[]);
    h.ax_created_as(PID, w(1, 11, rect(0., 0., 100., 100.)), "My Scratch pad", "org.y");
    assert_eq!(h.reactor.workspaces().workspace_of(wid(WEZ_PID, 1)).map(WorkspaceId::number), Some(4));
    assert_eq!(h.workspace_of(wid(PID, 1)), Some(5));
}

#[test]
fn if_workspace_matches_the_workspace_the_window_landed_on() {
    let text = "[[on-window-detected]]\nif.workspace = '2'\nrun = 'move-node-to-workspace 6'\n[mode.main.binding]\n";
    let mut h = one_display(text);
    h.launch(PID, &[w(1, 11, rect(0., 0., 100., 100.))]);
    assert_eq!(h.workspace_of(wid(PID, 1)), Some(1), "landed on 1: no match");
    h.cmd(Command::Workspace("2".into())).unwrap();
    h.ax_created(PID, w(2, 12, rect(0., 0., 100., 100.)));
    assert_eq!(h.workspace_of(wid(PID, 2)), Some(6), "landed on the visible workspace 2");
}

#[test]
fn the_first_matching_rule_wins_unless_it_checks_further() {
    let first_only = "[[on-window-detected]]\nif.app-id = 'x'\nrun = 'move-node-to-workspace 3'\n\
                      [[on-window-detected]]\nif.app-id = 'x'\nrun = 'move-node-to-workspace 4'\n[mode.main.binding]\n";
    let mut h = one_display(first_only);
    h.launch_app(PID, "x", "X", &[w(1, 11, rect(0., 0., 100., 100.))]);
    assert_eq!(h.workspace_of(wid(PID, 1)), Some(3));

    let chained = "[[on-window-detected]]\nif.app-id = 'x'\ncheck-further-callbacks = true\nrun = 'move-node-to-workspace 3'\n\
                   [[on-window-detected]]\nif.app-id = 'x'\nrun = 'move-node-to-workspace 4'\n[mode.main.binding]\n";
    let mut h = one_display(chained);
    h.launch_app(PID, "x", "X", &[w(1, 11, rect(0., 0., 100., 100.))]);
    assert_eq!(h.workspace_of(wid(PID, 1)), Some(4), "both ran, the later one last");
}

#[test]
fn during_startup_rules_match_only_before_startup_finishes() {
    let text = "[[on-window-detected]]\nif.during-aerospace-startup = true\nrun = 'move-node-to-workspace 7'\n\
                [[on-window-detected]]\nif.during-aerospace-startup = false\nrun = 'move-node-to-workspace 8'\n[mode.main.binding]\n";
    let mut h = one_display(text);
    h.launch(PID, &[w(1, 11, rect(0., 0., 100., 100.))]);
    assert_eq!(h.workspace_of(wid(PID, 1)), Some(7));
    h.send(Event::StartupFinished);
    h.ax_created(PID, w(2, 12, rect(0., 0., 100., 100.)));
    assert_eq!(h.workspace_of(wid(PID, 2)), Some(8));
}

#[test]
fn rules_can_emit_effects_like_exec() {
    let text = "[[on-window-detected]]\nif.app-id = 'x'\nrun = 'exec-and-forget notify-send hi'\n[mode.main.binding]\n";
    let mut h = one_display(text);
    h.launch_app(PID, "x", "X", &[w(1, 11, rect(0., 0., 100., 100.))]);
    assert!(h.effects().contains(&Effect::Exec("notify-send hi".into())));
}

#[test]
fn a_failing_rule_command_does_not_break_the_reactor() {
    let text = "[[on-window-detected]]\nif.app-id = 'x'\nrun = ['move-node-to-workspace 3', 'resize smart +10']\n[mode.main.binding]\n";
    let mut h = one_display(text);
    h.launch_app(PID, "x", "X", &[w(1, 11, rect(0., 0., 100., 100.))]);
    assert_eq!(h.workspace_of(wid(PID, 1)), Some(3), "the first command still happened");
}

#[test]
fn rules_reloaded_live_apply_to_windows_detected_afterwards() {
    let mut h = one_display(&cfg(""));
    h.launch_app(PID, "x", "X", &[w(1, 11, rect(0., 0., 100., 100.))]);
    assert_eq!(h.workspace_of(wid(PID, 1)), Some(1));
    reload(&mut h, "[[on-window-detected]]\nif.app-id = 'x'\nrun = 'move-node-to-workspace 4'\n[mode.main.binding]\n");
    h.ax_created_as(PID, w(2, 12, rect(0., 0., 100., 100.)), "later", "x");
    assert_eq!(h.workspace_of(wid(PID, 2)), Some(4));
    assert_eq!(h.workspace_of(wid(PID, 1)), Some(1), "already-detected windows are not revisited");
    reload(&mut h, &cfg(""));
    h.ax_created_as(PID, w(3, 13, rect(0., 0., 100., 100.)), "later still", "x");
    assert_eq!(h.workspace_of(wid(PID, 3)), Some(1), "the rule is gone");
}

#[test]
fn non_standard_windows_are_not_matched() {
    let text = "[[on-window-detected]]\nrun = 'move-node-to-workspace 4'\n[mode.main.binding]\n";
    let mut h = one_display(text);
    h.launch(PID, &[]);
    let mut dialog = crate::model::window_store::tests::info("dialog", rect(0., 0., 100., 100.), Some(11));
    dialog.is_standard = false;
    h.send(Event::WindowCreated(wid(PID, 1), dialog, None, None));
    assert_eq!(h.workspace_of(wid(PID, 1)), Some(1));
}

// ---- focus callbacks ----

const CALLBACKS: &str = "on-focused-monitor-changed = ['move-mouse monitor-lazy-center']\non-focus-changed = 'move-mouse window-lazy-center'\n";

fn two_windows_with_callbacks() -> Harness {
    let mut h = with_windows(&cfg(CALLBACKS), 2);
    finish_startup(&mut h);
    h
}

#[test]
fn on_focus_changed_fires_once_per_change() {
    let mut h = two_windows_with_callbacks();
    focus(&mut h, 1);
    h.effects();
    focus(&mut h, 2);
    let moves = mouse_effects(&h.effects());
    assert_eq!(moves, vec![(rect(10., 10., 100., 100.), true)], "window-lazy-center on the newly focused window");
    focus(&mut h, 2);
    assert!(mouse_effects(&h.effects()).is_empty(), "the same focus again is not a change");
    focus(&mut h, 1);
    assert_eq!(mouse_effects(&h.effects()).len(), 1);
}

#[test]
fn callbacks_do_not_fire_before_startup_finishes() {
    let mut h = with_windows(&cfg(CALLBACKS), 2);
    focus(&mut h, 2);
    assert!(mouse_effects(&h.effects()).is_empty());
    h.send(Event::StartupFinished);
    h.effects();
    focus(&mut h, 1);
    assert_eq!(mouse_effects(&h.effects()).len(), 1);
}

#[test]
fn the_focus_command_fires_the_callback_exactly_once() {
    let mut h = two_windows_with_callbacks();
    focus(&mut h, 1);
    h.effects();
    h.tile(Command::Focus { direction: Direction::Right, wrap: false }).unwrap();
    assert_eq!(mouse_effects(&h.effects()).len(), 1);
    // Through the action path too (a hotkey): once.
    h.send(Event::Actions(vec![Action::Reactor(Command::Focus { direction: Direction::Left, wrap: false })]));
    assert_eq!(mouse_effects(&h.effects()).len(), 1);
    // A focus command that goes nowhere changes nothing.
    h.send(Event::Actions(vec![Action::Reactor(Command::Focus { direction: Direction::Left, wrap: false })]));
    assert!(mouse_effects(&h.effects()).is_empty());
}

#[test]
fn on_focused_monitor_changed_fires_once_then_the_focus_callback() {
    let mut h = Harness::with_config_text(&cfg(CALLBACKS));
    h.screens(&[("L", 1, rect(0., 0., 1000., 600.)), ("R", 2, rect(1000., 0., 1000., 600.))]);
    h.launch(PID, &[w(1, 11, rect(10., 10., 100., 100.)), w(2, 12, rect(1100., 10., 100., 100.))]);
    focus(&mut h, 1);
    finish_startup(&mut h);
    h.cmd(Command::FocusMonitor(crate::model::workspace::MonitorTarget::Next { wrap: false })).unwrap();
    let moves = mouse_effects(&h.effects());
    assert_eq!(moves.len(), 2, "monitor callback, then the window callback");
    assert_eq!(moves[0], (rect(1000., 0., 1000., 600.), true), "monitor-lazy-center on the new monitor first");
    assert_eq!(moves[1], (rect(1100., 10., 100., 100.), true), "then window-lazy-center on its window");
    // Staying on the monitor fires nothing.
    h.cmd(Command::FocusMonitor(crate::model::workspace::MonitorTarget::Next { wrap: false })).ok();
}

#[test]
fn switching_workspace_on_one_monitor_fires_only_the_focus_callback() {
    let mut h = two_windows_with_callbacks();
    focus(&mut h, 1);
    h.effects();
    h.cmd(Command::MoveNodeToWorkspace { workspace: "2".into(), window: Some(wid(PID, 2)), follow: false }).unwrap();
    h.effects();
    h.cmd(Command::Workspace("2".into())).unwrap();
    let moves = mouse_effects(&h.effects());
    assert_eq!(moves, vec![(rect(10., 10., 100., 100.), true)], "one callback: window-lazy-center, no monitor change");
}

#[test]
fn a_workspace_with_no_window_fires_nothing_harmful() {
    let mut h = two_windows_with_callbacks();
    focus(&mut h, 1);
    h.effects();
    h.cmd(Command::Workspace("5".into())).unwrap();
    assert!(mouse_effects(&h.effects()).is_empty(), "no window to center on: the callback fails quietly");
}

#[test]
fn callbacks_accept_string_and_array_forms_and_run_other_commands_too() {
    let mut h = with_windows(&cfg("on-focus-changed = ['exec-and-forget echo focus', 'move-mouse window-force-center']"), 2);
    finish_startup(&mut h);
    focus(&mut h, 2);
    let effects = h.effects();
    assert!(effects.contains(&Effect::Exec("echo focus".into())));
    assert_eq!(mouse_effects(&effects), vec![(rect(10., 10., 100., 100.), false)], "force-center is not lazy");
}

#[test]
fn a_callback_that_changes_focus_does_not_recurse() {
    let mut h = with_windows(&cfg("on-focus-changed = ['focus right', 'exec-and-forget tick']"), 3);
    finish_startup(&mut h);
    focus(&mut h, 2);
    let ticks = h.effects().iter().filter(|e| **e == Effect::Exec("tick".into())).count();
    assert_eq!(ticks, 1, "the callback's own focus change is not a new change");
}

#[test]
fn callbacks_can_be_replaced_live() {
    let mut h = with_windows(&cfg(CALLBACKS), 2);
    finish_startup(&mut h);
    reload(&mut h, &cfg("on-focus-changed = []"));
    h.effects();
    focus(&mut h, 2);
    assert!(h.effects().is_empty(), "the old callback is gone");
    reload(&mut h, &cfg("on-focus-changed = 'exec-and-forget new'"));
    h.effects();
    focus(&mut h, 1);
    assert!(h.effects().contains(&Effect::Exec("new".into())));
}

#[test]
fn applying_a_config_does_not_itself_look_like_a_focus_change() {
    let mut h = with_windows(&cfg(""), 2);
    finish_startup(&mut h);
    focus(&mut h, 2);
    h.effects();
    reload(&mut h, &cfg(CALLBACKS));
    assert!(mouse_effects(&h.effects()).is_empty());
    focus(&mut h, 2);
    assert!(mouse_effects(&h.effects()).is_empty());
}

#[test]
fn without_any_config_there_are_no_effects() {
    let mut h = Harness::new();
    h.screens(&[("D", 1, rect(0., 0., 900., 600.))]);
    h.launch(PID, &[w(1, 11, rect(0., 0., 100., 100.))]);
    focus(&mut h, 1);
    assert!(h.effects().is_empty());
}

// ---- move-mouse ----

#[test]
fn move_mouse_targets_the_focused_monitor_and_window() {
    let mut h = with_windows(&cfg(""), 2);
    focus(&mut h, 2);
    h.reactor.run_command(Command::MoveMouse { target: super::MouseTarget::Monitor, lazy: false }).unwrap();
    h.reactor.run_command(Command::MoveMouse { target: super::MouseTarget::Window, lazy: true }).unwrap();
    assert_eq!(h.effects(), vec![
        Effect::MoveMouse { rect: rect(0., 0., 900., 600.), lazy: false },
        Effect::MoveMouse { rect: rect(10., 10., 100., 100.), lazy: true },
    ]);
}

#[test]
fn move_mouse_to_a_window_needs_one() {
    let mut h = one_display(&cfg(""));
    let result = h.reactor.run_command(Command::MoveMouse { target: super::MouseTarget::Window, lazy: true });
    assert_eq!(result, Err(CommandError::Workspace(WorkspaceError::NoFocusedWindow)));
    assert!(h.effects().is_empty());
}

// ---- close-all-windows-but-current ----

#[test]
fn close_all_windows_but_current_asks_every_other_window_to_close() {
    let mut h = with_windows(&cfg(""), 3);
    focus(&mut h, 2);
    h.app_requests(PID);
    h.reactor.run_command(Command::CloseAllWindowsButCurrent).unwrap();
    let closed: Vec<_> = h
        .app_requests(PID)
        .into_iter()
        .filter_map(|r| match r {
            app::Request::CloseWindow(id) => id.map(|i| i.as_u32()),
            _ => None,
        })
        .collect();
    assert_eq!(closed, vec![11, 13], "windows 1 and 3, not the focused 2");
}

#[test]
fn close_all_windows_but_current_leaves_other_workspaces_alone_and_needs_a_focus() {
    let mut h = with_windows(&cfg(""), 3);
    focus(&mut h, 1);
    h.cmd(Command::MoveNodeToWorkspace { workspace: "2".into(), window: Some(wid(PID, 3)), follow: false }).unwrap();
    h.app_requests(PID);
    h.reactor.run_command(Command::CloseAllWindowsButCurrent).unwrap();
    let closed = h.app_requests(PID).into_iter().filter(|r| matches!(r, app::Request::CloseWindow(_))).count();
    assert_eq!(closed, 1, "only window 2; window 3 lives on workspace 2");

    let mut empty = one_display(&cfg(""));
    assert_eq!(
        empty.reactor.run_command(Command::CloseAllWindowsButCurrent),
        Err(CommandError::Tiling(TilingError::NoFocusedWindow))
    );
}

// ---- resize to an exact size ----

#[test]
fn resize_to_sets_the_window_size() {
    let mut h = with_windows(&cfg(""), 2);
    focus(&mut h, 1);
    h.tile(Command::ResizeTo { dimension: ResizeDimension::Width, size: 600 }).unwrap();
    assert_eq!(px(&h, 1, 1).2, 600);
    assert_eq!(px(&h, 1, 2).2, 300, "the sibling gives up the difference");
    h.tile(Command::ResizeTo { dimension: ResizeDimension::Width, size: 600 }).unwrap();
    assert_eq!(px(&h, 1, 1).2, 600, "already that size: nothing to do");
    assert_eq!(h.tile(Command::ResizeTo { dimension: ResizeDimension::Smart, size: 5 }), Err(TilingError::NothingToResize));
}

// ---- focus follows the mouse ----

fn ffm_harness(on: bool) -> Harness {
    let text = cfg(&format!("focus-follows-mouse = {on}"));
    let mut h = with_windows(&text, 2);
    focus(&mut h, 1);
    h.app_requests(PID);
    h
}

fn raised(h: &mut Harness) -> Vec<WindowId> {
    h.app_requests(PID)
        .into_iter()
        .filter_map(|r| match r {
            app::Request::Raise(windows, ..) => Some(windows),
            _ => None,
        })
        .flatten()
        .collect()
}

fn settle(h: &mut Harness, sys_id: u32) {
    h.send(Event::PointerSettledOn(crate::sys::window_server::WindowServerId::new(sys_id)));
}

#[test]
fn the_pointer_settling_on_another_window_focuses_it() {
    let mut h = ffm_harness(true);
    settle(&mut h, 12);
    assert_eq!(h.reactor.focused_window(), Some(wid(PID, 2)));
    assert_eq!(raised(&mut h), vec![wid(PID, 2)]);
    assert_eq!(h.reactor.trees().get(ws(1)).unwrap().focused_window(), Some(wid(PID, 2)));
}

#[test]
fn nothing_happens_when_the_window_is_already_focused_or_unknown() {
    let mut h = ffm_harness(true);
    settle(&mut h, 11);
    settle(&mut h, 999);
    assert!(raised(&mut h).is_empty());
    assert_eq!(h.reactor.focused_window(), Some(wid(PID, 1)));
}

#[test]
fn focus_follows_mouse_off_does_nothing_and_is_toggled_live() {
    let mut h = ffm_harness(false);
    settle(&mut h, 12);
    assert_eq!(h.reactor.focused_window(), Some(wid(PID, 1)));
    assert!(raised(&mut h).is_empty());
    reload(&mut h, &cfg("focus-follows-mouse = true"));
    settle(&mut h, 12);
    assert_eq!(h.reactor.focused_window(), Some(wid(PID, 2)));
    reload(&mut h, &cfg("focus-follows-mouse = false"));
    h.app_requests(PID);
    settle(&mut h, 11);
    assert_eq!(h.reactor.focused_window(), Some(wid(PID, 2)), "off again");
}

#[test]
fn windows_on_hidden_workspaces_and_minimized_windows_are_not_focused() {
    let mut h = ffm_harness(true);
    h.cmd(Command::MoveNodeToWorkspace { workspace: "2".into(), window: Some(wid(PID, 2)), follow: false }).unwrap();
    h.app_requests(PID);
    settle(&mut h, 12);
    assert_eq!(h.reactor.focused_window(), Some(wid(PID, 1)), "window 2 is parked with workspace 2");
    assert!(raised(&mut h).is_empty());

    let mut h = ffm_harness(true);
    h.send(Event::WindowMinimized(wid(PID, 2)));
    h.app_requests(PID);
    settle(&mut h, 12);
    assert_eq!(h.reactor.focused_window(), Some(wid(PID, 1)));
}

#[test]
fn pointer_focus_on_another_monitor_moves_monitor_focus() {
    let text = cfg("focus-follows-mouse = true");
    let mut h = Harness::with_config_text(&text);
    h.screens(&[("L", 1, rect(0., 0., 1000., 600.)), ("R", 2, rect(1000., 0., 1000., 600.))]);
    h.launch(PID, &[w(1, 11, rect(10., 10., 100., 100.)), w(2, 12, rect(1100., 10., 100., 100.))]);
    focus(&mut h, 1);
    assert_eq!(h.focused().as_deref(), Some("L"));
    settle(&mut h, 12);
    assert_eq!(h.focused().as_deref(), Some("R"));
    assert_eq!(h.reactor.focused_window(), Some(wid(PID, 2)));
}

// ---- the existing focus command now raises ----

#[test]
fn the_focus_command_asks_the_app_to_raise_the_window() {
    let mut h = with_windows(&cfg(""), 2);
    focus(&mut h, 1);
    h.app_requests(PID);
    h.tile(Command::Focus { direction: Direction::Right, wrap: false }).unwrap();
    assert_eq!(raised(&mut h), vec![wid(PID, 2)]);
}
