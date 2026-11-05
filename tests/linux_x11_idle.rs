//! Linux X11 idle fallback: with no D-Bus idle source, the idle time comes
//! from `XScreenSaverQueryInfo` on a private Xvfb server.

#![cfg(target_os = "linux")]

mod common;

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use common::{setup_buses, strict, wait_until};

/// Display number of the private X server.
const DISPLAY: &str = ":99";
/// Socket the X server listens on for [`DISPLAY`].
const X_SOCKET: &str = "/tmp/.X11-unix/X99";
/// How long to wait for the X server socket.
const XVFB_START_TIMEOUT: Duration = Duration::from_secs(10);

/// A private Xvfb server, killed when dropped.
struct Xvfb(Child);

impl Drop for Xvfb {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Starts Xvfb on [`DISPLAY`] and waits for its socket. Returns `None`
/// (test skipped) when it cannot start, unless [`strict`] mode is on,
/// where it panics.
fn start_xvfb() -> Option<Xvfb> {
    let started = Command::new("Xvfb")
        .args([DISPLAY, "-nolisten", "tcp", "-screen", "0", "640x480x24"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| format!("cannot start Xvfb: {e}"))
        .and_then(|child| {
            let mut xvfb = Xvfb(child);
            let up = wait_until(XVFB_START_TIMEOUT, || {
                matches!(xvfb.0.try_wait(), Ok(None)) && Path::new(X_SOCKET).exists()
            });
            if up {
                Ok(xvfb)
            } else {
                Err(format!(
                    "Xvfb {DISPLAY} did not come up within {XVFB_START_TIMEOUT:?} \
                     (exited: {:?})",
                    xvfb.0.try_wait()
                ))
            }
        });
    match started {
        Ok(xvfb) => Some(xvfb),
        Err(e) if strict() => panic!("{e}"),
        Err(e) => {
            eprintln!("skipped: {e}");
            None
        }
    }
}

#[test]
fn linux_x11_idle() {
    // No idle mock on the session bus: Mutter and the screen saver fail.
    let Some(_buses) = setup_buses() else { return };
    let Some(_xvfb) = start_xvfb() else { return };
    // SAFETY: no crate call and no mock yet: only the address reader
    // threads of the test buses exist, and they do not touch the
    // environment.
    unsafe { std::env::set_var("DISPLAY", DISPLAY) };

    // Nobody touches the X server: its idle counter keeps growing.
    std::thread::sleep(Duration::from_secs(2));
    let idle = powermonitor::system_idle_time();
    assert!(idle >= 1, "X11 idle time is {idle} s after 2 s without input");
}
