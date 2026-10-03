pub mod admission;
pub mod turn;
pub mod wire;
pub fn now_us() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64
}
pub fn now_ms() -> u64 {
    now_us() / 1000
}

/// Opt-in frame receipts. Monotonic time is used for durations; realtime only
/// anchors each host to the clock brackets collected by the proof driver.
pub fn trace_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("IBARA_SCREEN_TRACE").is_some())
}
pub fn monotonic_us() -> u64 {
    let mut time = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time); }
    time.tv_sec as u64 * 1_000_000 + time.tv_nsec as u64 / 1000
}
pub fn trace(stage: &str, frame: u64) {
    if trace_enabled() { trace_at(stage, frame, monotonic_us()); }
}
pub fn trace_at(stage: &str, frame: u64, mono_us: u64) {
    if !trace_enabled() { return; }
    let real_us = now_us().saturating_sub(monotonic_us().saturating_sub(mono_us));
    eprintln!("{}", serde_json::json!({"event":"stage", "stage":stage,
        "frame":frame, "mono_us":mono_us, "real_us":real_us}));
}
