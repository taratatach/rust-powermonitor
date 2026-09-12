//! Backend macOS.
//!
//! - Suspend/Resume : IOKit `IORegisterForSystemPower` (dedicated CFRunLoop thread)
//! - AC/Battery : `IOPSNotificationCreateRunLoopSource` + `IOPSCopyPowerSourcesInfo`
//! - Lock/Unlock : `CFNotificationCenterGetDistributedCenter` —
//!   `com.apple.screenIsLocked` / `com.apple.screenIsUnlocked`
//! - Idle : `CGEventSourceSecondsSinceLastEventType`
//!
//! FFI declared by hand: no need for an IOKit/obj-c crate, all APIs used
//! are C.

use std::cell::Cell;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use crate::platform::{dispatch, Callbacks};
use crate::{Error, PowerEvent};

static CALLBACKS: Callbacks = std::sync::Mutex::new(Vec::new());
static STARTED: AtomicBool = AtomicBool::new(false);

pub fn listen(cb: Box<dyn Fn(PowerEvent) + Send>) -> Result<(), Error> {
    CALLBACKS.lock().unwrap().push(cb);
    if !STARTED.swap(true, Ordering::SeqCst) {
        thread::spawn(run_listener);
    }
    Ok(())
}

/// IOMessage codes pertinents (voir IOKit/IOMessage.h).
const K_IO_MESSAGE_SYSTEM_WILL_SLEEP: u32 = 0x0280_0000;
const K_IO_MESSAGE_SYSTEM_HAS_POWERED_ON: u32 = 0x0280_0300;

const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;

type CFTypeRef = *mut c_void;
type CFStringRef = *const c_void;
type CFRunLoopRef = *mut c_void;
type CFRunLoopSourceRef = *mut c_void;
type CFArrayRef = *const c_void;
type CFDictionaryRef = *const c_void;
type CFNotificationCenterRef = *mut c_void;

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IORegisterForSystemPower(
        refcon: *mut c_void,
        port: *mut *mut c_void,
        callback: extern "C" fn(*mut c_void, u32, u32, usize),
        notifier: *mut *mut c_void,
    ) -> u32;
    fn IOAllowPowerChange(kernel_port: u32, notification_id: usize) -> u32;
    fn IONotificationPortGetRunLoopSource(port: *mut c_void) -> CFRunLoopSourceRef;

    fn IOPSNotificationCreateRunLoopSource(
        callback: extern "C" fn(*mut c_void),
        context: *mut c_void,
    ) -> CFRunLoopSourceRef;
    fn IOPSCopyPowerSourcesInfo() -> CFTypeRef;
    fn IOPSCopyPowerSourcesList(blob: CFTypeRef) -> CFArrayRef;
    fn IOPSGetPowerSourceDescription(blob: CFTypeRef, ps: *mut c_void) -> CFDictionaryRef;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFRunLoopCommonModes: CFStringRef;

    fn CFRunLoopGetCurrent() -> CFRunLoopRef;
    fn CFRunLoopAddSource(rl: CFRunLoopRef, source: CFRunLoopSourceRef, mode: CFStringRef);
    fn CFRunLoopRun();

    fn CFRelease(cf: CFTypeRef);
    fn CFArrayGetCount(array: CFArrayRef) -> isize;
    fn CFArrayGetValueAtIndex(array: CFArrayRef, idx: isize) -> *mut c_void;
    fn CFDictionaryGetValue(dict: CFDictionaryRef, key: *const c_void) -> *const c_void;
    fn CFStringCreateWithCString(
        alloc: *const c_void,
        c_str: *const i8,
        encoding: u32,
    ) -> CFStringRef;
    fn CFStringCompare(the_string: CFStringRef, the_string2: CFStringRef, options: usize)
        -> isize;

    fn CFNotificationCenterGetDistributedCenter() -> CFNotificationCenterRef;
    fn CFNotificationCenterAddObserver(
        center: CFNotificationCenterRef,
        observer: *const c_void,
        callback: extern "C" fn(CFNotificationCenterRef, *const c_void, CFStringRef, *const c_void, CFDictionaryRef),
        name: CFStringRef,
        object: *const c_void,
        suspension_behaviour: isize,
    );
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGEventSourceSecondsSinceLastEventType(state: u32, event_type: u32) -> f64;
}

struct Ctx {
    conn: Cell<u32>,
    on_battery: AtomicBool,
}

extern "C" fn system_power_callback(refcon: *mut c_void, _service: u32, message_type: u32, message_arg: usize) {
    let ctx = unsafe { &*(refcon as *const Ctx) };
    match message_type {
        K_IO_MESSAGE_SYSTEM_WILL_SLEEP => {
            dispatch(PowerEvent::Suspend, &CALLBACKS);
            // Required: without this, sleep blocks indefinitely.
            unsafe { IOAllowPowerChange(ctx.conn.get(), message_arg) };
        }
        K_IO_MESSAGE_SYSTEM_HAS_POWERED_ON => dispatch(PowerEvent::Resume, &CALLBACKS),
        _ => {}
    }
}

