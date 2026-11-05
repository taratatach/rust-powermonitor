//! Shared core used by every backend (callback registry, start-up
//! handshake, idle mapping) and per-platform dispatch. Unsupported
//! platforms fall back to the stub below.

use std::any::Any;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;

use crate::{Error, IdleState, PowerEvent};

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "macos")]
use macos as backend;
#[cfg(target_os = "windows")]
use windows as backend;
#[cfg(target_os = "linux")]
use linux as backend;

/// Stub for unsupported platforms: no events, no idle information.
#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
mod backend {
    use super::Callback;
    use crate::{Error, Options};

    pub fn listen(_opts: Options, _cb: Callback) -> Result<(), Error> {
        Err(Error("unsupported platform".into()))
    }

    pub fn is_on_battery_power() -> bool {
        false
    }

    pub fn system_idle_time() -> Option<u64> {
        None
    }
}

// Every backend provides:
// - `listen(Options, Callback)`: registers the callback, starts the backend
//   on first use;
// - `is_on_battery_power() -> bool`;
// - `system_idle_time() -> Option<u64>`: seconds, `None` if no idle source
//   answers.
pub(crate) use backend::{is_on_battery_power, listen, system_idle_time};

/// A user callback, shared between the registry and in-flight dispatches.
pub(crate) type Callback = Arc<dyn Fn(PowerEvent) + Send + Sync>;

/// Channel through which a backend thread reports the outcome of its
/// initialization to the `listen` caller.
pub(crate) type Ready = SyncSender<Result<(), Error>>;

/// Registry of user callbacks.
///
/// Callbacks are called outside the lock, so they may register other
/// callbacks, and a panicking callback neither poisons the registry nor
/// prevents the others from running.
pub(crate) struct Callbacks(Mutex<Vec<Callback>>);

impl Callbacks {
    pub(crate) const fn new() -> Self {
        Self(Mutex::new(Vec::new()))
    }

    fn lock(&self) -> MutexGuard<'_, Vec<Callback>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn register(&self, cb: Callback) {
        self.lock().push(cb);
    }

    /// Calls every callback registered when the dispatch starts. A
    /// callback registered during the dispatch only gets later events.
    pub(crate) fn dispatch(&self, evt: PowerEvent) {
        let snapshot: Vec<Callback> = self.lock().clone();
        for cb in snapshot {
            if let Err(payload) = catch_unwind(AssertUnwindSafe(|| cb(evt))) {
                eprintln!(
                    "powermonitor: a callback panicked on {evt:?}: {}",
                    panic_message(&*payload)
                );
            }
        }
    }
}

fn panic_message(payload: &(dyn Any + Send)) -> &str {
    if let Some(s) = payload.downcast_ref::<&str>() {
        s
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s
    } else {
        "<non-string panic payload>"
    }
}

static CALLBACKS: Callbacks = Callbacks::new();

/// Adds `cb` to the process-wide registry.
#[cfg_attr(
    not(any(target_os = "macos", target_os = "windows", target_os = "linux")),
    allow(dead_code)
)]
pub(crate) fn register(cb: Callback) {
    CALLBACKS.register(cb);
}

/// Sends `evt` to every callback of the process-wide registry.
#[cfg_attr(
    not(any(target_os = "macos", target_os = "windows", target_os = "linux")),
    allow(dead_code)
)]
pub(crate) fn dispatch(evt: PowerEvent) {
    CALLBACKS.dispatch(evt);
}

/// Spawns the backend thread running `run` unless `started` is already
/// set, then waits until `run` reports its initialization through the
/// [`Ready`] channel.
///
/// Only the caller that flips `started` waits for the handshake; the
/// others return `Ok(())` immediately. On failure (spawn error, `Err`
/// reported, or thread gone without reporting), `started` is reset so
/// that a later call retries the initialization.
#[cfg_attr(
    not(any(target_os = "macos", target_os = "windows", target_os = "linux")),
    allow(dead_code)
)]
pub(crate) fn start_once<F>(started: &AtomicBool, run: F) -> Result<(), Error>
where
    F: FnOnce(Ready) + Send + 'static,
{
    if started
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return Ok(());
    }
    let (ready, outcome) = sync_channel(1);
    let result = thread::Builder::new()
        .name("powermonitor".into())
        .spawn(move || run(ready))
        .map_err(|e| Error(format!("cannot spawn the backend thread: {e}")))
        .and_then(|_| {
            outcome.recv().unwrap_or_else(|_| {
                Err(Error(
                    "the backend thread exited before completing its initialization".into(),
                ))
            })
        });
    if result.is_err() {
        started.store(false, Ordering::SeqCst);
    }
    result
}

