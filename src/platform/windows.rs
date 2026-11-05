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
