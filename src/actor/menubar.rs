//! The menu bar indicator: an `NSStatusItem` showing the focused workspace and, when it
//! is not `main`, the active binding mode.
//!
//! It is a subscriber like any IPC client: it receives [`Event`]s from the bus on a
//! channel and updates the title on the main thread, where AppKit requires it.

use dowm_protocol::{Event, MonitorInfo};
use objc2::MainThreadMarker;
use objc2_app_kit::{NSStatusBar, NSVariableStatusItemLength};
use objc2_foundation::NSString;

use crate::actor;

/// What the indicator displays, derived from the event stream.
#[derive(Debug, PartialEq)]
pub struct IndicatorState {
    workspace: Option<String>,
    /// Every display with the workspace it shows, left to right.
    monitors: Vec<MonitorInfo>,
    mode: String,
}

impl Default for IndicatorState {
    fn default() -> Self { Self { workspace: None, monitors: Vec::new(), mode: "main".to_owned() } }
}

impl IndicatorState {
    pub fn apply(&mut self, event: &Event) {
        match event {
            Event::WorkspaceChanged { workspace, .. } => self.workspace = Some(workspace.clone()),
            Event::MonitorsChanged { monitors } => {
                let mut monitors = monitors.clone();
                monitors.sort_by(|a, b| {
                    a.frame.x.total_cmp(&b.frame.x).then(a.frame.y.total_cmp(&b.frame.y)).then_with(|| a.id.cmp(&b.id))
                });
                self.monitors = monitors;
            }
            Event::ModeChanged { mode } => self.mode = mode.clone(),
            Event::FocusChanged { .. } => {}
        }
    }

    /// `2`, or `2 · resize` outside the main mode, or `–` before the first workspace.
    /// With several displays, each one's workspace in screen order (`[1] | 3`), the
    /// focused display in brackets.
    pub fn title(&self) -> String {
        let workspace = if self.monitors.len() > 1 {
            let segments: Vec<String> = self
                .monitors
                .iter()
                .map(|m| {
                    let name = m.workspace.as_deref().unwrap_or("\u{2013}");
                    if m.focused { format!("[{name}]") } else { name.to_owned() }
                })
                .collect();
            segments.join(" | ")
        } else {
            self.workspace.clone().unwrap_or_else(|| "\u{2013}".to_owned())
        };
        let workspace = workspace.as_str();
        if self.mode == "main" { workspace.to_owned() } else { format!("{workspace} \u{b7} {}", self.mode) }
    }
}

/// Show the indicator and keep it current until the channel closes. Main thread only.
pub async fn run(mtm: MainThreadMarker, mut events: actor::Receiver<Event>) {
    let item = NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
    let show = |state: &IndicatorState| {
        if let Some(button) = item.button(mtm) {
            button.setTitle(&NSString::from_str(&state.title()));
        }
    };
    let mut state = IndicatorState::default();
    show(&state);
    while let Some((_, event)) = events.recv().await {
        state.apply(&event);
        show(&state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws(name: &str) -> Event { Event::WorkspaceChanged { workspace: name.into(), previous: None } }

    #[test]
    fn it_shows_a_dash_until_a_workspace_is_known() {
        assert_eq!(IndicatorState::default().title(), "\u{2013}");
    }

    #[test]
    fn it_shows_the_focused_workspace() {
        let mut s = IndicatorState::default();
        s.apply(&ws("2"));
        assert_eq!(s.title(), "2");
        s.apply(&ws("7"));
        assert_eq!(s.title(), "7");
    }

    #[test]
    fn it_appends_any_mode_other_than_main() {
        let mut s = IndicatorState::default();
        s.apply(&ws("2"));
        s.apply(&Event::ModeChanged { mode: "resize".into() });
        assert_eq!(s.title(), "2 \u{b7} resize");
        s.apply(&Event::ModeChanged { mode: "main".into() });
        assert_eq!(s.title(), "2");
    }

    fn mon(id: &str, x: f64, ws: Option<&str>, focused: bool) -> MonitorInfo {
        MonitorInfo {
            id: id.into(),
            name: None,
            focused,
            workspace: ws.map(Into::into),
            frame: dowm_protocol::Rect { x, y: 0.0, width: 100.0, height: 100.0 },
        }
    }

    #[test]
    fn it_shows_each_display_in_screen_order_with_the_focused_one_bracketed() {
        let mut s = IndicatorState::default();
        s.apply(&Event::MonitorsChanged {
            monitors: vec![mon("b", 100.0, Some("3"), false), mon("a", 0.0, Some("1"), true)],
        });
        assert_eq!(s.title(), "[1] | 3");
        s.apply(&Event::ModeChanged { mode: "resize".into() });
        assert_eq!(s.title(), "[1] | 3 \u{b7} resize");
    }

    #[test]
    fn it_shows_a_dash_for_a_display_without_a_workspace() {
        let mut s = IndicatorState::default();
        s.apply(&Event::MonitorsChanged {
            monitors: vec![mon("a", 0.0, Some("1"), true), mon("b", 100.0, None, false)],
        });
        assert_eq!(s.title(), "[1] | \u{2013}");
    }

    #[test]
    fn a_single_display_looks_like_before() {
        let mut s = IndicatorState::default();
        s.apply(&ws("2"));
        s.apply(&Event::MonitorsChanged { monitors: vec![mon("a", 0.0, Some("2"), true)] });
        assert_eq!(s.title(), "2");
    }

    #[test]
    fn unplugging_a_display_collapses_the_title() {
        let mut s = IndicatorState::default();
        s.apply(&ws("1"));
        s.apply(&Event::MonitorsChanged {
            monitors: vec![mon("a", 0.0, Some("1"), true), mon("b", 100.0, Some("2"), false)],
        });
        s.apply(&Event::MonitorsChanged { monitors: vec![mon("a", 0.0, Some("1"), true)] });
        assert_eq!(s.title(), "1");
    }

    #[test]
    fn focus_events_do_not_change_the_title() {
        let mut s = IndicatorState::default();
        s.apply(&ws("2"));
        s.apply(&Event::FocusChanged { window: Some(1) });
        assert_eq!(s.title(), "2");
    }
}
