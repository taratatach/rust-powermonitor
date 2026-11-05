//! Linux `delay_suspend` end to end: the backend holds a logind `delay`
//! inhibitor, keeps it while the Suspend callbacks run, and takes a new
//! one after Resume.

#![cfg(target_os = "linux")]

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use common::{EVENT_TIMEOUT, EventLog, MockLogind, MockUPower, Steps, setup_buses};
use powermonitor::{Options, PowerEvent};

/// How long the Suspend callback runs.
const SLOW_CALLBACK: Duration = Duration::from_millis(500);

#[test]
fn linux_delay_suspend() {
    let Some(buses) = setup_buses() else { return };
    let logind = MockLogind::start(&buses);
    let _upower = MockUPower::start(&buses, false);

    let (started_tx, started_rx) = mpsc::channel();
    let finished = Arc::new(AtomicBool::new(false));
    let log = EventLog::new();
    let mut opts = Options::default();
    opts.delay_suspend = true;
    {
        let finished = finished.clone();
        let record = log.callback();
        powermonitor::listen_with(opts, move |evt| {
            if evt == PowerEvent::Suspend {
                let _ = started_tx.send(());
                std::thread::sleep(SLOW_CALLBACK);
                finished.store(true, Ordering::SeqCst);
            }
            record(evt);
        })
        .expect("listen_with(delay_suspend) with logind and UPower available");
    }

    let mut steps = Steps::new();

    steps.run("inhibit_taken_at_start", || {
        assert!(
            logind.wait_for_inhibits(1, EVENT_TIMEOUT),
            "logind received no Inhibit call"
        );
        let call = &logind.inhibit_calls()[0];
        assert_eq!(call.what, "sleep");
        assert_eq!(call.who, "powermonitor");
        assert_eq!(call.mode, "delay");
    });

    steps.run("inhibitor_held_until_slow_suspend_callback_returns", || {
        logind.emit_prepare_for_sleep(true);
        started_rx
            .recv_timeout(EVENT_TIMEOUT)
            .expect("the Suspend callback did not start");
        assert!(
            !logind.wait_inhibitor_released(0, SLOW_CALLBACK * 3 / 5),
            "the inhibitor was released while the Suspend callback was running"
        );
        assert!(
            logind.wait_inhibitor_released(0, EVENT_TIMEOUT),
            "the inhibitor was not released after the Suspend callback"
        );
        assert!(
            finished.load(Ordering::SeqCst),
            "the inhibitor was released before the Suspend callback returned"
        );
    });

    steps.run("inhibit_taken_again_after_resume", || {
        logind.emit_prepare_for_sleep(false);
        assert!(log.wait_for(PowerEvent::Resume, EVENT_TIMEOUT), "no Resume");
        assert!(
            logind.wait_for_inhibits(2, EVENT_TIMEOUT),
            "logind received no new Inhibit call after Resume"
        );
    });

    steps.finish();
}
