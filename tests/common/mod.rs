//! Shared end-to-end harness for the Linux backend: private D-Bus daemons
//! standing in for the system and session buses, and zbus mocks of the
//! services the backend talks to.
//!
//! The backend state is process-global, so each scenario lives in its own
//! test binary (one `#[test]` per file) and calls [`setup_buses`] first.
//! Each binary only uses part of this module, hence `dead_code` is allowed.

#![cfg(target_os = "linux")]
#![allow(dead_code)]

use std::io::{self, BufRead, BufReader, Read};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use futures::executor::block_on;
use powermonitor::PowerEvent;
use zbus::blocking::Connection;
use zbus::blocking::connection::Builder;
use zbus::blocking::object_server::InterfaceRef;
use zbus::interface;
use zbus::object_server::{Interface, SignalEmitter};
use zbus::zvariant::OwnedFd;

const DBUS_DAEMON: &str = "/usr/bin/dbus-daemon";

pub const LOGIND_NAME: &str = "org.freedesktop.login1";
pub const LOGIND_PATH: &str = "/org/freedesktop/login1";
pub const UPOWER_NAME: &str = "org.freedesktop.UPower";
pub const UPOWER_PATH: &str = "/org/freedesktop/UPower";
pub const GNOME_SCREENSAVER_NAME: &str = "org.gnome.ScreenSaver";
pub const GNOME_SCREENSAVER_PATH: &str = "/org/gnome/ScreenSaver";
pub const FDO_SCREENSAVER_NAME: &str = "org.freedesktop.ScreenSaver";
pub const FDO_SCREENSAVER_PATH: &str = "/ScreenSaver";
pub const MUTTER_IDLE_NAME: &str = "org.gnome.Mutter.IdleMonitor";
pub const MUTTER_IDLE_PATH: &str = "/org/gnome/Mutter/IdleMonitor/Core";

/// `true` when `POWERMONITOR_E2E_STRICT=1`: a missing prerequisite (such
/// as `dbus-daemon`) fails the test instead of skipping it.
pub fn strict() -> bool {
    std::env::var("POWERMONITOR_E2E_STRICT").is_ok_and(|v| v == "1")
}

/// Session-like bus configuration without any service directory, so that
/// a call to an unmocked name fails instead of activating a real service.
const BUS_CONFIG: &str = r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:tmpdir=/tmp</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
"#;

fn write_bus_config() -> io::Result<PathBuf> {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("powermonitor-dbus-{}-{n}.conf", std::process::id()));
    std::fs::write(&path, BUS_CONFIG)?;
    Ok(path)
}

/// How long [`TestBus::start`] waits for the daemon to print its address.
const START_TIMEOUT: Duration = Duration::from_secs(10);

/// Path of the socket file behind a `unix:path=...` address, if any
/// (abstract sockets leave nothing on disk).
fn socket_path(address: &str) -> Option<&str> {
    address
        .split(';')
        .next()?
        .strip_prefix("unix:")?
        .split(',')
        .find_map(|kv| kv.strip_prefix("path="))
}

/// A private `dbus-daemon`, killed when dropped.
pub struct TestBus {
    child: Child,
    // Kept open so that the daemon never writes to a closed pipe.
    _stdout: Option<BufReader<ChildStdout>>,
    address: String,
    config: PathBuf,
}

