//! The presenter: interpolates window frames toward layout targets on its own thread,
//! paced per display by `CADisplayLink`.
//!
//! Ported from rift's `actor/reactor/animation.rs` (Apache-2.0, see `NOTICE`).
//! Differences from upstream:
//! - Every presenter key is a [`DisplayId`]. rift grouped its viewport cameras by
//!   native `SpaceId`; dowm uses no native Spaces.
//! - rift's viewport-camera motion is not ported. It exists only for rift's scrolling
//!   layout and trackpad gestures, which dowm v1 does not have. [`Motion`] keeps the
//!   enum seam where it would return.
//! - The layout entry points take a [`PresenterHost`] instead of rift's `Reactor`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use parking_lot::Mutex;
use tracing::{debug, trace};

use super::transaction_manager::{TransactionId, TransactionManager};
use crate::actor::app::{AppThreadHandle, FrameMode, FrameSource, Request, WindowId, pid_t};
use crate::common::collections::{HashMap, HashSet};
use crate::sys::display_link::DisplayLink;
use crate::sys::geometry::{Round, SameAs};
use crate::sys::power;
use crate::sys::screen::DisplayId;
use crate::sys::window_server::WindowServerId;

#[derive(Debug)]
pub struct AnimationSender {
    tx: Sender<Message>,
    control: Arc<Mutex<PresenterControl>>,
}
impl AnimationSender {
    pub fn channel() -> (Self, AnimationReceiver) {
        let (tx, commands) = crossbeam_channel::unbounded();
        let control = Arc::default();
        (
            Self {
                tx,
                control: Arc::clone(&control),
            },
            AnimationReceiver { commands, control },
        )
    }

    pub fn send(&self, message: Message) -> Result<(), crossbeam_channel::SendError<Message>> {
        self.tx.send(message)
    }

    pub fn cancel(&self, windows: Vec<WindowId>) -> Vec<(WindowId, CGRect)> {
        let mut control = self.control.lock();
        let frames = windows
            .iter()
            .filter_map(|wid| {
                let (handle, frame) = control.frames.remove(wid)?;
                handle.cancel_window_animation(*wid);
                Some((*wid, frame))
            })
            .collect();
        control.cancelled.extend(windows.iter().copied());
        let _ = self.send(Message::Stop(windows));
        frames
    }
}

#[derive(Debug, Default)]
struct PresenterControl {
    frames: HashMap<WindowId, (AppThreadHandle, CGRect)>,
    cancelled: HashSet<WindowId>,
}

pub struct AnimationReceiver {
    pub(super) commands: Receiver<Message>,
    control: Arc<Mutex<PresenterControl>>,
}

#[derive(Debug)]
pub enum Message {
    Replace(Animation),
    SkipToEnd(Animation),
    Stop(Vec<WindowId>),
    /// The displays that currently exist; presenters for any other display retire.
    Displays(Vec<DisplayId>),
}

#[derive(Debug, Default)]
pub struct AnimationManager {
    motions: Vec<Motion>,
    frames: FrameBatch,
}

/// Display scheduling, cancellation and retirement are shared by every motion
/// policy. rift also has a viewport-camera variant for its scrolling layout; it
/// returns here if dowm gains one.
#[derive(Debug)]
enum Motion {
    Transition(ActiveAnimation),
}

type Frame = (WindowId, CGRect, bool, TransactionId, FrameSource);

/// Reusable presenter-local batches: all motions on a display publish through
/// one lock/wake per application, and terminal leases follow that publication.
#[derive(Debug, Default)]
struct FrameBatch(HashMap<pid_t, (AppThreadHandle, Vec<Frame>)>);
impl FrameBatch {
    fn extend(
        &mut self,
        handle: &AppThreadHandle,
        pid: pid_t,
        frames: impl Iterator<Item = Frame>,
    ) {
        let (actor, pending) = self.0.entry(pid).or_insert_with(|| (handle.clone(), Vec::new()));
        if !actor.same_actor(handle) {
            *actor = handle.clone();
        }
        pending.extend(frames);
    }

    fn flush(&mut self) {
        for (handle, frames) in self.0.values_mut() {
            if !frames.is_empty() {
                handle.send_interactive_frames(frames.drain(..));
            }
        }
    }
}

struct DisplayPresenter {
    manager: AnimationManager,
    link: DisplayLink,
    fallback_deadline: Instant,
}

#[derive(Debug)]
struct ActiveAnimation {
    animation: Animation,
    started: Instant,
    progress: f64,
    next_sample: Instant,
    ended: bool,
}

#[derive(Debug)]
pub struct Animation {
    interval: Duration,
    duration: Duration,
    display: DisplayId,
    windows: Vec<PresentedWindow>,
    handled_windows: Vec<WindowId>,
}

#[derive(Debug)]
struct PresentedWindow {
    handle: AppThreadHandle,
    wid: WindowId,
    from: CGRect,
    to: CGRect,
    /// The last publication.
    frame: CGRect,
    txid: TransactionId,
    leased: bool,
}

#[derive(Clone, Copy)]
struct Sample {
    progress: f64,
    eased: f64,
    set_size: bool,
}

impl PresentedWindow {
    fn begin(&mut self) {
        if !std::mem::replace(&mut self.leased, true) {
            let _ = self.handle.send(Request::BeginWindowAnimation(self.wid));
        }
    }

    fn end(&mut self) {
        if std::mem::replace(&mut self.leased, false) {
            let _ = self.handle.send(Request::EndWindowAnimation(self.wid));
        }
    }

    fn cancel(&mut self) {
        self.leased = false;
        self.handle.cancel_window_animation(self.wid);
    }

    fn sample(&mut self, Sample { progress, eased, set_size }: Sample) -> Option<Frame> {
        let mut frame = interpolate_frame(self.from, self.to, eased);
        frame.size = if progress >= 0.5 {
            self.to.size
        } else {
            self.from.size
        };
        // Ordinary frames preserve subpixel easing.
        let changed = frame != self.frame || set_size;
        self.frame = frame;
        changed.then_some((self.wid, frame, set_size, self.txid, FrameSource::Ordinary))
    }
}

fn stage_windows(windows: &mut [PresentedWindow], sample: Sample, frames: &mut FrameBatch) {
    for group in windows.chunk_by_mut(|a, b| a.wid.pid == b.wid.pid) {
        let handle = group[0].handle.clone();
        frames.extend(
            &handle,
            group[0].wid.pid,
            group.iter_mut().filter_map(|window| window.sample(sample)),
        );
    }
}

/// Animation timing. Defaults match rift's.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AnimationSettings {
    pub animate: bool,
    pub fps: f64,
    /// Seconds.
    pub duration: f64,
}

impl Default for AnimationSettings {
    fn default() -> Self {
        Self {
            animate: true,
            fps: 100.0,
            duration: 0.3,
        }
    }
}

/// The reactor's record of a window, as the presenter needs it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HostWindow {
    /// The last frame dowm sent or observed (rift's `frame_monotonic`).
    pub frame: CGRect,
    pub wsid: Option<WindowServerId>,
}

