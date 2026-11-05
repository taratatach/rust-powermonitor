//! Linux queries end to end: `is_on_battery_power` reads UPower on the
//! system bus, the idle time comes from Mutter, then from the freedesktop
//! screen saver (KDE), both on the session bus.

#![cfg(target_os = "linux")]

mod common;

use common::{MockFdoScreenSaver, MockMutterIdle, MockUPower, Steps, setup_buses};
use powermonitor::IdleState;

#[test]
fn linux_queries() {
    let Some(buses) = setup_buses() else { return };
    let mut steps = Steps::new();

    // Steps run in this order because a mock, once started, stays on the
    // bus: no idle source, then KDE only, then Mutter in front of KDE.
    steps.run("no_idle_source_and_no_display_is_unknown", || {
        assert_eq!(powermonitor::system_idle_time(), 0);
        assert_eq!(powermonitor::system_idle_state(1), IdleState::Unknown);
    });

    let kde = MockFdoScreenSaver::start(&buses, 7000);
    steps.run("kde_idle_time_is_milliseconds", || {
        assert_eq!(powermonitor::system_idle_time(), 7);
    });

    let mutter = MockMutterIdle::start(&buses, 5000);
    steps.run("mutter_idle_time_comes_first", || {
        assert_eq!(powermonitor::system_idle_time(), 5);
    });

    let upower = MockUPower::start(&buses, true);
    steps.run("is_on_battery_power_follows_upower", || {
        assert!(powermonitor::is_on_battery_power(), "OnBattery=true read as false");
        upower.set_on_battery(false);
        assert!(!powermonitor::is_on_battery_power(), "OnBattery=false read as true");
    });

    drop((kde, mutter));
    steps.finish();
}
