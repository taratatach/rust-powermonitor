//! Per-platform dispatch. Unsupported platforms fall back to the stub below.

use crate::{Error, PowerEvent};

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "macos")]
use macos as backend;
#[cfg(target_os = "linux")]
use linux as backend;

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod backend {} // stub for unsupported platforms

pub fn listen(cb: Box<dyn Fn(PowerEvent) + Send>) -> Result<(), Error> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    return backend::listen(cb);
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = cb;
        Err(Error("unsupported platform".into()))
    }
}

pub fn is_on_battery_power() -> bool {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    return backend::is_on_battery_power();
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        false // ponytail: without a backend, assume AC power
    }
}

pub fn system_idle_time() -> u64 {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    return backend::system_idle_time();
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        0
    }
}

pub(crate) type Callbacks = std::sync::Mutex<Vec<Box<dyn Fn(PowerEvent) + Send>>>;

pub(crate) fn dispatch(evt: PowerEvent, callbacks: &Callbacks) {
    let guard = callbacks.lock().unwrap();
    for cb in guard.iter() {
        cb(evt);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    #[test]
    fn dispatch_calls_all_callbacks() {
        let callbacks: Callbacks = Mutex::new(Vec::new());
        let hits = Arc::new(AtomicUsize::new(0));
        let hits2 = hits.clone();
        callbacks
            .lock()
            .unwrap()
            .push(Box::new(move |evt| {
                assert_eq!(evt, PowerEvent::Suspend);
                hits2.fetch_add(1, Ordering::SeqCst);
            }));
        dispatch(PowerEvent::Suspend, &callbacks);
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }
}
