//! Backend Windows.
//!
//! Dedicated thread with a hidden top-level window (required: WM_POWERBROADCAST
//! and WM_WTSSESSION_CHANGE are broadcast and are not received by HWND_MESSAGE
//! windows).
//!
//! - Suspend/Resume/AC/Battery : WM_POWERBROADCAST
//! - Lock/Unlock : WM_WTSSESSION_CHANGE (WTS session notifications)
//! - Shutdown : WM_QUERYENDSESSION
//! - Idle : GetLastInputInfo

use std::sync::atomic::{AtomicBool, Ordering};

use windows::core::w;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
use windows::Win32::System::RemoteDesktop::{
    WTSRegisterSessionNotification, NOTIFY_FOR_THIS_SESSION,
};
use windows::Win32::System::SystemInformation::GetTickCount;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, RegisterClassExW,
    TranslateMessage, CW_USEDEFAULT, MSG, PBT_APMSUSPEND, PBT_APMRESUMESUSPEND,
    PBT_APMPOWERSTATUSCHANGE, WM_POWERBROADCAST, WM_QUERYENDSESSION, WM_WTSSESSION_CHANGE,
    WNDCLASSEXW, WTS_SESSION_LOCK, WTS_SESSION_UNLOCK,
};

use crate::platform::{Callback, Ready, dispatch, register, start_once};
use crate::{Error, Options, PowerEvent};

static STARTED: AtomicBool = AtomicBool::new(false);
static ON_BATTERY: AtomicBool = AtomicBool::new(false);

/// Maps a `WM_POWERBROADCAST` event (its `wparam`) to a [`PowerEvent`].
///
/// - `suspended` is set by `PBT_APMSUSPEND` and cleared by the first resume
///   message, so that a wake reported by both `PBT_APMRESUMEAUTOMATIC` and
///   `PBT_APMRESUMESUSPEND` emits a single `Resume`;
/// - `on_battery` holds the last known power source, and `on_battery_now`
///   reads the current one (only called for `PBT_APMPOWERSTATUSCHANGE`).
// TDD red: current behaviour of `wnd_proc`, not wired into it yet.
#[cfg_attr(not(test), allow(dead_code))]
fn map_power_broadcast(
    wparam: u32,
    suspended: &AtomicBool,
    on_battery: &AtomicBool,
    on_battery_now: impl FnOnce() -> bool,
) -> Option<PowerEvent> {
    let _ = suspended;
    match wparam {
        PBT_APMSUSPEND => Some(PowerEvent::Suspend),
        PBT_APMRESUMESUSPEND => Some(PowerEvent::Resume),
        PBT_APMPOWERSTATUSCHANGE => {
            let now = on_battery_now();
            (now != on_battery.swap(now, Ordering::SeqCst)).then_some(if now {
                PowerEvent::OnBattery
            } else {
                PowerEvent::OnAc
            })
        }
        _ => None,
    }
}

