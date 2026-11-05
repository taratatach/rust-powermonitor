//! Linux events end to end: logind, UPower and both screen savers are
//! mocked on private buses, and each step checks the events a callback
//! receives.

#![cfg(target_os = "linux")]

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use common::{
    EVENT_TIMEOUT, EventLog, LOGIND_PATH, MockFdoScreenSaver, MockGnomeScreenSaver, MockLogind,
    MockUPower, Steps, setup_buses, wait_until,
};
use powermonitor::PowerEvent;

#[test]
fn linux_events() {
    let Some(buses) = setup_buses() else { return };
    let logind = MockLogind::start(&buses);
    // Started on battery: switching to AC must emit `OnAc`.
    let upower = MockUPower::start(&buses, true);
    let gnome = MockGnomeScreenSaver::start(&buses);
    let kde = MockFdoScreenSaver::start(&buses, 0);

    let log = EventLog::new();
    powermonitor::listen(log.callback()).expect("listen() with logind and UPower available");

    let mut steps = Steps::new();

    steps.run("prepare_for_sleep_maps_to_suspend_resume", || {
        log.drain();
        logind.emit_prepare_for_sleep(true);
        assert!(log.wait_for(PowerEvent::Suspend, EVENT_TIMEOUT), "no Suspend");
        logind.emit_prepare_for_sleep(false);
        assert!(log.wait_for(PowerEvent::Resume, EVENT_TIMEOUT), "no Resume");
    });

    steps.run("prepare_for_shutdown_maps_to_shutdown", || {
        log.drain();
        logind.emit_prepare_for_shutdown(true);
        assert!(log.wait_for(PowerEvent::Shutdown, EVENT_TIMEOUT), "no Shutdown");
    });

    steps.run("on_battery_false_maps_to_on_ac", || {
        log.drain();
        upower.set_on_battery(false);
        assert!(log.wait_for(PowerEvent::OnAc, EVENT_TIMEOUT), "no OnAc");
    });

    steps.run("gnome_active_changed_maps_to_lock_unlock", || {
        log.drain();
        gnome.emit_active_changed(true);
        assert!(log.wait_for(PowerEvent::LockScreen, EVENT_TIMEOUT), "no LockScreen");
        gnome.emit_active_changed(false);
        assert!(log.wait_for(PowerEvent::UnlockScreen, EVENT_TIMEOUT), "no UnlockScreen");
    });

    steps.run("kde_active_changed_maps_to_lock_unlock", || {
        log.drain();
        kde.emit_active_changed(true);
        assert!(log.wait_for(PowerEvent::LockScreen, EVENT_TIMEOUT), "no LockScreen");
        kde.emit_active_changed(false);
        assert!(log.wait_for(PowerEvent::UnlockScreen, EVENT_TIMEOUT), "no UnlockScreen");
    });

    steps.run("prepare_for_sleep_from_another_sender_is_ignored", || {
        // A client that does not own `org.freedesktop.login1` emits the
        // same signal on the same path and interface.
        let spoofer = buses.system.connect().expect("connect the spoofing client");
        std::thread::sleep(Duration::from_millis(300));
        log.drain();
        spoofer
            .emit_signal(
                None::<zbus::names::BusName<'_>>,
                LOGIND_PATH,
                "org.freedesktop.login1.Manager",
                "PrepareForSleep",
                &true,
            )
            .expect("emit the spoofed PrepareForSleep");
        std::thread::sleep(Duration::from_millis(300));
        // The genuine signal that follows must be the first event seen.
        logind.emit_prepare_for_sleep(false);
        assert_eq!(
            log.next(EVENT_TIMEOUT),
            Some(PowerEvent::Resume),
            "the spoofed PrepareForSleep(true) was not ignored"
        );
    });

    steps.run("callback_may_query_and_listen", || {
        let returned = Arc::new(AtomicBool::new(false));
        let armed = Arc::new(AtomicBool::new(true));
        {
            let returned = returned.clone();
            powermonitor::listen(move |evt| {
                if evt == PowerEvent::Suspend && armed.swap(false, Ordering::SeqCst) {
                    let _ = powermonitor::is_on_battery_power();
                    let _ = powermonitor::listen(|_| {});
                    returned.store(true, Ordering::SeqCst);
                }
            })
            .expect("listen() once the backend runs");
        }
        log.drain();
        logind.emit_prepare_for_sleep(true);
        assert!(log.wait_for(PowerEvent::Suspend, EVENT_TIMEOUT), "no Suspend");
        assert!(
            wait_until(EVENT_TIMEOUT, || returned.load(Ordering::SeqCst)),
            "is_on_battery_power()/listen() did not return from a callback"
        );
        logind.emit_prepare_for_sleep(false);
        assert!(
            log.wait_for(PowerEvent::Resume, EVENT_TIMEOUT),
            "no Resume after a callback called the crate"
        );
    });

    steps.run("panicking_callback_does_not_stop_the_others", || {
        powermonitor::listen(|evt| panic!("test callback panics on {evt:?}"))
            .expect("listen() once the backend runs");
        let after = EventLog::new();
        powermonitor::listen(after.callback()).expect("listen() once the backend runs");
        log.drain();
        logind.emit_prepare_for_sleep(true);
        assert!(
            after.wait_for(PowerEvent::Suspend, EVENT_TIMEOUT),
            "the callback registered after the panicking one got no Suspend"
        );
        logind.emit_prepare_for_sleep(false);
        assert!(
            after.wait_for(PowerEvent::Resume, EVENT_TIMEOUT),
            "the monitor stopped after a callback panicked"
        );
        assert!(
            log.wait_for(PowerEvent::Resume, EVENT_TIMEOUT),
            "the first callback stopped receiving events"
        );
    });

    steps.finish();
}
