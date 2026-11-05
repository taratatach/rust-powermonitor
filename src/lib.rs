//! System power monitor inspired by Electron's `powerMonitor` module.
//!
//! Events: suspend/resume, AC/battery, lock/unlock-screen (and shutdown).
//! Queries: `is_on_battery_power`, `system_idle_time`, `system_idle_state`.

use std::fmt;
use std::sync::Arc;

mod platform;

/// Event emitted by the monitor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
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
#[non_exhaustive]
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

/// Monitor options, passed to [`listen_with`].
///
/// They are read only by the call that starts the backend; later calls
/// ignore them.
///
/// ```
/// let mut opts = powermonitor::Options::default();
/// opts.delay_suspend = true;
/// ```
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct Options {
    /// Delay system suspend until the [`PowerEvent::Suspend`] callbacks
    /// have returned.
    ///
    /// - Linux: takes a logind `delay` inhibitor, released once the
    ///   callbacks have run (logind caps the delay, usually 5 seconds).
    /// - macOS: acknowledges the sleep request (`IOAllowPowerChange`) only
    ///   after the callbacks have run.
    /// - Windows: no effect.
    pub delay_suspend: bool,
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

/// Subscribes to power events with the default [`Options`].
///
/// See [`listen_with`] for the callback contract.
pub fn listen(cb: impl Fn(PowerEvent) + Send + Sync + 'static) -> Result<(), Error> {
    listen_with(Options::default(), cb)
}

/// Subscribes to power events.
///
/// The first call starts the backend thread and returns once the backend
/// is initialized: it returns `Err` if the initialization fails, and the
/// next call retries it. Later calls only add a callback and return
/// `Ok(())` at once, without waiting for the backend: a call that overlaps
/// the first one (from another thread) may therefore return `Ok(())`
/// before the backend is up, or even when its initialization then fails.
///
/// Callback contract:
///
/// - callbacks run on a background thread owned by the backend, in
///   registration order (on macOS, lock/unlock callbacks run on the main
///   run loop thread instead); they may be called from different threads,
///   possibly concurrently, hence the `Sync` bound;
/// - they may call [`listen`]/[`listen_with`] (a callback added during an
///   event only receives the following events);
/// - a panicking callback is caught and logged on stderr; the other
///   callbacks still run and the monitor keeps working;
/// - `opts` is only read by the call that starts the backend;
/// - the callback stays registered even when this call returns `Err`.
pub fn listen_with(
    opts: Options,
    cb: impl Fn(PowerEvent) + Send + Sync + 'static,
) -> Result<(), Error> {
    platform::listen(opts, Arc::new(cb))
}

/// `true` if the system is on battery power.
pub fn is_on_battery_power() -> bool {
    platform::is_on_battery_power()
}

/// System idle time in seconds. 0 if unavailable/undeterminable.
pub fn system_idle_time() -> u64 {
    platform::system_idle_time().unwrap_or(0)
}

/// Idle state vs the `threshold` in seconds: [`IdleState::Unknown`] when
/// the idle time cannot be determined. `Locked` is never returned by this
/// function (locking goes through events).
pub fn system_idle_state(threshold: u64) -> IdleState {
    platform::idle_state(platform::system_idle_time(), threshold)
}