/// What the layout entry points read from and write to the reactor.
///
/// rift implements these entry points directly on its `Reactor`. dowm's reactor
/// implements this trait instead, which also lets the entry points be tested
/// without one.
pub trait PresenterHost {
    /// The presenter thread's sender. `None` applies every layout immediately.
    fn animation_tx(&self) -> Option<&AnimationSender>;
    fn transaction_manager(&self) -> &TransactionManager;
    fn animation_settings(&self) -> AnimationSettings;
    fn window(&self, wid: WindowId) -> Option<HostWindow>;
    fn set_window_frame(&mut self, wid: WindowId, frame: CGRect);
    fn app_handle(&self, pid: pid_t) -> Option<AppThreadHandle>;
    /// Whether `wid` belongs to the workspace currently shown on `display`.
    /// Windows that are not are positioned directly instead of animated.
    fn is_visible_on(&self, display: DisplayId, wid: WindowId) -> bool;
}

/// Stops in-flight presentation of `windows`, adopting each one's last published
/// frame as its current frame.
fn cancel_window_presentations(host: &mut impl PresenterHost, mut windows: Vec<WindowId>) {
    if windows.is_empty() {
        return;
    }
    windows.sort_unstable();
    let Some(tx) = host.animation_tx() else {
        return;
    };
    for (wid, frame) in tx.cancel(windows) {
        if let Some(window) = host.window(wid) {
            host.set_window_frame(wid, frame);
            if let Some(wsid) = window.wsid {
                host.transaction_manager().clear_target_for_window(wsid);
            }
        }
    }
}

impl AnimationManager {
    pub fn new() -> Self { Self::default() }

    /// One runner for all displays. Commands are semantic work; display wakes
    /// are bounded to one permit and each link retains only its newest timing.
    pub fn run(receiver: AnimationReceiver) {
        let AnimationReceiver { commands: rx, control } = receiver;
        let (wake, ticks) = crossbeam_channel::bounded(1);
        let mut displays: HashMap<DisplayId, DisplayPresenter> = HashMap::default();
        loop {
            let deadline = displays
                .values()
                .filter(|d| d.manager.has_work())
                .map(|d| d.fallback_deadline)
                .min();
            let timeout =
                deadline.map_or(Duration::MAX, |t| t.saturating_duration_since(Instant::now()));
            crossbeam_channel::select! {
                recv(rx) -> message => {
                    let Ok(message) = message else { break };
                    match message {
                        Message::Displays(known) => {
                            displays.retain(|display, presenter| {
                                if known.contains(display) { return true; }
                                presenter.manager.stop_all();
                                false
                            });
                        }
                        Message::Stop(mut windows) => {
                            windows.sort_unstable();
                            for d in displays.values_mut() { d.manager.stop_windows(&windows); d.manager.retire(); }
                            let mut shared = control.lock();
                            for wid in &windows { shared.cancelled.remove(wid); shared.frames.remove(wid); }
                        }
                        mut message @ (Message::Replace(_) | Message::SkipToEnd(_)) => {
                            let (Message::Replace(animation) | Message::SkipToEnd(animation)) = &mut message else {
                                unreachable!()
                            };
                            let display = animation.display;
                            let entry = displays.entry(display).or_insert_with(|| DisplayPresenter {
                                manager: Self::new(), link: DisplayLink::for_display(display, wake.clone()),
                                fallback_deadline: Instant::now(),
                            });
                            let mut shared = control.lock();
                            let mut cancelled: Vec<_> = shared.cancelled.iter().copied().collect();
                            cancelled.sort_unstable();
                            entry.manager.stop_windows(&cancelled);
                            entry.manager.retire();
                            // A command queued before cancellation cannot resurrect its
                            // windows or replay an obsolete instant-layout target.
                            animation.windows.retain(|w| !shared.cancelled.contains(&w.wid));
                            entry.manager.handle_message(message);
                            entry.manager.publish_frames(&mut shared, true);
                            entry.manager.retire();
                            entry.link.set_paused(!entry.manager.has_work());
                            entry.fallback_deadline = Instant::now();
                        }
                    }
                }
                recv(ticks) -> _ => {}
                default(timeout) => {}
            }
            if crate::termination::process_shutting_down() {
                // The restore has begun: frames still in flight would park windows again.
                for d in displays.values_mut() {
                    d.manager.stop_all();
                }
                break;
            }
            let now = Instant::now();
            let mut shared = control.lock();
            let cancelled: Vec<_> = {
                let mut w: Vec<_> = shared.cancelled.iter().copied().collect();
                w.sort_unstable();
                w
            };
            for DisplayPresenter {
                manager,
                link,
                fallback_deadline: deadline,
            } in displays.values_mut()
            {
                manager.stop_windows(&cancelled);
                if !manager.has_work() {
                    manager.retire();
                    link.set_paused(true);
                    continue;
                }
                let native = link.latest();
                let sample =
                    native.map(|(_, target)| target).or_else(|| (now >= *deadline).then_some(now));
                if let Some(sample) = sample {
                    let delay =
                        manager.tick_at(sample).unwrap_or(Duration::from_secs_f64(1.0 / 60.0));
                    // Resume fallback after three missing native refreshes.
                    *deadline = if let Some((tick, _)) = native {
                        sample
                            + Duration::try_from_secs_f64(
                                3.0 * (tick.target_timestamp - tick.timestamp),
                            )
                            .ok()
                            .filter(|d| !d.is_zero())
                            .unwrap_or(delay)
                            .min(Duration::from_millis(250))
                    } else {
                        now + delay
                    };
                }
                manager.publish_frames(&mut shared, false);
                manager.retire();
            }
            for d in displays.values() {
                d.link.set_paused(!d.manager.has_work());
            }
        }
        for d in displays.values_mut() {
            for motion in &mut d.manager.motions {
                match motion {
                    Motion::Transition(a) => a.animation.finish_all(),
                }
            }
        }
    }

    fn retire(&mut self) { self.motions.retain(Motion::has_work); }

    fn stop_all(&mut self) {
        for motion in &mut self.motions {
            motion.stop();
        }
        self.retire();
    }

    fn publish_frames(&self, shared: &mut PresenterControl, initialize: bool) {
        for w in self.motions.iter().flat_map(Motion::windows) {
            let (handle, frame) =
                shared.frames.entry(w.wid).or_insert_with(|| (w.handle.clone(), w.frame));
            *frame = w.frame;
            if initialize {
                *handle = w.handle.clone();
            }
        }
    }

    fn has_work(&self) -> bool { self.motions.iter().any(Motion::has_work) }

    fn stop_windows(&mut self, windows: &[WindowId]) {
        if !windows.is_empty() {
            for motion in &mut self.motions {
                motion.stop_windows(windows);
            }
        }
    }

