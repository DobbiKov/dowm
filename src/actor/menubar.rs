//! The menu bar indicator: an `NSStatusItem` showing the focused workspace and, when it
//! is not `main`, the active binding mode.
//!
//! It is a subscriber like any IPC client: it receives [`Event`]s from the bus on a
//! channel and updates the title on the main thread, where AppKit requires it.

use dowm_protocol::Event;
use objc2::MainThreadMarker;
use objc2_app_kit::{NSStatusBar, NSVariableStatusItemLength};
use objc2_foundation::NSString;

use crate::actor;

/// What the indicator displays, derived from the event stream.
#[derive(Debug, PartialEq, Eq)]
pub struct IndicatorState {
    workspace: Option<String>,
    mode: String,
}

impl Default for IndicatorState {
    fn default() -> Self { Self { workspace: None, mode: "main".to_owned() } }
}

impl IndicatorState {
    pub fn apply(&mut self, event: &Event) {
        match event {
            Event::WorkspaceChanged { workspace, .. } => self.workspace = Some(workspace.clone()),
            Event::ModeChanged { mode } => self.mode = mode.clone(),
            Event::FocusChanged { .. } => {}
        }
    }

    /// `2`, or `2 · resize` outside the main mode, or `–` before the first workspace.
    pub fn title(&self) -> String {
        let workspace = self.workspace.as_deref().unwrap_or("\u{2013}");
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

    #[test]
    fn focus_events_do_not_change_the_title() {
        let mut s = IndicatorState::default();
        s.apply(&ws("2"));
        s.apply(&Event::FocusChanged { window: Some(1) });
        assert_eq!(s.title(), "2");
    }
}
