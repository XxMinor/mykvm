// Two-process check for the single-instance lock (Windows named mutex /
// macOS flock):
//
//   cargo run --example single_instance -- hold   # terminal 1: acquires, holds 5s
//   cargo run --example single_instance           # terminal 2: prints "acquired: false"
//
// On macOS point HOME at a scratch dir so the check does not collide with a
// running MyKVM (the real app holds the same per-user lock).

fn main() {
    let hold = std::env::args().nth(1).as_deref() == Some("hold");
    let acquired = mykvm_lib::acquire_single_instance();
    println!("acquired: {acquired}");
    if hold && acquired {
        std::thread::sleep(std::time::Duration::from_secs(5));
    }
    std::process::exit(if acquired { 0 } else { 1 });
}
