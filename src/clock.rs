//! Unified monotonic clock. All timestamps in the app are nanoseconds since
//! app launch, derived from the same base Core Audio uses for host times
//! (`mach_absolute_time` on macOS, `CLOCK_MONOTONIC` on Linux), so hardware
//! audio timestamps convert losslessly onto the app timeline.

use std::sync::OnceLock;

#[cfg(target_os = "macos")]
mod mach {
    #[repr(C)]
    pub struct MachTimebaseInfo {
        pub numer: u32,
        pub denom: u32,
    }
    extern "C" {
        pub fn mach_absolute_time() -> u64;
        pub fn mach_timebase_info(info: *mut MachTimebaseInfo) -> i32;
    }
}

#[cfg(target_os = "macos")]
fn timebase() -> (u64, u64) {
    static TB: OnceLock<(u64, u64)> = OnceLock::new();
    *TB.get_or_init(|| {
        let mut info = mach::MachTimebaseInfo { numer: 0, denom: 0 };
        unsafe { mach::mach_timebase_info(&mut info) };
        (info.numer as u64, info.denom as u64)
    })
}

/// Raw monotonic time in ns (absolute, since boot-ish origin).
#[cfg(target_os = "macos")]
fn raw_ns() -> u64 {
    let (numer, denom) = timebase();
    let t = unsafe { mach::mach_absolute_time() } as u128;
    (t * numer as u128 / denom as u128) as u64
}

#[cfg(target_os = "linux")]
fn raw_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn raw_ns() -> u64 {
    use std::time::Instant;
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_nanos() as u64
}

fn origin() -> u64 {
    static ORIGIN: OnceLock<u64> = OnceLock::new();
    *ORIGIN.get_or_init(raw_ns)
}

/// Initialize the clock origin. Call once early in main().
pub fn init() {
    origin();
}

/// Nanoseconds since app launch.
pub fn now_ns() -> u64 {
    raw_ns().saturating_sub(origin())
}

/// Convert a Core Audio `mHostTime` (mach ticks) to ns-since-app-launch.
#[cfg(target_os = "macos")]
pub fn host_time_to_ns(host_time: u64) -> u64 {
    let (numer, denom) = timebase();
    let ns = (host_time as u128 * numer as u128 / denom as u128) as u64;
    ns.saturating_sub(origin())
}

#[cfg(not(target_os = "macos"))]
pub fn host_time_to_ns(host_time: u64) -> u64 {
    host_time.saturating_sub(origin())
}
