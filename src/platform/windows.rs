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
use std::thread;

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

use crate::platform::{dispatch, Callbacks};
use crate::{Error, PowerEvent};

static CALLBACKS: Callbacks = std::sync::Mutex::new(Vec::new());
static STARTED: AtomicBool = AtomicBool::new(false);
static ON_BATTERY: AtomicBool = AtomicBool::new(false);

pub fn listen(cb: Box<dyn Fn(PowerEvent) + Send>) -> Result<(), Error> {
    CALLBACKS.lock().unwrap().push(cb);
    if !STARTED.swap(true, Ordering::SeqCst) {
        thread::spawn(run_listener);
    }
    Ok(())
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_POWERBROADCAST => {
            match wparam.0 as u32 {
                PBT_APMSUSPEND => dispatch(PowerEvent::Suspend, &CALLBACKS),
                PBT_APMRESUMESUSPEND => dispatch(PowerEvent::Resume, &CALLBACKS),
                PBT_APMPOWERSTATUSCHANGE => {
                    let on_battery = is_on_battery_power();
                    if on_battery != ON_BATTERY.swap(on_battery, Ordering::SeqCst) {
                        let evt = if on_battery { PowerEvent::OnBattery } else { PowerEvent::OnAc };
                        dispatch(evt, &CALLBACKS);
                    }
                }
                _ => {}
            }
            LRESULT(1)
        }
        WM_WTSSESSION_CHANGE => {
            match wparam.0 as u32 {
                WTS_SESSION_LOCK => dispatch(PowerEvent::LockScreen, &CALLBACKS),
                WTS_SESSION_UNLOCK => dispatch(PowerEvent::UnlockScreen, &CALLBACKS),
                _ => {}
            }
            LRESULT(0)
        }
        WM_QUERYENDSESSION => {
            dispatch(PowerEvent::Shutdown, &CALLBACKS);
            LRESULT(1) // allows the session to end
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

fn run_listener() {
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

pub fn system_idle_time() -> u64 {
    let mut info = LASTINPUTINFO {
        cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
        dwTime: 0,
    };
    unsafe {
        if GetLastInputInfo(&mut info).as_bool() {
            GetTickCount().wrapping_sub(info.dwTime) as u64 / 1000
        } else {
            0
        }
    }
}
