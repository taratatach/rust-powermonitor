# powermonitor

Rust crate inspired by Electron's [`powerMonitor`](https://www.electronjs.org/docs/latest/api/power-monitor)
module: system power events (suspend/resume, AC/battery,
lock/unlock-screen, shutdown) and idle queries.

Usable in any Rust app, including the backend of a Tauri app.

## Usage

```rust
use powermonitor::{listen, is_on_battery_power, system_idle_time, system_idle_state, PowerEvent};

fn main() {
    listen(|evt| match evt {
        PowerEvent::Suspend => println!("suspending"),
        PowerEvent::Resume => println!("resumed"),
        PowerEvent::OnBattery => println!("on battery"),
        PowerEvent::OnAc => println!("on AC power"),
        PowerEvent::LockScreen => println!("screen locked"),
        PowerEvent::UnlockScreen => println!("screen unlocked"),
        PowerEvent::Shutdown => println!("imminent shutdown"),
    }).unwrap();

    println!("battery: {}", is_on_battery_power());
    println!("idle: {}s", system_idle_time());
    println!("state: {:?}", system_idle_state(300));
}
```

The first `listen` call spawns the backend thread; callbacks run on that
thread (do not call `listen` from there).

## Platform support

| Event / query | macOS | Windows | Linux |
|---|---|---|---|
| Suspend / Resume | ✓ IOKit | ✓ WM_POWERBROADCAST | ✓ logind |
| OnAc / OnBattery | ✓ IOPS | ✓ WM_POWERBROADCAST | ✓ UPower |
| LockScreen / UnlockScreen | ✓ DistributedNotificationCenter | ✓ WTS | ✓¹ ScreenSaver D-Bus |
| Shutdown | — | ✓ WM_QUERYENDSESSION | — |
| `is_on_battery_power` | ✓ | ✓ | ✓ |
| `system_idle_time` | ✓ CoreGraphics | ✓ GetLastInputInfo | ✓² Mutter / ScreenSaver |

¹ Linux lock/unlock: via `org.freedesktop.ScreenSaver.ActiveChanged` (GNOME,
KDE). Absent on some compositors (sway, Hyprland) → never emitted.

² Linux idle: `org.gnome.Mutter.IdleMonitor` (GNOME), fallback
`org.freedesktop.ScreenSaver.GetSessionIdleTime` (KDE), otherwise 0.

## Usage in Tauri

The Tauri backend is standard Rust: the crate works there directly.
To forward events to the frontend:

```rust
tauri::Builder::default()
    .setup(|app| {
        let handle = app.handle().clone();
        powermonitor::listen(move |evt| {
            let _ = handle.emit("powermonitor", format!("{evt:?}"));
        })?;
        Ok(())
    })
```

## Out of scope (Electron parity)

The following Electron events are not implemented:
`thermal-state-change`, `speed-limit-change`, `user-did-become-active`,
`user-did-resign-active` (macOS), `shutdown` on macOS/Linux. `IdleState::Locked`
is never returned by `system_idle_state` (locking goes through events).

## License

LGPL-3.0-only