impl TestBus {
    /// Starts a daemon with a session-like configuration and waits (at
    /// most [`START_TIMEOUT`]) for its address.
    pub fn start() -> io::Result<Self> {
        let config = write_bus_config()?;
        let spawned = Command::new(DBUS_DAEMON)
            .arg(format!("--config-file={}", config.display()))
            .args(["--nofork", "--print-address=1"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn();
        let mut child = match spawned {
            Ok(child) => child,
            Err(e) => {
                let _ = std::fs::remove_file(&config);
                return Err(e);
            }
        };
        // Read the address on a helper thread so that a stuck daemon cannot
        // block the test forever. Once the daemon is killed, the thread
        // gets EOF and exits.
        let stdout = child.stdout.take().expect("piped stdout");
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut stdout = BufReader::new(stdout);
            let mut line = String::new();
            let read = stdout.read_line(&mut line).map(|_| line);
            let _ = tx.send((stdout, read));
        });
        // From here on, `Drop` cleans up on error.
        let mut bus = Self {
            child,
            _stdout: None,
            address: String::new(),
            config,
        };
        let (stdout, read) = rx.recv_timeout(START_TIMEOUT).map_err(|_| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                format!("dbus-daemon did not print its address within {START_TIMEOUT:?}"),
            )
        })?;
        bus._stdout = Some(stdout);
        bus.address = read?.trim().to_owned();
        if bus.address.is_empty() {
            return Err(io::Error::other(
                "dbus-daemon exited without printing its address",
            ));
        }
        Ok(bus)
    }

    pub fn address(&self) -> &str {
        &self.address
    }

    /// Opens a new client connection to this bus.
    pub fn connect(&self) -> zbus::Result<Connection> {
        Builder::address(self.address())?.build()
    }
}

impl Drop for TestBus {
    fn drop(&mut self) {
        // SIGKILL: the daemon cannot unlink its socket file itself.
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(path) = socket_path(&self.address) {
            let _ = std::fs::remove_file(path);
        }
        let _ = std::fs::remove_file(&self.config);
    }
}

/// The two private buses of a scenario.
pub struct Buses {
    pub system: TestBus,
    pub session: TestBus,
}

/// Exports `system` / `session` in `DBUS_SYSTEM_BUS_ADDRESS` /
/// `DBUS_SESSION_BUS_ADDRESS`.
///
/// [`setup_buses`] already does it; call it directly only to change the
/// addresses afterwards, e.g. a scenario that starts with an invalid
/// system bus address and must export the valid one again after a failed
/// `listen()`.
///
/// # Safety
///
/// Same contract as [`std::env::set_var`]: no other thread may read or
/// write the environment at the same time. After a crate or mock call,
/// zbus/async-io threads may exist; only call it once they are idle
/// (e.g. right after a `listen()` that returned `Err`, whose backend
/// thread is gone) and accept that this relies on them not touching the
/// environment.
pub unsafe fn export_bus_env(system: &str, session: &str) {
    // SAFETY: forwarded to the caller.
    unsafe {
        std::env::set_var("DBUS_SYSTEM_BUS_ADDRESS", system);
        std::env::set_var("DBUS_SESSION_BUS_ADDRESS", session);
    }
}

/// Starts the system and session test buses, exports their addresses
/// (see [`export_bus_env`]) and removes `DISPLAY` / `WAYLAND_DISPLAY`, so
/// that the backend never reaches the developer's real display server
/// (an X11 scenario sets `DISPLAY` to its own Xvfb afterwards).
///
/// Call it once, first thing in the test, before any call to the crate
/// or any mock start. Returns `None` (test skipped) when `dbus-daemon`
/// cannot be started, unless [`strict`] mode is on, where it panics.
pub fn setup_buses() -> Option<Buses> {
    let started = TestBus::start().and_then(|system| {
        Ok(Buses {
            system,
            session: TestBus::start()?,
        })
    });
    match started {
        Ok(buses) => {
            // SAFETY: called at the start of the only test of the binary,
            // before the crate or the mocks spawn any thread that could
            // read the environment concurrently (the address reader
            // threads of `TestBus::start` do not touch it).
            unsafe {
                export_bus_env(buses.system.address(), buses.session.address());
                std::env::remove_var("DISPLAY");
                std::env::remove_var("WAYLAND_DISPLAY");
            }
            Some(buses)
        }
        Err(e) if strict() => panic!("cannot start {DBUS_DAEMON}: {e}"),
        Err(e) => {
            eprintln!("skipped: cannot start {DBUS_DAEMON}: {e}");
            None
        }
    }
}

