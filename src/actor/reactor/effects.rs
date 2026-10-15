//! What the reactor asks the platform shell to do.
//!
//! The reactor touches no OS API. When a command needs one (spawn a process, move the
//! cursor, swap the hotkey table), the reactor queues an [`Effect`]; the daemon drains
//! the queue after every event and a platform actor carries it out. Tests read the same
//! queue with `Reactor::take_effects`.

use std::sync::Arc;

use objc2_core_foundation::CGRect;

use crate::actor;
use crate::config::Config;

#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    /// `exec-and-forget`: run this through `/bin/bash -c`, detached.
    Exec(String),
    /// `mode <name>`: switch the active binding mode.
    SetMode(String),
    /// `reload-config`: re-read the config file now.
    ReloadConfig,
    /// `move-mouse`: center the cursor in `rect` (global top-left coordinates). With
    /// `lazy`, do nothing if the cursor is already inside it.
    MoveMouse { rect: CGRect, lazy: bool },
    /// A config was applied: swap the hotkey table and the focus-follows-mouse switch.
    ConfigApplied(Arc<Config>),
}

pub type EffectSender = actor::Sender<Effect>;
pub type EffectReceiver = actor::Receiver<Effect>;