extern "C" fn power_source_callback(refcon: *mut c_void) {
    let ctx = unsafe { &*(refcon as *const Ctx) };
    let on_battery = current_on_battery();
    if on_battery != ctx.on_battery.swap(on_battery, Ordering::SeqCst) {
        let evt = if on_battery { PowerEvent::OnBattery } else { PowerEvent::OnAc };
        dispatch(evt, &CALLBACKS);
    }
}

extern "C" fn screen_locked_callback(_center: CFNotificationCenterRef, _observer: *const c_void, _name: CFStringRef, _object: *const c_void, _info: CFDictionaryRef) {
    dispatch(PowerEvent::LockScreen, &CALLBACKS);
}

extern "C" fn screen_unlocked_callback(_center: CFNotificationCenterRef, _observer: *const c_void, _name: CFStringRef, _object: *const c_void, _info: CFDictionaryRef) {
    dispatch(PowerEvent::UnlockScreen, &CALLBACKS);
}

/// Current snapshot: `true` = on battery (no source of type "AC Power",
/// and at least one declared source). Desktop with no source → AC power.
fn current_on_battery() -> bool {
    unsafe {
        let info = IOPSCopyPowerSourcesInfo();
        if info.is_null() {
            return false;
        }
        let mut on_ac = false;
        let mut has_source = false;
        let list = IOPSCopyPowerSourcesList(info);
        if !list.is_null() {
            let type_key = CFStringCreateWithCString(std::ptr::null(), c"Type".as_ptr(), K_CF_STRING_ENCODING_UTF8);
            let ac = CFStringCreateWithCString(std::ptr::null(), c"AC Power".as_ptr(), K_CF_STRING_ENCODING_UTF8);
            for i in 0..CFArrayGetCount(list) {
                has_source = true;
                let desc = IOPSGetPowerSourceDescription(info, CFArrayGetValueAtIndex(list, i));
                if desc.is_null() {
                    continue;
                }
                let ty = CFDictionaryGetValue(desc, type_key as *const c_void);
                if !ty.is_null() && CFStringCompare(ty as CFStringRef, ac, 0) == 0 {
                    on_ac = true;
                }
            }
            CFRelease(type_key as CFTypeRef);
            CFRelease(ac as CFTypeRef);
            CFRelease(list as CFTypeRef);
        }
        CFRelease(info);
        has_source && !on_ac
    }
}

fn run_listener() {
    let ctx = Box::new(Ctx {
        conn: Cell::new(0),
        on_battery: AtomicBool::new(current_on_battery()),
    });
    let ctx = Box::into_raw(ctx);
    unsafe {
        // Suspend / Resume.
        let mut port: *mut c_void = std::ptr::null_mut();
        let mut notifier: *mut c_void = std::ptr::null_mut();
        let conn = IORegisterForSystemPower(ctx.cast(), &mut port, system_power_callback, &mut notifier);
        if conn != 0 {
            (*ctx).conn.set(conn);
            let source = IONotificationPortGetRunLoopSource(port);
            if !source.is_null() {
                CFRunLoopAddSource(CFRunLoopGetCurrent(), source, kCFRunLoopCommonModes);
            }
        }

        // AC / Battery.
        let source = IOPSNotificationCreateRunLoopSource(power_source_callback, ctx.cast());
        if !source.is_null() {
            CFRunLoopAddSource(CFRunLoopGetCurrent(), source, kCFRunLoopCommonModes);
        }

        // Lock / Unlock screen.
        let center = CFNotificationCenterGetDistributedCenter();
        if !center.is_null() {
            for (name, cb) in [
                ("com.apple.screenIsLocked", screen_locked_callback as extern "C" fn(CFNotificationCenterRef, *const c_void, CFStringRef, *const c_void, CFDictionaryRef)),
                ("com.apple.screenIsUnlocked", screen_unlocked_callback as _),
            ] {
                let cf_name = CFStringCreateWithCString(std::ptr::null(), format!("{name}\0").as_ptr().cast(), K_CF_STRING_ENCODING_UTF8);
                // DeliverImmediately = 0 (default)
                CFNotificationCenterAddObserver(center, std::ptr::null(), cb, cf_name, std::ptr::null(), 0);
                CFRelease(cf_name as CFTypeRef);
            }
        }

        CFRunLoopRun();
    }
}

pub fn is_on_battery_power() -> bool {
    current_on_battery()
}

pub fn system_idle_time() -> u64 {
    // kCGEventSourceStateCombinedSessionState = 1, kCGAnyInputEventType = ~0
    let secs = unsafe { CGEventSourceSecondsSinceLastEventType(1, u32::MAX) };
    if secs < 0.0 { 0 } else { secs as u64 }
}
