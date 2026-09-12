//! System power monitor inspired by Electron's `powerMonitor` module.
//!
//! Events: suspend/resume, AC/battery, lock/unlock-screen (and shutdown).
//! Queries: `is_on_battery_power`, `system_idle_time`, `system_idle_state`.

use std::fmt;

mod platform;

/// Event emitted by the monitor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerEvent {
    /// The system is about to suspend.
    Suspend,
    /// The system just resumed.
    Resume,
    /// Switched to AC power.
    OnAc,
    /// Switched to battery power.
    OnBattery,
    /// The screen is about to lock.
    LockScreen,
    /// The screen was just unlocked.
    UnlockScreen,
    /// The system is about to restart/shut down.
    Shutdown,
}

/// System idle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleState {
    /// User is active.
    Active,
    /// User has been idle for `threshold` seconds.
    Idle,
    /// Screen locked (not detectable by this crate: see docs).
    Locked,
    /// Cannot be determined.
    Unknown,
}

/// Crate error.
#[derive(Debug)]
pub struct Error(pub(crate) String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "powermonitor: {}", self.0)
    }
}

impl std::error::Error for Error {}

/// Subscribes to power events.
///
/// The first call spawns the backend thread; subsequent calls simply add
/// a callback. Callbacks run on the backend thread.
pub fn listen(cb: impl Fn(PowerEvent) + Send + 'static) -> Result<(), Error> {
    platform::listen(Box::new(cb))
}

/// `true` if the system is on battery power.
pub fn is_on_battery_power() -> bool {
    // ponytail: windows returns false if the snapshot is unavailable, linux/upower hardcodes false
    platform::is_on_battery_power()
}

/// System idle time in seconds. 0 if unavailable/undeterminable.
pub fn system_idle_time() -> u64 {
    platform::system_idle_time()
}

/// Idle state vs the `threshold` in seconds. `Locked` is never returned
/// by this function (locking goes through events).
pub fn system_idle_state(threshold: u64) -> IdleState {
    let t = system_idle_time();
    if t >= threshold {
        IdleState::Idle
    } else {
        IdleState::Active
    }
}