/// Records the events received by a callback.
pub struct EventLog {
    tx: Sender<PowerEvent>,
    rx: Receiver<PowerEvent>,
}

impl Default for EventLog {
    fn default() -> Self {
        Self::new()
    }
}

impl EventLog {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        Self { tx, rx }
    }

    /// A callback to pass to `powermonitor::listen`.
    pub fn callback(&self) -> impl Fn(PowerEvent) + Send + Sync + 'static {
        let tx = self.tx.clone();
        move |evt| {
            let _ = tx.send(evt);
        }
    }

    /// Next recorded event, or `None` after `timeout`.
    pub fn next(&self, timeout: Duration) -> Option<PowerEvent> {
        self.rx.recv_timeout(timeout).ok()
    }

    /// Consumes recorded events until `expected` shows up. Returns `false`
    /// if it does not within `timeout`.
    pub fn wait_for(&self, expected: PowerEvent, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.rx.recv_timeout(left) {
                Ok(evt) if evt == expected => return true,
                Ok(_) => continue,
                Err(_) => return false,
            }
        }
    }
}

/// Polls `cond` every 10 ms until it holds or `timeout` elapses.
pub fn wait_until(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if cond() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn serve<I: Interface>(
    bus: &TestBus,
    name: &'static str,
    path: &'static str,
    iface: I,
) -> Connection {
    Builder::address(bus.address())
        .and_then(|b| b.name(name))
        .and_then(|b| b.serve_at(path, iface))
        .and_then(|b| b.build())
        .unwrap_or_else(|e| panic!("cannot start the {name} mock: {e}"))
}

fn iface_ref<I: Interface>(conn: &Connection, path: &'static str) -> InterfaceRef<I> {
    conn.object_server()
        .interface::<_, I>(path)
        .unwrap_or_else(|e| panic!("no mock interface at {path}: {e}"))
}

fn locked<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

// ---------------------------------------------------------------------------
// logind (system bus)
// ---------------------------------------------------------------------------

/// Arguments of one `Inhibit` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InhibitCall {
    pub what: String,
    pub who: String,
    pub why: String,
    pub mode: String,
}

#[derive(Default)]
struct LogindState {
    calls: Vec<InhibitCall>,
    // Our end of each inhibitor: it reads EOF once the client has closed
    // the descriptor it received.
    held: Vec<UnixStream>,
}

struct LogindIface {
    state: Arc<Mutex<LogindState>>,
}

#[interface(name = "org.freedesktop.login1.Manager")]
impl LogindIface {
    fn inhibit(
        &self,
        what: String,
        who: String,
        why: String,
        mode: String,
    ) -> zbus::fdo::Result<OwnedFd> {
        let (ours, theirs) =
            UnixStream::pair().map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        let mut state = locked(&self.state);
        state.calls.push(InhibitCall {
            what,
            who,
            why,
            mode,
        });
        state.held.push(ours);
        Ok(std::os::fd::OwnedFd::from(theirs).into())
    }

    #[zbus(signal)]
    async fn prepare_for_sleep(emitter: &SignalEmitter<'_>, start: bool) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn prepare_for_shutdown(emitter: &SignalEmitter<'_>, start: bool) -> zbus::Result<()>;
}

/// `org.freedesktop.login1` mock: sleep/shutdown signals and `Inhibit`.
pub struct MockLogind {
    conn: Connection,
    state: Arc<Mutex<LogindState>>,
}

impl MockLogind {
    /// Starts the mock on the system bus.
    pub fn start(buses: &Buses) -> Self {
        Self::start_on(&buses.system)
    }

    pub fn start_on(bus: &TestBus) -> Self {
        let state = Arc::new(Mutex::new(LogindState::default()));
        let iface = LogindIface {
            state: state.clone(),
        };
        let conn = serve(bus, LOGIND_NAME, LOGIND_PATH, iface);
        Self { conn, state }
    }

