//! Backend Linux (D-Bus, best effort).
//!
//! - Suspend/Resume : logind `org.freedesktop.login1.Manager.PrepareForSleep`
//! - AC/Battery : UPower `PropertiesChanged` sur `OnBattery`
//! - Lock/Unlock : `org.freedesktop.ScreenSaver.ActiveChanged` (GNOME/KDE;
//!   absent on some compositors → never emitted, documented)
//! - Idle : `org.gnome.Mutter.IdleMonitor.GetIdletime` (GNOME), fallback
//!   `org.freedesktop.ScreenSaver.GetSessionIdleTime` (KDE), otherwise 0
//!
//! A single thread with a mini futures executor: no runtime imposed on
//! the user.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};

use futures::executor::block_on;
use futures::stream::{select_all, StreamExt};
use zbus::zvariant::Value;
use zbus::{Connection, MatchRule, MessageStream};

use crate::platform::{Callback, Ready, dispatch, register, start_once};
use crate::{Error, Options, PowerEvent};

static STARTED: AtomicBool = AtomicBool::new(false);
static ON_BATTERY: AtomicBool = AtomicBool::new(false);

pub fn listen(_opts: Options, cb: Callback) -> Result<(), Error> {
    register(cb);
    start_once(&STARTED, run_listener)
}

fn match_rule(interface: &'static str, member: &'static str) -> MatchRule<'static> {
    MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface(interface)
        .expect("valid D-Bus interface")
        .member(member)
        .expect("valid D-Bus member")
        .build()
}

fn run_listener(ready: Ready) {
    // TODO: report the bus connection and subscription outcome instead.
    let _ = ready.send(Ok(()));
    block_on(async {
        let conn = match Connection::system().await {
            Ok(c) => c,
            Err(e) => {
                eprintln!("powermonitor: cannot connect to the system bus: {e}");
                return;
            }
        };
        ON_BATTERY.store(query_on_battery(&conn).await, Ordering::SeqCst);

        let suspend = match_rule("org.freedesktop.login1.Manager", "PrepareForSleep");
        let upower = match_rule("org.freedesktop.DBus.Properties", "PropertiesChanged");
        let screensaver = match_rule("org.freedesktop.ScreenSaver", "ActiveChanged");

        let mut streams = Vec::new();
        for rule in [suspend, upower, screensaver] {
            match MessageStream::for_match_rule(rule, &conn, None).await {
                Ok(s) => streams.push(s),
                Err(e) => eprintln!("powermonitor: match rule ignored: {e}"),
            }
        }
        if streams.is_empty() {
            return;
        }

        let mut merged = select_all(streams);
        while let Some(msg) = merged.next().await {
            let Ok(msg) = msg else { continue };
            let header = msg.header();
            let member = header.member().map(|m| m.as_str());
            match member {
                Some("PrepareForSleep") => {
                    if let Ok(sleeping) = msg.body().deserialize::<bool>() {
                        let evt = if sleeping { PowerEvent::Suspend } else { PowerEvent::Resume };
                        dispatch(evt);
                    }
                }
                Some("PropertiesChanged") => {
                    let body = msg.body();
                    let Ok((iface, changed, _)) =
                        body.deserialize::<(String, HashMap<String, Value>, Vec<String>)>()
                    else {
                        continue;
                    };
                    if iface != "org.freedesktop.UPower" {
                        continue;
                    }
                    if let Some(Value::Bool(on_battery)) = changed.get("OnBattery")
                        && *on_battery != ON_BATTERY.swap(*on_battery, Ordering::SeqCst)
                    {
                        let evt = if *on_battery { PowerEvent::OnBattery } else { PowerEvent::OnAc };
                        dispatch(evt);
                    }
                }
                Some("ActiveChanged") => {
                    if let Ok(locked) = msg.body().deserialize::<bool>() {
                        let evt = if locked { PowerEvent::LockScreen } else { PowerEvent::UnlockScreen };
                        dispatch(evt);
                    }
                }
                _ => {}
            }
        }
    })
}

async fn query_on_battery(conn: &Connection) -> bool {
    let proxy = zbus::Proxy::new(
        conn,
        "org.freedesktop.UPower",
        "/org/freedesktop/UPower",
        "org.freedesktop.DBus.Properties",
    )
    .await;
    let Ok(proxy) = proxy else { return false };
    proxy.get_property::<bool>("OnBattery").await.unwrap_or(false)
}

pub fn is_on_battery_power() -> bool {
    block_on(async {
        match Connection::system().await {
            Ok(conn) => query_on_battery(&conn).await,
            Err(_) => false,
        }
    })
}

pub fn system_idle_time() -> Option<u64> {
    block_on(async {
        let Ok(conn) = Connection::system().await else { return None };
        // GNOME: milliseconds.
        if let Ok(proxy) = zbus::Proxy::new(
            &conn,
            "org.gnome.Mutter.IdleMonitor",
            "/org/gnome/Mutter/IdleMonitor/Core",
            "org.gnome.Mutter.IdleMonitor",
        )
        .await
            && let Ok(ms) = proxy.call::<_, _, u64>("GetIdletime", &()).await
        {
            return Some(ms / 1000);
        }
        // KDE: seconds.
        if let Ok(proxy) = zbus::Proxy::new(
            &conn,
            "org.freedesktop.ScreenSaver",
            "/ScreenSaver",
            "org.freedesktop.ScreenSaver",
        )
        .await
            && let Ok(secs) = proxy.call::<_, _, u32>("GetSessionIdleTime", &()).await
        {
            return Some(secs as u64);
        }
        None
    })
}