    pub fn handle_message(&mut self, message: Message) {
        self.retire();
        match message {
            Message::Stop(mut windows) => {
                windows.sort_unstable();
                self.stop_windows(&windows);
            }
            Message::Replace(animation) => {
                let old = self
                    .motions
                    .iter()
                    .position(|m| matches!(m, Motion::Transition(_)))
                    .map(|idx| self.motions.swap_remove(idx));
                let next = match old {
                    Some(Motion::Transition(active)) => Some(active.replace_with(animation)),
                    None => ActiveAnimation::start(animation),
                };
                if let Some(next) = next {
                    self.motions.push(Motion::Transition(next));
                }
            }
            Message::SkipToEnd(animation) => {
                self.motions.retain_mut(|m| match m {
                    Motion::Transition(a) => {
                        a.animation.finish_all();
                        false
                    }
                });
                animation.skip_to_end();
            }
            Message::Displays(_) => unreachable!("runner handles display control"),
        }
    }

    pub fn tick_at(&mut self, now: Instant) -> Option<Duration> {
        let delay = self
            .motions
            .iter_mut()
            .filter_map(|motion| motion.sample(now, &mut self.frames))
            .min();
        self.frames.flush();
        for motion in &mut self.motions {
            motion.finish_frame();
        }
        if !self.has_work() {
            self.frames.0.clear();
        }
        delay
    }

    /// Moves `layout`'s windows to their targets. Windows on the workspace shown on
    /// `display` animate there; the rest are positioned directly. Returns whether any
    /// frame changed.
    pub fn animate_layout(
        host: &mut impl PresenterHost,
        display: DisplayId,
        layout: &[(WindowId, CGRect)],
        is_resize: bool,
        skip_wid: Option<WindowId>,
    ) -> bool {
        let settings = host.animation_settings();
        let animate = !is_resize && settings.animate && !power::is_low_power_mode_enabled();
        if !animate {
            cancel_window_presentations(host, layout.iter().map(|(wid, _)| *wid).collect());
        }
        let mut anim = Animation::new(settings.fps, settings.duration, display);
        let mut any_frame_changed = false;

        for &(wid, target_frame) in layout {
            if skip_wid == Some(wid) {
                anim.mark_handled(wid);
                trace!(
                    ?wid,
                    "Skipping animated layout update for window currently being dragged"
                );
                continue;
            }

            let target_frame = target_frame.round();
            let Some(window) = host.window(wid) else {
                continue;
            };
            let current_frame = window.frame;
            let window_server_id = window.wsid;
            let pending = window_server_id
                .and_then(|wsid| host.transaction_manager().get_target_frame(wsid));
            // Matching the observed frame cannot cancel an older, different target.
            if pending.is_some_and(|frame| frame.same_as(target_frame))
                || (pending.is_none() && target_frame.same_as(current_frame))
            {
                continue;
            }
            any_frame_changed = true;
            let txid = window_server_id
                .map(|wsid| host.transaction_manager().generate_next_txid(wsid))
                .unwrap_or_default();

            let Some(handle) = host.app_handle(wid.pid) else {
                debug!(?wid, "Skipping for window - app no longer exists");
                continue;
            };

            if let Some(wsid) = window_server_id {
                host.transaction_manager().update_txid_entries([(wsid, txid, target_frame)]);
            }
            if host.is_visible_on(display, wid) {
                trace!(?wid, ?current_frame, ?target_frame, "Animating visible window");
                anim.add_window(&handle, wid, current_frame, target_frame, txid);
            } else {
                anim.mark_handled(wid);
                trace!(
                    ?wid,
                    ?current_frame,
                    ?target_frame,
                    "Direct positioning hidden window"
                );
                if let Err(e) =
                    handle.send(Request::set_window_frame(wid, target_frame, txid, true))
                {
                    debug!(?wid, ?e, "Failed to send frame request for hidden window");
                    continue;
                }
            }

            host.set_window_frame(wid, target_frame);
        }

        if !anim.is_empty() {
            if let Some(tx) = host.animation_tx() {
                let message = if !animate {
                    Message::SkipToEnd(anim)
                } else {
                    Message::Replace(anim)
                };
                if let Err(err) = tx.send(message) {
                    match err.0 {
                        Message::Replace(animation) | Message::SkipToEnd(animation) => {
                            animation.skip_to_end()
                        }
                        Message::Stop(..) | Message::Displays(_) => {}
                    }
                }
            } else {
                anim.skip_to_end();
            }
        }

        any_frame_changed
    }

    /// Applies `layout` immediately, cancelling any in-flight animation of its windows.
    pub fn instant_layout(
        host: &mut impl PresenterHost,
        layout: &[(WindowId, CGRect)],
        skip_wid: Option<WindowId>,
    ) -> bool {
        Self::instant_layout_inner(host, layout, skip_wid, false)
    }

    /// Apply the position-only layout used while switching workspaces.
    ///
    /// Keep this entry point separate from `instant_layout`: layouts merely suppressed
    /// while a switch is in progress may still change window sizes and must use the
    /// full-frame request.
    pub fn workspace_switch_layout(
        host: &mut impl PresenterHost,
        layout: &[(WindowId, CGRect)],
        skip_wid: Option<WindowId>,
    ) -> bool {
        Self::instant_layout_inner(host, layout, skip_wid, true)
    }

    fn instant_layout_inner(
        host: &mut impl PresenterHost,
        layout: &[(WindowId, CGRect)],
        skip_wid: Option<WindowId>,
        position_only: bool,
    ) -> bool {
        cancel_window_presentations(host, layout.iter().map(|(wid, _)| *wid).collect());
        let mut per_app: HashMap<pid_t, _> = HashMap::default();
        let mut any_frame_changed = false;

        for &(wid, target_frame) in layout {
            if skip_wid == Some(wid) {
                trace!(?wid, "Skipping layout update for window currently being dragged");
                continue;
            }

            let Some(window) = host.window(wid) else {
                debug!(?wid, "Skipping layout - window no longer exists");
                continue;
            };
            let target_frame = target_frame.round();
            let current_frame = window.frame;
            if target_frame.same_as(current_frame) {
                continue;
            }
            if let Some(wsid) = window.wsid
                && host
                    .transaction_manager()
                    .get_target_frame(wsid)
                    .is_some_and(|pending| pending.same_as(target_frame))
            {
                trace!(?wid, ?target_frame, "Skipping redundant instant layout request");
                continue;
            }
            any_frame_changed = true;
            trace!(
                ?wid,
                ?current_frame,
                ?target_frame,
                "Instant workspace positioning"
            );

            let size_unchanged = current_frame.size.same_as(target_frame.size);
            host.set_window_frame(wid, target_frame);
            let (frames, positions, first_wsid) =
                per_app.entry(wid.pid).or_insert_with(|| (Vec::new(), Vec::new(), None));
            *first_wsid = first_wsid.or(window.wsid);
            if position_only && size_unchanged {
                positions.push((wid, target_frame));
            } else {
                frames.push((wid, target_frame));
            }
        }

        for (pid, (frames, positions, first_wsid)) in per_app {
            let Some(handle) = host.app_handle(pid) else {
                debug!(?pid, "Skipping layout update for app - app no longer exists");
                continue;
            };

            let tm = host.transaction_manager();
            let txid = first_wsid.map(|wsid| tm.generate_next_txid(wsid)).unwrap_or_default();
            // rift stores the window's live frame here; the per-entry target is the same
            // value unless `layout` lists a window twice.
            for (wid, target) in frames.iter().chain(positions.iter()) {
                if let Some(wsid) = host.window(*wid).and_then(|w| w.wsid) {
                    tm.store_txid(wsid, txid, *target);
                }
            }
            let requests = [
                (!positions.is_empty())
                    .then(|| Request::SetWindowFrames(positions, txid, FrameMode::Position, true)),
                (!frames.is_empty())
                    .then(|| Request::SetWindowFrames(frames, txid, FrameMode::Full, true)),
            ];
            for request in requests.into_iter().flatten() {
                if let Err(e) = handle.send(request) {
                    debug!(
                        ?pid,
                        ?e,
                        "Failed to send instant layout request - app may have quit"
                    );
                    break;
                }
            }
        }

        any_frame_changed
    }
}

