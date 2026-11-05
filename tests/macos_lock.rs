//! End-to-end scenario for the macOS lock/unlock events.
//!
//! The test posts `com.apple.screenIsLocked` / `com.apple.screenIsUnlocked`
//! on the distributed notification center and expects the matching
//! [`PowerEvent`]s. Distributed notifications are delivered on the main
//! thread, so this binary uses `harness = false`: `main` owns the main
//! thread and runs its run loop while waiting.
//!
//! The backend state is process-global, so this scenario has its own test
//! binary. On other platforms it compiles to an empty `main`.

#[cfg(not(target_os = "macos"))]
fn main() {}

#[cfg(target_os = "macos")]
fn main() {
    scenario::run();
}

#[cfg(target_os = "macos")]
mod scenario {
    use std::ffi::{CStr, c_void};
    use std::sync::mpsc::{self, Receiver};
    use std::time::{Duration, Instant};

    use powermonitor::{IdleState, PowerEvent};

    /// Upper bound for each expected event.
    const EVENT_TIMEOUT: Duration = Duration::from_secs(5);
    /// Slice of main run loop time between two checks of the event log.
    const RUN_LOOP_SLICE: f64 = 0.05;

    const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
    /// `kCFRunLoopRunFinished`: the mode has no source to run.
    const K_CF_RUN_LOOP_RUN_FINISHED: i32 = 1;

    type CFStringRef = *const c_void;
    type CFDictionaryRef = *const c_void;
    type CFNotificationCenterRef = *mut c_void;

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        static kCFRunLoopDefaultMode: CFStringRef;

        fn CFRunLoopRunInMode(
            mode: CFStringRef,
            seconds: f64,
            return_after_source_handled: u8,
        ) -> i32;
        fn CFRelease(cf: *const c_void);
        fn CFStringCreateWithCString(
            alloc: *const c_void,
            c_str: *const i8,
            encoding: u32,
        ) -> CFStringRef;
        fn CFNotificationCenterGetDistributedCenter() -> CFNotificationCenterRef;
        fn CFNotificationCenterPostNotification(
            center: CFNotificationCenterRef,
            name: CFStringRef,
            object: *const c_void,
            user_info: CFDictionaryRef,
            deliver_immediately: u8,
        );
    }

    /// Posts `name` on the distributed center with `deliverImmediately`.
    fn post_distributed(name: &CStr) {
        // SAFETY: plain CoreFoundation calls; the string is created and
        // released here, the distributed center is a process singleton.
        unsafe {
            let center = CFNotificationCenterGetDistributedCenter();
            assert!(!center.is_null(), "no distributed notification center");
            let cf_name = CFStringCreateWithCString(
                std::ptr::null(),
                name.as_ptr(),
                K_CF_STRING_ENCODING_UTF8,
            );
            assert!(!cf_name.is_null(), "cannot create the CFString {name:?}");
            CFNotificationCenterPostNotification(
                center,
                cf_name,
                std::ptr::null(),
                std::ptr::null(),
                1,
            );
            CFRelease(cf_name);
        }
    }

    /// Runs the main run loop until `expected` is received or `timeout`
    /// elapses. Other events are skipped.
    fn wait_on_main_run_loop(
        rx: &Receiver<PowerEvent>,
        expected: PowerEvent,
        timeout: Duration,
    ) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            while let Ok(evt) = rx.try_recv() {
                if evt == expected {
                    return true;
                }
            }
            if Instant::now() >= deadline {
                return false;
            }
            // SAFETY: called on the main thread, which owns its run loop.
            let outcome = unsafe { CFRunLoopRunInMode(kCFRunLoopDefaultMode, RUN_LOOP_SLICE, 0) };
            if outcome == K_CF_RUN_LOOP_RUN_FINISHED {
                // No source on the main run loop yet: avoid a busy loop.
                std::thread::sleep(Duration::from_secs_f64(RUN_LOOP_SLICE));
            }
        }
    }

    /// Outcome of the checks, printed in a libtest-like format.
    #[derive(Default)]
    struct Report {
        failed: Vec<&'static str>,
        passed: usize,
    }

    impl Report {
        fn check(&mut self, name: &'static str, ok: bool, detail: impl FnOnce() -> String) {
            if ok {
                println!("test macos_lock::{name} ... ok");
                self.passed += 1;
            } else {
                println!("test macos_lock::{name} ... FAILED");
                eprintln!("  {name}: {}", detail());
                self.failed.push(name);
            }
        }

        /// Prints the summary and returns `true` if every check passed.
        fn finish(self) -> bool {
            let failed = self.failed.len();
            println!();
            if failed > 0 {
                println!("failures:");
                for name in &self.failed {
                    println!("    macos_lock::{name}");
                }
                println!();
            }
            println!(
                "test result: {}. {} passed; {failed} failed",
                if failed == 0 { "ok" } else { "FAILED" },
                self.passed
            );
            failed == 0
        }
    }

    pub fn run() {
        println!("running 4 checks");
        let mut report = Report::default();

        let (tx, rx) = mpsc::channel();
        let listened = powermonitor::listen(move |evt| {
            let _ = tx.send(evt);
        });
        report.check("listen_returns_ok", listened.is_ok(), || {
            format!("listen() returned {listened:?}")
        });

        post_distributed(c"com.apple.screenIsLocked");
        let locked = wait_on_main_run_loop(&rx, PowerEvent::LockScreen, EVENT_TIMEOUT);
        report.check("lock_screen_received", locked, || {
            format!("no LockScreen within {EVENT_TIMEOUT:?} after com.apple.screenIsLocked")
        });

        post_distributed(c"com.apple.screenIsUnlocked");
        let unlocked = wait_on_main_run_loop(&rx, PowerEvent::UnlockScreen, EVENT_TIMEOUT);
        report.check("unlock_screen_received", unlocked, || {
            format!("no UnlockScreen within {EVENT_TIMEOUT:?} after com.apple.screenIsUnlocked")
        });

        let state = powermonitor::system_idle_state(u64::MAX);
        report.check("idle_state_is_active", state == IdleState::Active, || {
            format!(
                "system_idle_state(u64::MAX) = {state:?} (system_idle_time() = {})",
                powermonitor::system_idle_time()
            )
        });

        if !report.finish() {
            std::process::exit(1);
        }
    }
}
