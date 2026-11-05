//! End-to-end scenarios for the Windows backend: window messages are sent
//! straight to the hidden window of the backend, as Windows would
//! broadcast them, and the events seen by a `listen` callback are checked.
//!
//! The backend state is process-global, so every test shares one `listen`
//! callback and holds the [`events`] lock for its whole run: the tests of
//! this binary run one at a time. `SendMessageW` returns once the window
//! procedure has handled the message, so its events are already recorded
//! when it returns.

#![cfg(target_os = "windows")]

use std::sync::mpsc::{self, Receiver};
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use powermonitor::PowerEvent;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_MOVE, MOUSEINPUT, SendInput,
};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowExW, GetWindowThreadProcessId, PBT_APMRESUMEAUTOMATIC, PBT_APMRESUMESUSPEND,
    PBT_APMSUSPEND, SendMessageW, WM_POWERBROADCAST, WM_QUERYENDSESSION, WM_WTSSESSION_CHANGE,
    WTS_SESSION_LOCK, WTS_SESSION_UNLOCK,
};
use windows::core::{PCWSTR, w};

/// How long to wait for an expected event, or for the backend window.
const TIMEOUT: Duration = Duration::from_secs(5);
/// How long to wait before concluding that no further event comes.
const QUIET: Duration = Duration::from_millis(500);

/// Events received by the shared `listen` callback. The first call starts
/// the backend; the guard serializes the tests.
fn events() -> MutexGuard<'static, Receiver<PowerEvent>> {
    static EVENTS: OnceLock<Mutex<Receiver<PowerEvent>>> = OnceLock::new();
    let events = EVENTS.get_or_init(|| {
        let (tx, rx) = mpsc::channel();
        powermonitor::listen(move |evt| {
            let _ = tx.send(evt);
        })
        .expect("listen() failed");
        Mutex::new(rx)
    });
    let rx = events.lock().unwrap_or_else(PoisonError::into_inner);
    // Drop what earlier tests left behind.
    while rx.try_recv().is_ok() {}
    rx
}

/// The hidden top-level window of the backend in this process. Its class
/// name duplicates the backend's `CLASS_NAME`.
fn backend_window() -> HWND {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        // Walk the top-level windows of that class: another process may
        // own one too.
        // A null parent and a null "child after" start from the first
        // top-level window.
        let mut after = HWND::default();
        loop {
            // SAFETY: plain Win32 call with a static class name.
            let found = unsafe {
                FindWindowExW(HWND::default(), after, w!("powermonitor"), PCWSTR::null())
            };
            let Ok(hwnd) = found else { break };
            let mut pid = 0u32;
            // SAFETY: `hwnd` comes from `FindWindowExW`, `pid` outlives the call.
            unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
            if pid == std::process::id() {
                return hwnd;
            }
            after = hwnd;
        }
        assert!(
            Instant::now() < deadline,
            "no \"powermonitor\" window in this process after {TIMEOUT:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Sends `msg` to the backend window and waits until it is handled.
fn send(msg: u32, wparam: u32, lparam: isize) {
    let hwnd = backend_window();
    // SAFETY: `hwnd` is a live window of this process.
    unsafe { SendMessageW(hwnd, msg, WPARAM(wparam as usize), LPARAM(lparam)) };
}

fn expect_event(rx: &Receiver<PowerEvent>, expected: PowerEvent, after: &str) {
    assert_eq!(
        rx.recv_timeout(TIMEOUT).ok(),
        Some(expected),
        "expected {expected:?} after {after}"
    );
}

fn expect_no_event(rx: &Receiver<PowerEvent>, after: &str) {
    if let Ok(evt) = rx.recv_timeout(QUIET) {
        panic!("unexpected {evt:?} after {after}");
    }
}

#[test]
fn suspend_emits_suspend() {
    let rx = events();
    send(WM_POWERBROADCAST, PBT_APMSUSPEND, 0);
    expect_event(&rx, PowerEvent::Suspend, "PBT_APMSUSPEND");
    expect_no_event(&rx, "PBT_APMSUSPEND");
}

#[test]
fn resume_automatic_then_resume_suspend_emits_one_resume() {
    let rx = events();
    send(WM_POWERBROADCAST, PBT_APMSUSPEND, 0);
    expect_event(&rx, PowerEvent::Suspend, "PBT_APMSUSPEND");

    // Windows sends PBT_APMRESUMEAUTOMATIC on every wake: Resume must not
    // wait for PBT_APMRESUMESUSPEND, which only follows user input.
    send(WM_POWERBROADCAST, PBT_APMRESUMEAUTOMATIC, 0);
    expect_event(&rx, PowerEvent::Resume, "PBT_APMRESUMEAUTOMATIC");

    send(WM_POWERBROADCAST, PBT_APMRESUMESUSPEND, 0);
    expect_no_event(&rx, "PBT_APMRESUMEAUTOMATIC + PBT_APMRESUMESUSPEND");
}

#[test]
fn session_lock_unlock_emit_lock_and_unlock_screen() {
    let rx = events();
    send(WM_WTSSESSION_CHANGE, WTS_SESSION_LOCK, 0);
    expect_event(&rx, PowerEvent::LockScreen, "WTS_SESSION_LOCK");
    send(WM_WTSSESSION_CHANGE, WTS_SESSION_UNLOCK, 0);
    expect_event(&rx, PowerEvent::UnlockScreen, "WTS_SESSION_UNLOCK");
    expect_no_event(&rx, "WTS_SESSION_UNLOCK");
}

#[test]
fn query_end_session_emits_shutdown() {
    let rx = events();
    // lParam 0: shutdown or restart (no ENDSESSION_* flag).
    send(WM_QUERYENDSESSION, 0, 0);
    expect_event(&rx, PowerEvent::Shutdown, "WM_QUERYENDSESSION");
    expect_no_event(&rx, "WM_QUERYENDSESSION");
}

#[test]
fn mouse_input_resets_idle_time() {
    // Let the last input age a little, so that a stale reading cannot pass.
    std::thread::sleep(Duration::from_secs(2));

    let mouse_move = |dx| INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy: 0,
                mouseData: 0,
                dwFlags: MOUSEEVENTF_MOVE,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    let inputs = [mouse_move(1), mouse_move(-1)];
    // SAFETY: `inputs` is a valid array of `INPUT` of the given size.
    let sent = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
    assert_eq!(
        sent as usize,
        inputs.len(),
        "SendInput injected {sent} of {} events (no interactive desktop?)",
        inputs.len()
    );

    let deadline = Instant::now() + TIMEOUT;
    let mut idle = powermonitor::system_idle_time();
    while idle > 1 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
        idle = powermonitor::system_idle_time();
    }
    assert!(idle <= 1, "system_idle_time() is {idle} s after SendInput");
}
