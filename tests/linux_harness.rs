//! Smoke test of the e2e harness: both private buses start and the mocks
//! answer on the bus they belong to.

#![cfg(target_os = "linux")]

mod common;

use common::{
    FDO_SCREENSAVER_NAME, FDO_SCREENSAVER_PATH, MockFdoScreenSaver, MockUPower, UPOWER_NAME,
    UPOWER_PATH, setup_buses,
};
use zbus::blocking::Proxy;
use zbus::blocking::fdo::PropertiesProxy;
use zbus::names::InterfaceName;

#[test]
fn buses_start_and_mocks_answer() {
    let Some(buses) = setup_buses() else { return };
    assert_eq!(
        std::env::var("DBUS_SYSTEM_BUS_ADDRESS").as_deref(),
        Ok(buses.system.address())
    );
    assert_eq!(
        std::env::var("DBUS_SESSION_BUS_ADDRESS").as_deref(),
        Ok(buses.session.address())
    );
    assert_ne!(buses.system.address(), buses.session.address());

    // System bus: UPower `OnBattery`, read through a fresh connection.
    let upower = MockUPower::start(&buses, true);
    let conn = zbus::blocking::Connection::system().expect("connect to the system test bus");
    let props = PropertiesProxy::builder(&conn)
        .destination(UPOWER_NAME)
        .unwrap()
        .path(UPOWER_PATH)
        .unwrap()
        .build()
        .expect("UPower properties proxy");
    let iface = InterfaceName::from_static_str_unchecked(UPOWER_NAME);
    let on_battery = props
        .get(iface.clone(), "OnBattery")
        .expect("Get OnBattery");
    assert_eq!(bool::try_from(on_battery).ok(), Some(true));
    upower.set_on_battery(false);
    let on_battery = props.get(iface, "OnBattery").expect("Get OnBattery");
    assert_eq!(bool::try_from(on_battery).ok(), Some(false));

    // Session bus: freedesktop screen saver idle time.
    let _screensaver = MockFdoScreenSaver::start(&buses, 1234);
    let conn = zbus::blocking::Connection::session().expect("connect to the session test bus");
    let proxy = Proxy::new(
        &conn,
        FDO_SCREENSAVER_NAME,
        FDO_SCREENSAVER_PATH,
        FDO_SCREENSAVER_NAME,
    )
    .expect("screen saver proxy");
    let idle_ms: u32 = proxy
        .call("GetSessionIdleTime", &())
        .expect("GetSessionIdleTime");
    assert_eq!(idle_ms, 1234);
}