impl Motion {
    fn windows(&self) -> &[PresentedWindow] {
        match self {
            Self::Transition(a) => &a.animation.windows,
        }
    }

    fn windows_mut(&mut self) -> &mut Vec<PresentedWindow> {
        match self {
            Self::Transition(a) => &mut a.animation.windows,
        }
    }

    fn stop_windows(&mut self, windows: &[WindowId]) {
        self.windows_mut().retain_mut(|window| {
            if windows.binary_search(&window.wid).is_ok() {
                window.cancel();
                false
            } else {
                true
            }
        });
    }

    fn stop(&mut self) {
        for window in self.windows_mut().iter_mut() {
            window.cancel();
        }
        self.windows_mut().clear();
    }

    fn has_work(&self) -> bool {
        match self {
            Self::Transition(a) => !a.animation.is_empty() && !a.is_complete(),
        }
    }

    fn sample(&mut self, now: Instant, frames: &mut FrameBatch) -> Option<Duration> {
        match self {
            Self::Transition(a) => {
                if a.animation.is_empty() || a.is_complete() {
                    return None;
                }
                if now + Duration::from_micros(1) < a.next_sample
                    && now.saturating_duration_since(a.started) < a.animation.duration
                {
                    return Some(a.next_sample - now);
                }
                let missed = now.saturating_duration_since(a.next_sample).as_nanos()
                    / a.animation.interval.as_nanos();
                a.next_sample += a.animation.interval.mul_f64((missed + 1) as f64);
                a.send_frame(now, frames);
                (!a.is_complete()).then_some(a.animation.interval)
            }
        }
    }

    fn finish_frame(&mut self) {
        match self {
            Self::Transition(a) if a.is_complete() && !a.ended => {
                a.animation.end();
                a.ended = true;
            }
            Self::Transition(_) => {}
        }
    }
}

impl ActiveAnimation {
    fn start(mut animation: Animation) -> Option<Self> {
        if animation.is_empty() {
            return None;
        }
        animation.windows.sort_unstable_by_key(|w| w.wid.pid);
        animation.begin();
        let started = Instant::now();
        let next_sample = started + animation.interval;
        Some(Self {
            animation,
            started,
            progress: 0.0,
            next_sample,
            ended: false,
        })
    }

    fn replace_with(self, mut next: Animation) -> Self {
        for window in &mut next.windows {
            if let Some(old) = self.animation.windows.iter().find(|old| old.wid == window.wid) {
                window.from = old.frame;
                window.leased = old.leased;
            } else {
                window.begin();
            }
        }
        for mut old in self.animation.windows {
            if !next.handled_windows.contains(&old.wid) {
                old.from = old.frame;
                next.windows.push(old);
            } else if !next.windows.iter().any(|w| w.wid == old.wid) {
                old.cancel();
            }
        }
        next.windows.sort_unstable_by_key(|w| w.wid.pid);
        let started = Instant::now();
        let next_sample = started + next.interval;
        Self {
            animation: next,
            started,
            progress: 0.0,
            next_sample,
            ended: false,
        }
    }

    fn send_frame(&mut self, now: Instant, frames: &mut FrameBatch) {
        let t = if self.animation.duration.is_zero() {
            1.0
        } else {
            (now.saturating_duration_since(self.started).as_secs_f64()
                / self.animation.duration.as_secs_f64())
            .clamp(0.0, 1.0)
        };
        let t = t.max(self.progress);
        self.animation.stage_frame(t, self.progress, frames);
        self.progress = t;
    }

    fn is_complete(&self) -> bool { self.progress >= 1.0 }
}

impl Animation {
    fn new(fps: f64, duration: f64, display: DisplayId) -> Self {
        let interval = Duration::from_secs_f64(1.0 / fps);
        Self {
            interval,
            duration: Duration::from_secs_f64(duration),
            display,
            windows: vec![],
            handled_windows: vec![],
        }
    }

    fn add_window(
        &mut self,
        handle: &AppThreadHandle,
        wid: WindowId,
        start: CGRect,
        finish: CGRect,
        txid: TransactionId,
    ) {
        self.windows.push(PresentedWindow {
            handle: handle.clone(),
            wid,
            from: start,
            to: finish,
            frame: start,
            txid,
            leased: false,
        });
        self.mark_handled(wid);
    }

    fn mark_handled(&mut self, wid: WindowId) {
        if !self.handled_windows.contains(&wid) {
            self.handled_windows.push(wid);
        }
    }

    pub fn skip_to_end(&self) {
        for window in &self.windows {
            _ = window.handle.send(Request::set_window_frame(
                window.wid,
                window.to,
                window.txid,
                true,
            ));
        }
    }

    fn is_empty(&self) -> bool { self.windows.is_empty() }

    fn begin(&mut self) {
        for window in &mut self.windows {
            window.begin();
        }
    }

    fn finish_all(&mut self) {
        let mut frames = FrameBatch::default();
        self.stage_frame(1.0, 0.0, &mut frames);
        frames.flush();
        self.end();
    }

    fn stage_frame(&mut self, t: f64, previous: f64, frames: &mut FrameBatch) {
        stage_windows(
            &mut self.windows,
            Sample {
                progress: t,
                eased: ease(t),
                set_size: (previous < 0.5 && t >= 0.5) || t == 1.0,
            },
            frames,
        );
    }

    fn end(&mut self) {
        for window in &mut self.windows {
            window.end();
        }
    }
}

#[cfg(test)]
fn get_frame(a: CGRect, b: CGRect, t: f64) -> CGRect { interpolate_frame(a, b, ease(t)) }

fn interpolate_frame(a: CGRect, b: CGRect, s: f64) -> CGRect {
    CGRect {
        origin: CGPoint {
            x: blend(a.origin.x, b.origin.x, s),
            y: blend(a.origin.y, b.origin.y, s),
        },
        size: CGSize {
            width: blend(a.size.width, b.size.width, s),
            height: blend(a.size.height, b.size.height, s),
        },
    }
}

