//! Linux without `delay_suspend`: the backend never takes a logind
//! inhibitor.

#![cfg(target_os = "linux")]

mod common;

use std::time::Duration;

use common::{EVENT_TIMEOUT, EventLog, MockLogind, MockUPower, setup_buses};
use powermonitor::PowerEvent;

#[test]
fn linux_delay_suspend_off() {
    let Some(buses) = setup_buses() else { return };
    let logind = MockLogind::start(&buses);
    let _upower = MockUPower::start(&buses, false);

    let log = EventLog::new();
    powermonitor::listen(log.callback()).expect("listen() with logind and UPower available");

    // A full sleep cycle proves the backend is up and handles logind.
    logind.emit_prepare_for_sleep(true);
    assert!(log.wait_for(PowerEvent::Suspend, EVENT_TIMEOUT), "no Suspend");
    logind.emit_prepare_for_sleep(false);
    assert!(log.wait_for(PowerEvent::Resume, EVENT_TIMEOUT), "no Resume");

    // Leave time for a late Inhibit call to show up.
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(
        logind.inhibit_calls(),
        Vec::new(),
        "Inhibit was called without delay_suspend"
    );
}