/// Maps an idle time (seconds, `None` if unknown) to an [`IdleState`].
pub(crate) fn idle_state(idle: Option<u64>, threshold: u64) -> IdleState {
    match idle {
        None => IdleState::Unknown,
        Some(t) if t >= threshold => IdleState::Idle,
        Some(_) => IdleState::Active,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::IdleState;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, mpsc};
    use std::time::Duration;

    fn counter(hits: &Arc<AtomicUsize>) -> Callback {
        let hits = hits.clone();
        Arc::new(move |_| {
            hits.fetch_add(1, Ordering::SeqCst);
        })
    }

    #[test]
    fn dispatch_calls_all_callbacks() {
        let callbacks = Callbacks::new();
        let hits = Arc::new(AtomicUsize::new(0));
        let hits2 = hits.clone();
        callbacks.register(Arc::new(move |evt| {
            assert_eq!(evt, PowerEvent::Suspend);
            hits2.fetch_add(1, Ordering::SeqCst);
        }));
        callbacks.register(counter(&hits));
        callbacks.dispatch(PowerEvent::Suspend);
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn dispatch_releases_lock_before_calling() {
        static CALLBACKS: Callbacks = Callbacks::new();
        let late_hits = Arc::new(AtomicUsize::new(0));
        let registered = Arc::new(AtomicBool::new(false));
        {
            let late_hits = late_hits.clone();
            let registered = registered.clone();
            CALLBACKS.register(Arc::new(move |_| {
                // Re-entrant registration: deadlocks if the lock is held.
                if !registered.swap(true, Ordering::SeqCst) {
                    CALLBACKS.register(counter(&late_hits));
                }
            }));
        }

        let (done_tx, done_rx) = mpsc::channel();
        std::thread::spawn(move || {
            CALLBACKS.dispatch(PowerEvent::Suspend);
            let _ = done_tx.send(());
        });
        done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("dispatch deadlocked on a re-entrant register");

        // The callback added during the dispatch only sees the next event.
        assert_eq!(late_hits.load(Ordering::SeqCst), 0);
        CALLBACKS.dispatch(PowerEvent::Resume);
        assert_eq!(late_hits.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn dispatch_isolates_panics() {
        let callbacks = Callbacks::new();
        let hits = Arc::new(AtomicUsize::new(0));
        callbacks.register(Arc::new(|_| panic!("boom")));
        callbacks.register(counter(&hits));

        callbacks.dispatch(PowerEvent::Suspend);
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        assert!(!callbacks.0.is_poisoned());

        // The monitor keeps working after the panic.
        callbacks.register(counter(&hits));
        callbacks.dispatch(PowerEvent::Resume);
        assert_eq!(hits.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn idle_state_maps_none_to_unknown() {
        assert_eq!(idle_state(None, 0), IdleState::Unknown);
        assert_eq!(idle_state(None, 60), IdleState::Unknown);
    }

    #[test]
    fn idle_state_threshold_boundaries() {
        assert_eq!(idle_state(Some(59), 60), IdleState::Active);
        assert_eq!(idle_state(Some(60), 60), IdleState::Idle);
        assert_eq!(idle_state(Some(61), 60), IdleState::Idle);
        assert_eq!(idle_state(Some(0), 0), IdleState::Idle);
    }

    #[test]
    fn start_once_resets_on_error() {
        let started = AtomicBool::new(false);
        let attempts = Arc::new(AtomicUsize::new(0));

        // The init fails: the error reaches the caller and the flag is reset.
        let a = attempts.clone();
        let res = start_once(&started, move |ready| {
            a.fetch_add(1, Ordering::SeqCst);
            let _ = ready.send(Err(Error("init failed".into())));
        });
        assert_eq!(res.unwrap_err().0, "init failed");
        assert!(!started.load(Ordering::SeqCst));

        // The backend thread exits without reporting: also an error.
        let a = attempts.clone();
        let res = start_once(&started, move |ready| {
            a.fetch_add(1, Ordering::SeqCst);
            drop(ready);
        });
        assert!(res.is_err());
        assert!(!started.load(Ordering::SeqCst));

        // A later attempt runs the init again and succeeds.
        let a = attempts.clone();
        let res = start_once(&started, move |ready| {
            a.fetch_add(1, Ordering::SeqCst);
            let _ = ready.send(Ok(()));
        });
        assert!(res.is_ok());
        assert!(started.load(Ordering::SeqCst));
        assert_eq!(attempts.load(Ordering::SeqCst), 3);

        // Once started, the init never runs again.
        let a = attempts.clone();
        let res = start_once(&started, move |ready| {
            a.fetch_add(1, Ordering::SeqCst);
            let _ = ready.send(Err(Error("must not run".into())));
        });
        assert!(res.is_ok());
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
    }
}