    fn iface(&self) -> InterfaceRef<LogindIface> {
        iface_ref(&self.conn, LOGIND_PATH)
    }

    pub fn emit_prepare_for_sleep(&self, start: bool) {
        let iface = self.iface();
        block_on(LogindIface::prepare_for_sleep(
            iface.signal_emitter(),
            start,
        ))
        .expect("emit PrepareForSleep");
    }

    pub fn emit_prepare_for_shutdown(&self, start: bool) {
        let iface = self.iface();
        block_on(LogindIface::prepare_for_shutdown(
            iface.signal_emitter(),
            start,
        ))
        .expect("emit PrepareForShutdown");
    }

    pub fn inhibit_count(&self) -> usize {
        locked(&self.state).calls.len()
    }

    pub fn inhibit_calls(&self) -> Vec<InhibitCall> {
        locked(&self.state).calls.clone()
    }

    /// Waits until at least `n` `Inhibit` calls were received.
    pub fn wait_for_inhibits(&self, n: usize, timeout: Duration) -> bool {
        wait_until(timeout, || self.inhibit_count() >= n)
    }

    /// Waits until the client closes the descriptor returned by the
    /// `index`-th `Inhibit` call (0-based). Returns `false` if it is still
    /// held after `timeout`.
    pub fn wait_inhibitor_released(&self, index: usize, timeout: Duration) -> bool {
        let mut stream = {
            let state = locked(&self.state);
            let held = state
                .held
                .get(index)
                .unwrap_or_else(|| panic!("no inhibitor #{index}"));
            held.try_clone().expect("clone the inhibitor socket")
        };
        let deadline = Instant::now() + timeout;
        let mut buf = [0u8; 64];
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            stream
                .set_read_timeout(Some(left))
                .expect("set the inhibitor read timeout");
            match stream.read(&mut buf) {
                Ok(0) => return true,
                Ok(_) => continue,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    return false;
                }
                Err(e) => panic!("read the inhibitor socket: {e}"),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// UPower (system bus)
// ---------------------------------------------------------------------------

struct UPowerIface {
    on_battery: bool,
}

#[interface(name = "org.freedesktop.UPower")]
impl UPowerIface {
    #[zbus(property)]
    fn on_battery(&self) -> bool {
        self.on_battery
    }
}

/// `org.freedesktop.UPower` mock exposing the `OnBattery` property.
pub struct MockUPower {
    conn: Connection,
}

impl MockUPower {
    /// Starts the mock on the system bus.
    pub fn start(buses: &Buses, on_battery: bool) -> Self {
        Self::start_on(&buses.system, on_battery)
    }

    pub fn start_on(bus: &TestBus, on_battery: bool) -> Self {
        let conn = serve(bus, UPOWER_NAME, UPOWER_PATH, UPowerIface { on_battery });
        Self { conn }
    }

    /// Updates `OnBattery` and emits `PropertiesChanged`.
    pub fn set_on_battery(&self, on_battery: bool) {
        let iface = iface_ref::<UPowerIface>(&self.conn, UPOWER_PATH);
        let mut guard = iface.get_mut();
        guard.on_battery = on_battery;
        block_on(guard.on_battery_changed(iface.signal_emitter()))
            .expect("emit PropertiesChanged(OnBattery)");
    }
}

// ---------------------------------------------------------------------------
// GNOME screen saver (session bus)
// ---------------------------------------------------------------------------

struct GnomeScreenSaverIface;

#[interface(name = "org.gnome.ScreenSaver")]
impl GnomeScreenSaverIface {
    #[zbus(signal)]
    async fn active_changed(emitter: &SignalEmitter<'_>, active: bool) -> zbus::Result<()>;
}

/// `org.gnome.ScreenSaver` mock emitting `ActiveChanged`.
pub struct MockGnomeScreenSaver {
    conn: Connection,
}

impl MockGnomeScreenSaver {
    /// Starts the mock on the session bus.
    pub fn start(buses: &Buses) -> Self {
        Self::start_on(&buses.session)
    }

    pub fn start_on(bus: &TestBus) -> Self {
        let conn = serve(
            bus,
            GNOME_SCREENSAVER_NAME,
            GNOME_SCREENSAVER_PATH,
            GnomeScreenSaverIface,
        );
        Self { conn }
    }

    pub fn emit_active_changed(&self, active: bool) {
        let iface = iface_ref::<GnomeScreenSaverIface>(&self.conn, GNOME_SCREENSAVER_PATH);
        block_on(GnomeScreenSaverIface::active_changed(
            iface.signal_emitter(),
            active,
        ))
        .expect("emit ActiveChanged");
    }
}

// ---------------------------------------------------------------------------
// freedesktop screen saver (session bus, KDE)
// ---------------------------------------------------------------------------

struct FdoScreenSaverIface {
    idle_ms: u32,
}

#[interface(name = "org.freedesktop.ScreenSaver")]
impl FdoScreenSaverIface {
    /// Idle time in milliseconds.
    fn get_session_idle_time(&self) -> u32 {
        self.idle_ms
    }

    #[zbus(signal)]
    async fn active_changed(emitter: &SignalEmitter<'_>, active: bool) -> zbus::Result<()>;
}

/// `org.freedesktop.ScreenSaver` mock: `ActiveChanged` and
/// `GetSessionIdleTime` (milliseconds).
pub struct MockFdoScreenSaver {
    conn: Connection,
}

impl MockFdoScreenSaver {
    /// Starts the mock on the session bus.
    pub fn start(buses: &Buses, idle_ms: u32) -> Self {
        Self::start_on(&buses.session, idle_ms)
    }

    pub fn start_on(bus: &TestBus, idle_ms: u32) -> Self {
        let conn = serve(
            bus,
            FDO_SCREENSAVER_NAME,
            FDO_SCREENSAVER_PATH,
            FdoScreenSaverIface { idle_ms },
        );
        Self { conn }
    }

    fn iface(&self) -> InterfaceRef<FdoScreenSaverIface> {
        iface_ref(&self.conn, FDO_SCREENSAVER_PATH)
    }

    pub fn set_idle_ms(&self, idle_ms: u32) {
        self.iface().get_mut().idle_ms = idle_ms;
    }

    pub fn emit_active_changed(&self, active: bool) {
        let iface = self.iface();
        block_on(FdoScreenSaverIface::active_changed(
            iface.signal_emitter(),
            active,
        ))
        .expect("emit ActiveChanged");
    }
}

// ---------------------------------------------------------------------------
// Mutter idle monitor (session bus, GNOME)
// ---------------------------------------------------------------------------

struct MutterIdleIface {
    idle_ms: u64,
}

#[interface(name = "org.gnome.Mutter.IdleMonitor")]
impl MutterIdleIface {
    /// Idle time in milliseconds.
    fn get_idletime(&self) -> u64 {
        self.idle_ms
    }
}

/// `org.gnome.Mutter.IdleMonitor` mock: `GetIdletime` (milliseconds).
pub struct MockMutterIdle {
    conn: Connection,
}

impl MockMutterIdle {
    /// Starts the mock on the session bus.
    pub fn start(buses: &Buses, idle_ms: u64) -> Self {
        Self::start_on(&buses.session, idle_ms)
    }

    pub fn start_on(bus: &TestBus, idle_ms: u64) -> Self {
        let conn = serve(
            bus,
            MUTTER_IDLE_NAME,
            MUTTER_IDLE_PATH,
            MutterIdleIface { idle_ms },
        );
        Self { conn }
    }

    pub fn set_idle_ms(&self, idle_ms: u64) {
        iface_ref::<MutterIdleIface>(&self.conn, MUTTER_IDLE_PATH)
            .get_mut()
            .idle_ms = idle_ms;
    }
}
