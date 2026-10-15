//! Applying the computed tiling layout to real windows.
//!
//! [`Reactor::flush_layout`] runs after every event (and after [`Reactor::flush_visibility`],
//! so parking and unparking are already out). It computes [`LayoutTarget`]s for every
//! visible workspace and hands the ones that changed to the presenter through
//! [`AnimationManager::animate_layout`], one call per display.
//!
//! Feedback loops are avoided by comparing against what dowm itself last applied, never
//! against what the app reports: a window that cannot reach its target (minimum size,
//! user drag) is not rewritten until its target changes. Echoes of our own writes arrive
//! as `WindowFrameChanged(.., Requested(true))` and only update the window's record.
//! Windows that are parked, or on a hidden workspace, have no target; their entry is
//! forgotten, so they are laid out afresh when they return.

use objc2_core_foundation::CGRect;

use super::animation::AnimationManager;
use super::{LayoutTarget, Reactor};
use crate::actor::app::WindowId;
use crate::common::collections::HashSet;
use crate::sys::screen::DisplayId;

/// What the layout last asked of a window.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct AppliedTarget {
    frame: CGRect,
    raise: bool,
}

impl Reactor {
    /// The targets of every visible workspace, grouped by the display they are shown on.
    pub(super) fn layout_batches(&self) -> Vec<(DisplayId, Vec<LayoutTarget>)> {
        let mut out = Vec::new();
        for id in self.workspaces.ids() {
            let Some(display) = self.workspaces.display_of(id).and_then(|d| self.displays.get(d)) else {
                continue;
            };
            let Some(computed) = self.workspace_layout(id) else { continue };
            let targets = computed
                .windows
                .iter()
                .map(|(&window, &frame)| LayoutTarget {
                    window,
                    frame: super::from_layout_rect(frame),
                    overlapping: computed.overlapping.contains(&window),
                    raise: computed.raise.contains(&window),
                })
                .collect();
            out.push((display.cg_id, targets));
        }
        out
    }

    /// Send the layout changes since the last call to the presenter: animated frame
    /// writes per display, then raises for the front window of overlapping layouts.
    ///
    /// Does nothing while the parked windows are being restored at shutdown.
    pub fn flush_layout(&mut self) {
        if self.parked_registry.is_shutting_down() {
            return;
        }
        let mut batches = self.layout_batches();
        for (_, targets) in &mut batches {
            targets.retain(|t| {
                self.windows.get(t.window).is_some_and(|r| !r.is_minimized)
                    && !self.visibility.is_parked(t.window)
                    && !self.foreign.contains(&t.window)
            });
        }
        let live: HashSet<WindowId> =
            batches.iter().flat_map(|(_, ts)| ts.iter().map(|t| t.window)).collect();
        self.applied_layout.retain(|w, _| live.contains(w));

        let mut raises = Vec::new();
        for (display, targets) in batches {
            let changed: Vec<&LayoutTarget> = targets
                .iter()
                .filter(|t| {
                    self.applied_layout.get(&t.window)
                        != Some(&AppliedTarget { frame: t.frame, raise: t.raise })
                })
                .collect();
            if changed.is_empty() {
                continue;
            }
            for t in &changed {
                let was_raised = self.applied_layout.get(&t.window).is_some_and(|a| a.raise);
                if t.raise && t.overlapping && !was_raised && self.focused_window != Some(t.window) {
                    raises.push(t.window);
                }
            }
            let layout: Vec<(WindowId, CGRect)> = changed.iter().map(|t| (t.window, t.frame)).collect();
            AnimationManager::animate_layout(self, display, &layout, false, None);
            for t in &targets {
                self.applied_layout.insert(t.window, AppliedTarget { frame: t.frame, raise: t.raise });
            }
            // A window just unhidden is confirmed by the answer to its newest write.
            let now = self.clock.0.now();
            for t in &targets {
                if !self.visibility.is_unconfirmed(t.window) {
                    continue;
                }
                let txid = self
                    .windows
                    .get(t.window)
                    .and_then(|r| r.sys_id)
                    .map(|wsid| self.presenter.transactions.get_last_sent_txid(wsid));
                self.visibility.arm(t.window, txid, now);
            }
        }
        for window in raises {
            self.request_raise(window);
        }
    }
}
