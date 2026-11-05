//! Linux start-up failure: `listen()` reports an unreachable system bus,
//! and a later call retries once the bus is reachable.

#![cfg(target_os = "linux")]

mod common;

use common::{MockLogind, MockUPower, Steps, export_bus_env, setup_buses};

/// A system bus address that nothing listens on.
const INVALID_ADDRESS: &str = "unix:path=/nonexistent/powermonitor-no-bus";

#[test]
fn linux_no_bus() {
    let Some(buses) = setup_buses() else { return };
    // SAFETY: no crate call and no mock yet: only the address reader
    // threads of the test buses exist, and they do not touch the
    // environment.
    unsafe { export_bus_env(INVALID_ADDRESS, buses.session.address()) };

    let mut steps = Steps::new();

    steps.run("listen_fails_without_system_bus", || {
        assert!(
            powermonitor::listen(|_| {}).is_err(),
            "listen() returned Ok with an unreachable system bus"
        );
    });

    // SAFETY: the failed `listen()` above has no backend thread left, and
    // the mocks are not started yet (see `export_bus_env`).
    unsafe { export_bus_env(buses.system.address(), buses.session.address()) };
    let _logind = MockLogind::start(&buses);
    let _upower = MockUPower::start(&buses, false);

    steps.run("listen_retries_once_the_bus_is_reachable", || {
        powermonitor::listen(|_| {}).expect("listen() with a reachable system bus");
    });

    steps.finish();
}