fn ease(t: f64) -> f64 {
    if t < 0.5 {
        (1.0 - f64::sqrt(1.0 - f64::powi(2.0 * t, 2))) / 2.0
    } else {
        (f64::sqrt(1.0 - f64::powi(-2.0 * t + 2.0, 2)) + 1.0) / 2.0
    }
}

fn blend(a: f64, b: f64, s: f64) -> f64 { (1.0 - s) * a + s * b }

#[cfg(test)]
mod tests {
    use objc2_core_foundation::{CGPoint, CGSize};

    use super::*;
    use crate::model::tx_store::WindowTxStore;

    const DISPLAY: DisplayId = DisplayId::new(1);

    fn rect(origin_x: f64, origin_y: f64, width: f64, height: f64) -> CGRect {
        CGRect::new(CGPoint::new(origin_x, origin_y), CGSize::new(width, height))
    }

    fn empty_animation() -> Animation {
        let settings = AnimationSettings::default();
        Animation::new(settings.fps, settings.duration, DISPLAY)
    }

    fn animation(handle: &AppThreadHandle, wid: WindowId, from: CGRect, to: CGRect) -> Animation {
        let mut animation = empty_animation();
        animation.add_window(handle, wid, from, to, TransactionId::default());
        animation
    }

    fn transition_state(manager: &AnimationManager) -> Option<&ActiveAnimation> {
        manager.motions.iter().find_map(|m| match m {
            Motion::Transition(a) if !a.is_complete() && !a.animation.is_empty() => Some(a),
            Motion::Transition(_) => None,
        })
    }
    fn active_mut(manager: &mut AnimationManager) -> Option<&mut ActiveAnimation> {
        manager.motions.iter_mut().map(|Motion::Transition(a)| a).next()
    }

    #[derive(Debug)]
    enum Observed {
        Frame {
            wid: WindowId,
            frame: CGRect,
            set_size: bool,
            txid: TransactionId,
        },
        Request(Request),
    }
    fn collect_requests(rx: &mut crate::actor::Receiver<Request>) -> Vec<Observed> {
        let mut requests = Vec::new();
        while let Ok((_, request)) = rx.try_recv() {
            if let Request::InteractiveFramesPending(queue) = request {
                queue.drain_with(|wid, frame, set_size, txid, _, _| {
                    requests.push(Observed::Frame { wid, frame, set_size, txid })
                });
            } else {
                requests.push(Observed::Request(request));
            }
        }
        requests
    }

    fn assert_set_window_frame(request: &Observed, wid: WindowId, frame: CGRect) {
        match request {
            Observed::Request(Request::SetWindowFrames(frames, txid, FrameMode::Full, eui)) => {
                let (req_wid, req_frame) = frames[0];
                assert_eq!(req_wid, wid);
                assert_eq!(req_frame, frame);
                assert_eq!(*txid, TransactionId::default());
                assert!(*eui);
            }
            _ => panic!("expected SetWindowFrame, got {request:?}"),
        }
    }

    fn assert_animation_frame(request: &Observed, wid: WindowId, frame: CGRect) {
        match request {
            Observed::Frame {
                wid: req_wid,
                frame: req_frame,
                set_size,
                txid,
            } => {
                assert_eq!(*req_wid, wid);
                assert_eq!(*req_frame, frame);
                assert!(*set_size, "expected a set_size frame");
                assert_eq!(*txid, TransactionId::default());
            }
            _ => panic!("expected coalesced frame, got {request:?}"),
        }
    }

    fn assert_animation_pos(request: &Observed, wid: WindowId, pos: CGPoint) {
        match request {
            Observed::Frame {
                wid: req_wid,
                frame,
                set_size,
                txid,
            } => {
                assert_eq!(*req_wid, wid);
                assert_eq!(frame.origin, pos);
                assert!(!*set_size, "expected a position-only frame");
                assert_eq!(*txid, TransactionId::default());
            }
            _ => panic!("expected coalesced frame, got {request:?}"),
        }
    }

    #[test]
    fn native_and_fallback_sampling_use_elapsed_time_and_finish_exactly() {
        let start = Instant::now();
        let mut results = Vec::new();
        for fps in [60.0, 120.0] {
            let (tx, mut rx) = crate::actor::channel();
            let handle = AppThreadHandle::new_for_test(tx);
            let mut a = Animation::new(fps, 0.3, DISPLAY);
            a.add_window(
                &handle,
                WindowId::new(1, 1),
                rect(0.0, 0.0, 10.0, 10.0),
                rect(100.0, 50.0, 20.0, 30.0),
                TransactionId::default(),
            );
            let mut manager = AnimationManager::new();
            manager.handle_message(Message::Replace(a));
            active_mut(&mut manager).unwrap().started = start;
            collect_requests(&mut rx);
            let mut frames = Vec::new();
            for ms in [40, 150, 300] {
                manager.tick_at(start + Duration::from_millis(ms));
                frames.extend(collect_requests(&mut rx).into_iter().filter_map(|r| match r {
                    Observed::Frame { frame, .. } => Some(frame),
                    _ => None,
                }));
                // A duplicate wake must not advance time or replay a frame.
                manager.tick_at(start + Duration::from_millis(ms));
                assert!(collect_requests(&mut rx).is_empty());
            }
            assert_eq!(frames.len(), 3);
            assert_eq!(frames[1].origin, CGPoint::new(50.0, 25.0));
            assert_eq!(frames[2], rect(100.0, 50.0, 20.0, 30.0));
            assert!(transition_state(&manager).is_none());
            results.push(frames);
        }
        assert_eq!(results[0], results[1]);
    }

    #[test]
    fn native_ticks_gate_generic_fps_and_skip_missed_intervals() {
        let start = Instant::now();
        for (fps, expected) in [(60.0, 60), (30.0, 30)] {
            let (handle, mut rx) = AppThreadHandle::channel();
            let mut a = Animation::new(fps, 1.0, DISPLAY);
            let wid = WindowId::new(1, 1);
            a.add_window(
                &handle,
                wid,
                rect(0.0, 0.0, 10.0, 10.0),
                rect(1000.0, 0.0, 10.0, 10.0),
                TransactionId::default(),
            );
            let mut manager = AnimationManager::new();
            manager.handle_message(Message::Replace(a));
            let active = active_mut(&mut manager).unwrap();
            active.started = start;
            active.next_sample = start + active.animation.interval;
            collect_requests(&mut rx);
            let mut count = 0;
            for tick in 1..=120 {
                manager.tick_at(start + Duration::from_secs_f64(tick as f64 / 120.0));
                count += collect_requests(&mut rx)
                    .iter()
                    .filter(|r| matches!(r, Observed::Frame { .. }))
                    .count();
            }
            assert_eq!(count, expected);
            assert!(!manager.has_work());
        }
        let (handle, mut rx) = AppThreadHandle::channel();
        let wid = WindowId::new(1, 1);
        let mut a = animation(
            &handle,
            wid,
            rect(0.0, 0.0, 10.0, 10.0),
            rect(1000.0, 0.0, 10.0, 10.0),
        );
        a.interval = Duration::from_millis(20);
        a.duration = Duration::from_secs(1);
        let mut manager = AnimationManager::new();
        manager.handle_message(Message::Replace(a));
        let active = active_mut(&mut manager).unwrap();
        active.started = start;
        active.next_sample = start;
        collect_requests(&mut rx);
        manager.tick_at(start + Duration::from_millis(200));
        let frames = collect_requests(&mut rx);
        assert_eq!(frames.len(), 1, "missed frames must not replay");
        let Observed::Frame { frame: last, .. } = frames[0] else {
            panic!("frame");
        };
        manager.handle_message(Message::Replace(animation(
            &handle,
            wid,
            last,
            rect(2000.0, 0.0, 10.0, 10.0),
        )));
        assert_eq!(
            transition_state(&manager).unwrap().animation.windows[0].from,
            last
        );
        manager.tick_at(transition_state(&manager).unwrap().next_sample - Duration::from_millis(1));
        assert!(
            collect_requests(&mut rx).is_empty(),
            "old timestamps cannot sample a new transition"
        );
    }

