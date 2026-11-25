fn main() {
    // Keep the build (and its lease) alive long enough to collect during it.
    if std::env::var("HOLD_SECS").is_ok() {
        let n: u64 = std::env::var("HOLD_SECS").unwrap().parse().unwrap_or(0);
        std::thread::sleep(std::time::Duration::from_secs(n));
    }
    println!("cargo:rerun-if-env-changed=HOLD_SECS");
}