/// `opts` has no effect on Windows.
pub fn listen(_opts: Options, cb: Callback) -> Result<(), Error> {
    register(cb);
    start_once(&STARTED, run_listener)
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_POWERBROADCAST => {
            match wparam.0 as u32 {
                PBT_APMSUSPEND => dispatch(PowerEvent::Suspend),
                PBT_APMRESUMESUSPEND => dispatch(PowerEvent::Resume),
                PBT_APMPOWERSTATUSCHANGE => {
                    let on_battery = is_on_battery_power();
                    if on_battery != ON_BATTERY.swap(on_battery, Ordering::SeqCst) {
                        let evt = if on_battery { PowerEvent::OnBattery } else { PowerEvent::OnAc };
                        dispatch(evt);
                    }
                }
                _ => {}
            }
            LRESULT(1)
        }
        WM_WTSSESSION_CHANGE => {
            match wparam.0 as u32 {
                WTS_SESSION_LOCK => dispatch(PowerEvent::LockScreen),
                WTS_SESSION_UNLOCK => dispatch(PowerEvent::UnlockScreen),
                _ => {}
            }
            LRESULT(0)
        }
        WM_QUERYENDSESSION => {
            dispatch(PowerEvent::Shutdown);
            LRESULT(1) // allows the session to end
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

fn run_listener(ready: Ready) {
    // TODO: report the window creation outcome instead.
    let _ = ready.send(Ok(()));
    unsafe {
        let instance = HINSTANCE::default();
        let class = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            hInstance: instance,
            lpszClassName: w!("powermonitor"),
            lpfnWndProc: Some(wnd_proc),
            ..Default::default()
        };
        if RegisterClassExW(&class) == 0 {
            eprintln!("powermonitor: RegisterClassExW failed");
            return;
        }
        let hwnd = CreateWindowExW(
            Default::default(),
            w!("powermonitor"),
            w!("powermonitor"),
            Default::default(),
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            None,
            None,
            instance,
            None,
        );
        if let Err(e) = hwnd {
            eprintln!("powermonitor: CreateWindowExW failed: {e}");
            return;
        }
        let hwnd = hwnd.unwrap();
        let _ = WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION);
        ON_BATTERY.store(is_on_battery_power(), Ordering::SeqCst);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

pub fn is_on_battery_power() -> bool {
    let mut status = SYSTEM_POWER_STATUS::default();
    unsafe {
        if GetSystemPowerStatus(&mut status).is_ok() {
            // ACLineStatus: 0 = battery, 1 = AC, 255 = unknown (assume AC)
            status.ACLineStatus == 0
        } else {
            false
        }
    }
}

pub fn system_idle_time() -> Option<u64> {
    let mut info = LASTINPUTINFO {
        cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
        dwTime: 0,
    };
    unsafe {
        if GetLastInputInfo(&mut info).as_bool() {
            Some(GetTickCount().wrapping_sub(info.dwTime) as u64 / 1000)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::WindowsAndMessaging::PBT_APMRESUMEAUTOMATIC;

    /// Power source reader for messages that must not read it.
    fn no_power_read() -> bool {
        panic!("only PBT_APMPOWERSTATUSCHANGE may read the power source")
    }

    #[test]
    fn power_broadcast_suspend_emits_suspend() {
        let suspended = AtomicBool::new(false);
        let on_battery = AtomicBool::new(false);
        assert_eq!(
            map_power_broadcast(PBT_APMSUSPEND, &suspended, &on_battery, no_power_read),
            Some(PowerEvent::Suspend)
        );
    }

    #[test]
    fn power_broadcast_resume_automatic_alone_emits_resume() {
        // Wake from a timer or wake-on-LAN without user input: Windows only
        // sends PBT_APMRESUMEAUTOMATIC.
        let suspended = AtomicBool::new(false);
        let on_battery = AtomicBool::new(false);
        map_power_broadcast(PBT_APMSUSPEND, &suspended, &on_battery, no_power_read);
        assert_eq!(
            map_power_broadcast(
                PBT_APMRESUMEAUTOMATIC,
                &suspended,
                &on_battery,
                no_power_read
            ),
            Some(PowerEvent::Resume)
        );
    }

    #[test]
    fn power_broadcast_resume_automatic_then_resume_suspend_emits_one_resume() {
        // Wake by the user: PBT_APMRESUMEAUTOMATIC, then PBT_APMRESUMESUSPEND.
        // Resume is emitted once, on the first of them.
        let suspended = AtomicBool::new(false);
        let on_battery = AtomicBool::new(false);
        let map = |wparam| map_power_broadcast(wparam, &suspended, &on_battery, no_power_read);

        assert_eq!(map(PBT_APMSUSPEND), Some(PowerEvent::Suspend));
        assert_eq!(map(PBT_APMRESUMEAUTOMATIC), Some(PowerEvent::Resume));
        assert_eq!(map(PBT_APMRESUMESUSPEND), None);

        // The next suspend arms the resume again.
        assert_eq!(map(PBT_APMSUSPEND), Some(PowerEvent::Suspend));
        assert_eq!(map(PBT_APMRESUMEAUTOMATIC), Some(PowerEvent::Resume));
        assert_eq!(map(PBT_APMRESUMESUSPEND), None);
    }

    #[test]
    fn power_broadcast_power_status_change_emits_on_source_change_only() {
        let suspended = AtomicBool::new(false);
        let on_battery = AtomicBool::new(false);
        let map = |now: bool| {
            map_power_broadcast(PBT_APMPOWERSTATUSCHANGE, &suspended, &on_battery, || now)
        };

        assert_eq!(map(true), Some(PowerEvent::OnBattery));
        assert!(on_battery.load(Ordering::SeqCst));
        // Same source (e.g. a battery level change): no event.
        assert_eq!(map(true), None);
        assert_eq!(map(false), Some(PowerEvent::OnAc));
        assert_eq!(map(false), None);
        assert!(!on_battery.load(Ordering::SeqCst));
    }
}