    /// Each display gets its own manager in the runner, so transitions on two
    /// displays advance independently instead of one replacing the other.
    #[test]
    fn transitions_on_different_displays_are_independent() {
        let (handle, mut rx) = AppThreadHandle::channel();
        let left = WindowId::new(1, 1);
        let right = WindowId::new(1, 2);
        let (sender, receiver) = AnimationSender::channel();
        let runner = std::thread::spawn(move || AnimationManager::run(receiver));
        let mut first = Animation::new(100.0, 0.05, DisplayId::new(1));
        first.add_window(
            &handle,
            left,
            rect(0.0, 0.0, 10.0, 10.0),
            rect(100.0, 0.0, 10.0, 10.0),
            TransactionId::default(),
        );
        // Sharing one manager, this would cancel `left`: replacement drops windows the
        // new layout handled without animating.
        let mut second = Animation::new(100.0, 0.05, DisplayId::new(2));
        second.add_window(
            &handle,
            right,
            rect(0.0, 0.0, 10.0, 10.0),
            rect(2000.0, 0.0, 10.0, 10.0),
            TransactionId::default(),
        );
        second.mark_handled(left);
        sender.send(Message::Replace(first)).unwrap();
        sender.send(Message::Replace(second)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut seen = Vec::new();
        let ended = |seen: &[Observed]| {
            seen.iter()
                .filter(|r| matches!(r, Observed::Request(Request::EndWindowAnimation(_))))
                .count()
        };
        while ended(&seen) < 2 && Instant::now() < deadline {
            seen.extend(collect_requests(&mut rx));
            std::thread::sleep(Duration::from_millis(1));
        }
        drop(sender);
        runner.join().unwrap();
        seen.extend(collect_requests(&mut rx));
        for wid in [left, right] {
            assert!(
                seen.iter().any(|r| matches!(
                    r, Observed::Request(Request::EndWindowAnimation(id)) if *id == wid
                )),
                "{wid:?} must finish its transition: {seen:?}"
            );
        }
        assert!(
            !seen.iter().any(|r| matches!(r, Observed::Request(Request::CancelWindowAnimation(..)))),
            "a transition on one display must not cancel another's: {seen:?}"
        );
    }

    #[test]
    fn cancellation_discards_pending_frames_and_does_not_replay_the_target() {
        let (handle, mut rx) = AppThreadHandle::channel();
        let wid = WindowId::new(1, 1);
        let (sender, commands_rx) = AnimationSender::channel();
        let runner = std::thread::spawn(move || AnimationManager::run(commands_rx));
        sender
            .send(Message::Replace(animation(
                &handle,
                wid,
                rect(0.0, 0.0, 10.0, 10.0),
                rect(100.0, 50.0, 10.0, 10.0),
            )))
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        // Leave the display wake outstanding so cancellation must discard it.
        while !sender
            .control
            .lock()
            .frames
            .get(&wid)
            .is_some_and(|(_, frame)| frame.origin.x > 0.0)
            && Instant::now() < deadline
        {
            std::thread::yield_now();
        }
        let frames = sender.cancel(vec![wid]);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].0, wid);
        assert!(frames[0].1.origin.x > 0.0 && frames[0].1.origin.x < 100.0);
        drop(sender);
        runner.join().unwrap();
        let requests = collect_requests(&mut rx);
        assert!(requests.iter().any(
            |r| matches!(r, Observed::Request(Request::CancelWindowAnimation(id, _)) if *id == wid)
        ));
        assert!(
            requests.iter().all(|r| matches!(
                r,
                Observed::Request(
                    Request::BeginWindowAnimation(_) | Request::CancelWindowAnimation(..)
                )
            )),
            "cancelled frames and the target cannot be replayed: {requests:?}"
        );
    }

    #[test]
    fn replacement_uses_last_animated_frame_for_continuing_windows() {
        let (tx, mut rx) = crate::actor::channel();
        let handle = AppThreadHandle::new_for_test(tx);
        let wid = WindowId::new(1, 1);
        let first = animation(
            &handle,
            wid,
            rect(0.0, 0.0, 10.0, 10.0),
            rect(50.0, 60.0, 10.0, 10.0),
        );
        let second = animation(
            &handle,
            wid,
            rect(50.0, 60.0, 10.0, 10.0),
            rect(80.0, 90.0, 10.0, 10.0),
        );

        let mut manager = AnimationManager::new();
        manager.handle_message(Message::Replace(first));
        assert!(matches!(
            collect_requests(&mut rx).as_slice(),
            [Observed::Request(Request::BeginWindowAnimation(req_wid))] if *req_wid == wid
        ));

        manager.tick_at(transition_state(&manager).unwrap().started + Duration::from_millis(10));
        let active = transition_state(&manager).unwrap();
        let continuing_frame = active.animation.windows[0].frame;
        assert_animation_pos(&collect_requests(&mut rx)[0], wid, continuing_frame.origin);

        manager.handle_message(Message::Replace(second));
        assert!(collect_requests(&mut rx).is_empty());

        let resumed_start = transition_state(&manager).unwrap().animation.windows[0].from;
        assert_eq!(resumed_start, continuing_frame);

        manager.tick_at(transition_state(&manager).unwrap().started + Duration::from_millis(10));
        let expected_next = get_frame(resumed_start, rect(80.0, 90.0, 10.0, 10.0), 1.0 / 30.0);
        assert_animation_pos(&collect_requests(&mut rx)[0], wid, expected_next.origin);
    }

    fn animation_contains(manager: &AnimationManager, wid: WindowId) -> bool {
        transition_state(manager).is_some_and(|a| a.animation.windows.iter().any(|w| w.wid == wid))
    }

    #[test]
    fn replacement_only_restarts_changed_windows() {
        let (tx, mut rx) = crate::actor::channel();
        let handle = AppThreadHandle::new_for_test(tx);
        let wid1 = WindowId::new(1, 1);
        let wid2 = WindowId::new(1, 2);
        let wid3 = WindowId::new(1, 3);
        let mut first = empty_animation();
        first.add_window(
            &handle,
            wid1,
            rect(0.0, 0.0, 10.0, 10.0),
            rect(50.0, 60.0, 10.0, 10.0),
            TransactionId::default(),
        );
        first.add_window(
            &handle,
            wid2,
            rect(10.0, 0.0, 10.0, 10.0),
            rect(60.0, 60.0, 10.0, 10.0),
            TransactionId::default(),
        );
        let mut second = empty_animation();
        second.add_window(
            &handle,
            wid1,
            rect(50.0, 60.0, 10.0, 10.0),
            rect(80.0, 90.0, 10.0, 10.0),
            TransactionId::default(),
        );
        second.add_window(
            &handle,
            wid3,
            rect(20.0, 0.0, 10.0, 10.0),
            rect(90.0, 90.0, 10.0, 10.0),
            TransactionId::default(),
        );

        let mut manager = AnimationManager::new();
        manager.handle_message(Message::Replace(first));
        assert_eq!(collect_requests(&mut rx).len(), 2);
        manager.handle_message(Message::Replace(second));

        let requests = collect_requests(&mut rx);
        assert_eq!(requests.len(), 1);
        assert!(
            matches!(requests[0], Observed::Request(Request::BeginWindowAnimation(req_wid)) if req_wid == wid3)
        );
        assert!(animation_contains(&manager, wid2));

        let carried = transition_state(&manager)
            .unwrap()
            .animation
            .windows
            .iter()
            .find(|w| w.wid == wid2)
            .unwrap();
        assert_eq!(carried.to, rect(60.0, 60.0, 10.0, 10.0));
    }

    #[test]
    fn replacement_does_not_carry_over_explicitly_handled_windows() {
        let (tx, mut rx) = crate::actor::channel();
        let handle = AppThreadHandle::new_for_test(tx);
        let wid1 = WindowId::new(1, 1);
        let wid2 = WindowId::new(1, 2);
        let mut first = empty_animation();
        first.add_window(
            &handle,
            wid1,
            rect(0.0, 0.0, 10.0, 10.0),
            rect(50.0, 60.0, 10.0, 10.0),
            TransactionId::default(),
        );
        first.add_window(
            &handle,
            wid2,
            rect(10.0, 0.0, 10.0, 10.0),
            rect(60.0, 60.0, 10.0, 10.0),
            TransactionId::default(),
        );
        let mut second = empty_animation();
        second.add_window(
            &handle,
            wid1,
            rect(50.0, 60.0, 10.0, 10.0),
            rect(80.0, 90.0, 10.0, 10.0),
            TransactionId::default(),
        );
        second.mark_handled(wid2);

        let mut manager = AnimationManager::new();
        manager.handle_message(Message::Replace(first));
        let _ = collect_requests(&mut rx);
        manager.handle_message(Message::Replace(second));

        assert!(!animation_contains(&manager, wid2));
    }

    #[test]
    fn skip_to_end_finishes_active_animation_and_applies_new_layout() {
        let (tx, mut rx) = crate::actor::channel();
        let handle = AppThreadHandle::new_for_test(tx);
        let wid = WindowId::new(1, 1);
        let first = animation(
            &handle,
            wid,
            rect(0.0, 0.0, 10.0, 10.0),
            rect(50.0, 60.0, 10.0, 10.0),
        );
        let second = animation(
            &handle,
            wid,
            rect(50.0, 60.0, 10.0, 10.0),
            rect(80.0, 90.0, 10.0, 10.0),
        );

        let mut manager = AnimationManager::new();
        manager.handle_message(Message::Replace(first));
        manager.handle_message(Message::SkipToEnd(second));

        let requests = collect_requests(&mut rx);
        assert_eq!(requests.len(), 4);
        assert!(
            matches!(requests[0], Observed::Request(Request::BeginWindowAnimation(req_wid)) if req_wid == wid)
        );
        assert_animation_frame(&requests[1], wid, rect(50.0, 60.0, 10.0, 10.0));
        assert!(
            matches!(requests[2], Observed::Request(Request::EndWindowAnimation(req_wid)) if req_wid == wid)
        );
        assert_set_window_frame(&requests[3], wid, rect(80.0, 90.0, 10.0, 10.0));
    }

    // Layout entry points, driven through a fake reactor.

    struct FakeHost {
        tx: Option<AnimationSender>,
        tm: TransactionManager,
        settings: AnimationSettings,
        windows: HashMap<WindowId, HostWindow>,
        apps: HashMap<pid_t, AppThreadHandle>,
        visible: Vec<(DisplayId, WindowId)>,
    }

    impl FakeHost {
        fn new(tx: Option<AnimationSender>) -> Self {
            Self {
                tx,
                tm: TransactionManager::new(WindowTxStore::new()),
                settings: AnimationSettings::default(),
                windows: HashMap::default(),
                apps: HashMap::default(),
                visible: Vec::new(),
            }
        }

        fn add_app(&mut self, pid: pid_t) -> crate::actor::Receiver<Request> {
            let (handle, rx) = AppThreadHandle::channel();
            self.apps.insert(pid, handle);
            rx
        }

        fn add_window(&mut self, wid: WindowId, frame: CGRect, visible_on: Option<DisplayId>) {
            let wsid = WindowServerId::new(wid.idx.get());
            self.windows.insert(wid, HostWindow { frame, wsid: Some(wsid) });
            if let Some(display) = visible_on {
                self.visible.push((display, wid));
            }
        }

        fn wsid(&self, wid: WindowId) -> WindowServerId { self.windows[&wid].wsid.unwrap() }
    }

    impl PresenterHost for FakeHost {
        fn animation_tx(&self) -> Option<&AnimationSender> { self.tx.as_ref() }

        fn transaction_manager(&self) -> &TransactionManager { &self.tm }

        fn animation_settings(&self) -> AnimationSettings { self.settings }

        fn window(&self, wid: WindowId) -> Option<HostWindow> { self.windows.get(&wid).copied() }

        fn set_window_frame(&mut self, wid: WindowId, frame: CGRect) {
            self.windows.get_mut(&wid).unwrap().frame = frame;
        }

        fn app_handle(&self, pid: pid_t) -> Option<AppThreadHandle> { self.apps.get(&pid).cloned() }

        fn is_visible_on(&self, display: DisplayId, wid: WindowId) -> bool {
            self.visible.contains(&(display, wid))
        }
    }

    fn sent_frames(rx: &mut crate::actor::Receiver<Request>) -> Vec<(FrameMode, Vec<(WindowId, CGRect)>)> {
        collect_requests(rx)
            .into_iter()
            .filter_map(|r| match r {
                Observed::Request(Request::SetWindowFrames(frames, _, mode, _)) => {
                    Some((mode, frames))
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn animate_layout_keys_the_transition_by_display_and_positions_hidden_windows_directly() {
        let (sender, receiver) = AnimationSender::channel();
        let mut host = FakeHost::new(Some(sender));
        let mut app = host.add_app(1);
        let display = DisplayId::new(2);
        let shown = WindowId::new(1, 10);
        let hidden = WindowId::new(1, 11);
        host.add_window(shown, rect(0.0, 0.0, 100.0, 100.0), Some(display));
        host.add_window(hidden, rect(0.0, 0.0, 100.0, 100.0), None);
        let layout = [
            (shown, rect(200.0, 0.0, 100.0, 100.0)),
            (hidden, rect(400.0, 0.0, 100.0, 100.0)),
        ];

        assert!(AnimationManager::animate_layout(&mut host, display, &layout, false, None));

        let Ok(Message::Replace(animation)) = receiver.commands.try_recv() else {
            panic!("expected a transition for the presenter");
        };
        assert_eq!(animation.display, display);
        assert_eq!(animation.windows.len(), 1);
        assert_eq!(animation.windows[0].wid, shown);
        assert_eq!(animation.windows[0].from, rect(0.0, 0.0, 100.0, 100.0));
        assert_eq!(animation.windows[0].to, layout[0].1);
        assert_eq!(sent_frames(&mut app), vec![(FrameMode::Full, vec![layout[1]])]);
        for (wid, target) in layout {
            assert_eq!(host.windows[&wid].frame, target);
            assert_eq!(host.tm.get_target_frame(host.wsid(wid)), Some(target));
        }
    }

    #[test]
    fn animate_layout_skips_windows_already_at_or_heading_to_their_target() {
        let (sender, receiver) = AnimationSender::channel();
        let mut host = FakeHost::new(Some(sender));
        let _app = host.add_app(1);
        let at_target = WindowId::new(1, 1);
        let heading = WindowId::new(1, 2);
        let stale = WindowId::new(1, 3);
        let target = rect(50.0, 0.0, 10.0, 10.0);
        for wid in [at_target, heading, stale] {
            host.add_window(wid, rect(0.0, 0.0, 10.0, 10.0), Some(DISPLAY));
        }
        host.set_window_frame(at_target, target);
        host.tm.store_txid(host.wsid(heading), TransactionId::default(), target);
        // An older, different target: matching the observed frame cannot cancel it.
        host.set_window_frame(stale, target);
        host.tm.store_txid(host.wsid(stale), TransactionId::default(), rect(9.0, 9.0, 9.0, 9.0));

        let layout = [(at_target, target), (heading, target)];
        assert!(!AnimationManager::animate_layout(&mut host, DISPLAY, &layout, false, None));
        assert!(receiver.commands.try_recv().is_err());

        assert!(AnimationManager::animate_layout(&mut host, DISPLAY, &[(stale, target)], false, None));
        let Ok(Message::Replace(animation)) = receiver.commands.try_recv() else {
            panic!("expected the stale window to be re-targeted");
        };
        assert_eq!(animation.windows[0].wid, stale);
    }

    #[test]
    fn resize_layouts_skip_to_the_end_and_without_a_presenter_apply_immediately() {
        let (sender, receiver) = AnimationSender::channel();
        let mut host = FakeHost::new(Some(sender));
        let _app = host.add_app(1);
        let wid = WindowId::new(1, 1);
        host.add_window(wid, rect(0.0, 0.0, 10.0, 10.0), Some(DISPLAY));
        let target = rect(0.0, 0.0, 300.0, 200.0);
        assert!(AnimationManager::animate_layout(&mut host, DISPLAY, &[(wid, target)], true, None));
        // In-flight presentation is cancelled before the new target is applied.
        assert!(matches!(receiver.commands.try_recv(), Ok(Message::Stop(w)) if w == vec![wid]));
        assert!(matches!(receiver.commands.try_recv(), Ok(Message::SkipToEnd(_))));

        let mut host = FakeHost::new(None);
        let mut app = host.add_app(1);
        host.add_window(wid, rect(0.0, 0.0, 10.0, 10.0), Some(DISPLAY));
        assert!(AnimationManager::animate_layout(&mut host, DISPLAY, &[(wid, target)], false, None));
        assert_eq!(sent_frames(&mut app), vec![(FrameMode::Full, vec![(wid, target)])]);
    }

    #[test]
    fn workspace_switch_layout_moves_same_size_windows_by_position_only() {
        let mut host = FakeHost::new(None);
        let mut app = host.add_app(1);
        let moved = WindowId::new(1, 1);
        let resized = WindowId::new(1, 2);
        host.add_window(moved, rect(0.0, 0.0, 100.0, 100.0), None);
        host.add_window(resized, rect(0.0, 0.0, 100.0, 100.0), None);
        let layout = [
            (moved, rect(-99.0, 900.0, 100.0, 100.0)),
            (resized, rect(-99.0, 900.0, 50.0, 50.0)),
        ];

        assert!(AnimationManager::workspace_switch_layout(&mut host, &layout, None));
        assert_eq!(
            sent_frames(&mut app),
            vec![
                (FrameMode::Position, vec![layout[0]]),
                (FrameMode::Full, vec![layout[1]]),
            ]
        );

        // Already there: nothing to send.
        assert!(!AnimationManager::instant_layout(&mut host, &layout, None));
        assert!(sent_frames(&mut app).is_empty());
    }

    #[test]
    fn instant_layout_cancels_in_flight_transitions() {
        let (sender, receiver) = AnimationSender::channel();
        let mut host = FakeHost::new(Some(sender));
        let mut app = host.add_app(1);
        let wid = WindowId::new(1, 1);
        host.add_window(wid, rect(0.0, 0.0, 10.0, 10.0), Some(DISPLAY));
        let midway = rect(40.0, 0.0, 10.0, 10.0);
        let handle = host.apps[&1].clone();
        host.tx.as_ref().unwrap().control.lock().frames.insert(wid, (handle, midway));
        host.tm.store_txid(host.wsid(wid), TransactionId::default(), rect(80.0, 0.0, 10.0, 10.0));

        // Targeting where the transition stopped: once the midway frame is adopted
        // and the stale target cleared, there is nothing left to send.
        assert!(!AnimationManager::instant_layout(&mut host, &[(wid, midway)], None));

        assert!(matches!(receiver.commands.try_recv(), Ok(Message::Stop(w)) if w == vec![wid]));
        let requests = collect_requests(&mut app);
        assert!(
            matches!(
                requests.as_slice(),
                [Observed::Request(Request::CancelWindowAnimation(id, _))] if *id == wid
            ),
            "only the cancellation is sent: {requests:?}"
        );
        assert_eq!(host.windows[&wid].frame, midway);
        assert_eq!(host.tm.get_target_frame(host.wsid(wid)), None);

        let target = rect(500.0, 0.0, 10.0, 10.0);
        assert!(AnimationManager::instant_layout(&mut host, &[(wid, target)], None));
        assert_eq!(sent_frames(&mut app), vec![(FrameMode::Full, vec![(wid, target)])]);
        assert_eq!(host.windows[&wid].frame, target);
        assert_eq!(host.tm.get_target_frame(host.wsid(wid)), Some(target));
    }
}
