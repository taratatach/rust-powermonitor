//! Listens to power events and prints the current queries.
//! `cargo run --example listen`

use powermonitor::{is_on_battery_power, listen, system_idle_state, system_idle_time};

fn main() {
    listen(|evt| println!("event: {evt:?}")).expect("listen");
    println!("on battery: {}", is_on_battery_power());
    println!("idle: {}s ({:?})", system_idle_time(), system_idle_state(300));
    println!("listening… (Ctrl-C to quit)");
    loop {
        std::thread::park();
    }
}
