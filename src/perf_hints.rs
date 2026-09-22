/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
//! Host scheduling and power-management hints for the emulator thread.
//!
//! Everything the emulator does — the JIT'd guest code for every guest
//! thread, the host implementations of the frameworks, GL submission and the
//! presentation of frames — runs on one host thread. On Android that thread
//! typically spends a well-defined chunk of each display refresh busy and
//! then sleeps until the next frame (the emulator paces frames itself, see
//! `--fps-limit`). The CPU frequency governor sees that idle time as
//! headroom and clocks the core down; the busy chunk then grows into the
//! whole interval, frames start to miss it, and the game drops to half the
//! refresh rate even though the device could easily have kept up.
//!
//! Two standard remedies, both Android-only and both harmless no-ops
//! elsewhere:
//!
//! - **ADPF performance hint session**: report the per-frame CPU time of the
//!   emulator thread to the OS together with the frame-time target, through
//!   the NDK's `APerformanceHint_*` API (Android 13+, loaded dynamically so
//!   older releases are unaffected). The power HAL then keeps the core
//!   clocked for the reported load instead of for the idle time between
//!   frames. This is what games use for the same reason.
//! - **Thread priority**: raise the emulator thread to Android's "display"
//!   priority band (the same one the UI toolkit's render thread runs at),
//!   so background work on the device doesn't steal time slices from it.
//!
//! Both can be switched off with `--no-perf-hints` / `TOUCHHLE_PERF_HINTS=0`.

use std::time::Duration;

/// Handle for the hints. Create it on the emulator thread, then call
/// [PerfHints::frame_presented] once per presented frame from that thread.
pub struct PerfHints {
    #[cfg(target_os = "android")]
    inner: Option<android::HintSession>,
}

impl PerfHints {
    /// `target_frame_time` is the frame interval the emulator paces to (or
    /// the display's interval if it doesn't pace). `affinity` comes from
    /// `--affinity=` (`big` / `all` / `off` / a CPU list); the
    /// `TOUCHHLE_AFFINITY` environment variable, when set, takes precedence.
    pub fn new(enabled: bool, target_frame_time: Duration, affinity: Option<&str>) -> PerfHints {
        #[cfg(not(target_os = "android"))]
        {
            let _ = (enabled, target_frame_time, affinity);
            PerfHints {}
        }
        #[cfg(target_os = "android")]
        {
            if !enabled {
                log!("Performance hints disabled (--no-perf-hints).");
                return PerfHints { inner: None };
            }
            android::raise_thread_priority();
            android::pin_to_big_cores(affinity);
            PerfHints {
                inner: android::HintSession::open(target_frame_time),
            }
        }
    }

    /// Report that a frame was just presented. Cheap when hints are
    /// unavailable or disabled.
    #[inline]
    pub fn frame_presented(&mut self) {
        #[cfg(target_os = "android")]
        {
            if let Some(session) = self.inner.as_mut() {
                session.frame_presented();
            }
        }
    }
}

#[cfg(target_os = "android")]
mod android {
    use std::ffi::{c_int, c_void, CStr};
    use std::time::{Duration, Instant};

    /// `THREAD_PRIORITY_URGENT_DISPLAY` from `android.os.Process`: the band
    /// used for compositing the screen and delivering input. Apps may use it
    /// for their own threads; SurfaceFlinger and the input pipeline run at
    /// real-time priorities anyway, so this can't starve them.
    const EMULATOR_THREAD_NICE: c_int = -8;

    pub fn raise_thread_priority() {
        // SAFETY: plain libc calls with valid arguments; `gettid` has no
        // preconditions and `setpriority` on our own thread is always
        // permitted to fail gracefully.
        unsafe {
            let tid = ::libc::gettid();
            let before = ::libc::getpriority(::libc::PRIO_PROCESS, tid as ::libc::id_t);
            if ::libc::setpriority(
                ::libc::PRIO_PROCESS,
                tid as ::libc::id_t,
                EMULATOR_THREAD_NICE,
            ) == 0
            {
                log!(
                    "Emulator thread priority raised (nice {} -> {}).",
                    before,
                    EMULATOR_THREAD_NICE
                );
            } else {
                log!(
                    "Could not raise the emulator thread priority (nice {}): {}",
                    before,
                    std::io::Error::last_os_error()
                );
            }
        }
    }

    /// Pin the calling thread to the "big" CPU cluster: the cores whose
    /// maximum frequency matches the fastest core on the device (typically a
    /// big.LITTLE or tri-cluster SoC where the scheduler is otherwise free to
    /// migrate the single hot emulator thread between clusters, losing L2/L3
    /// locality and risking idle-clock time on the slower cores).
    ///
    /// Cores are discovered from sysfs (`cpufreq/cpuinfo_max_freq`); every
    /// core within 10% of the fastest core's frequency is included, which
    /// keeps a homogeneous (all-big or all-mid) device un-pinned when the
    /// "cluster" would be the whole CPU anyway.
    ///
    /// `affinity_override` comes from `--affinity=`; the `TOUCHHLE_AFFINITY`
    /// environment variable, when set, takes precedence over it.
    pub fn pin_to_big_cores(affinity_override: Option<&str>) {
        let override_ = std::env::var("TOUCHHLE_AFFINITY")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .or_else(|| affinity_override.map(|v| v.to_string()))
            .unwrap_or_default();
        let override_ = override_.trim();
        if override_.is_empty() || override_.eq_ignore_ascii_case("big") {
            if let Some(cores) = big_core_cpus() {
                if set_affinity(&cores, "big cluster") {
                    return;
                }
            }
            return;
        }
        if override_.eq_ignore_ascii_case("off") || override_.eq_ignore_ascii_case("all") {
            log!("CPU affinity override: staying on all cores.");
            return;
        }
        let cores: Vec<usize> = override_
            .split(',')
            .flat_map(|part| match part.split_once('-') {
                Some((a, b)) => {
                    let a: usize = a.trim().parse().unwrap_or(usize::MAX);
                    let b: usize = b.trim().parse().unwrap_or(usize::MAX);
                    (a..=b).collect::<Vec<usize>>()
                }
                None => match part.trim().parse() {
                    Ok(cpu) => vec![cpu],
                    Err(_) => Vec::new(),
                },
            })
            .collect();
        if cores.is_empty() {
            log!("TOUCHHLE_AFFINITY={override_}: no valid CPU list, ignoring.");
            return;
        }
        set_affinity(&cores, &format!("CPU list {override_}"));
    }

    /// The CPUs of the big cluster, discovered from sysfs. `None` when the
    /// layout can't be determined or there is nothing to pin to.
    fn big_core_cpus() -> Option<Vec<usize>> {
        let mut freqs: Vec<(usize, u64)> = Vec::new();
        for cpu in 0..64 {
            let path = format!("/sys/devices/system/cpu/cpu{cpu}/cpufreq/cpuinfo_max_freq");
            if !std::path::Path::new(&path).exists() {
                if cpu == 0 {
                    return None;
                }
                continue;
            }
            if let Ok(text) = std::fs::read_to_string(&path) {
                if let Ok(khz) = text.trim().parse::<u64>() {
                    freqs.push((cpu, khz));
                }
            }
        }
        if freqs.len() < 2 {
            return None;
        }
        let max = freqs.iter().map(|(_, f)| *f).max()?;
        // A core is "big" when within 10% of the fastest core: 2.7 GHz
        // against a 2.2 GHz mid cluster does not qualify, sibling big cores
        // (e.g. 3.19 GHz vs 3.00 GHz on the same SoC) do.
        let big: Vec<usize> = freqs
            .iter()
            .filter(|(_, f)| *f * 10 >= max * 9)
            .map(|(cpu, _)| *cpu)
            .collect();
        // Nothing to gain from pinning when every core qualifies.
        if big.len() == freqs.len() {
            log!(
                "CPU affinity: all {} cores run at the same maximum frequency, leaving the scheduler alone.",
                freqs.len()
            );
            return None;
        }
        Some(big)
    }

    /// Apply a CPU affinity mask to the calling thread. Logging is best
    /// effort: affinity is an optimization, never a correctness requirement.
    fn set_affinity(cores: &[usize], what: &str) -> bool {
        // SAFETY: a zeroed `cpu_set_t` is valid (an empty set), and
        // `CPU_SET` refuses indices >= CPU_SETSIZE.
        unsafe {
            let mut set: ::libc::cpu_set_t = std::mem::zeroed();
            ::libc::CPU_ZERO(&mut set);
            for &cpu in cores {
                ::libc::CPU_SET(cpu, &mut set);
            }
            let res = ::libc::sched_setaffinity(
                0,
                std::mem::size_of::<::libc::cpu_set_t>(),
                &set,
            );
            if res == 0 {
                log!(
                    "Emulator thread pinned to the {} (cores {:?}).",
                    what,
                    cores
                );
                true
            } else {
                log!(
                    "Could not pin the emulator thread to the {} (cores {:?}): {}",
                    what,
                    cores,
                    std::io::Error::last_os_error()
                );
                false
            }
        }
    }

    /// Opaque NDK types.
    type APerformanceHintManager = c_void;
    type APerformanceHintSession = c_void;

    /// The subset of `<android/performance_hint.h>` (API 33+) we use,
    /// resolved at runtime from `libandroid.so`.
    struct Api {
        get_manager: unsafe extern "C" fn() -> *mut APerformanceHintManager,
        get_preferred_update_rate_nanos:
            unsafe extern "C" fn(*mut APerformanceHintManager) -> i64,
        create_session: unsafe extern "C" fn(
            *mut APerformanceHintManager,
            *const i32,
            usize,
            i64,
        ) -> *mut APerformanceHintSession,
        report_actual_work_duration:
            unsafe extern "C" fn(*mut APerformanceHintSession, i64) -> c_int,
        close_session: unsafe extern "C" fn(*mut APerformanceHintSession),
    }

    impl Api {
        fn load() -> Option<Api> {
            // SAFETY: dlopen/dlsym with valid NUL-terminated names; the
            // function pointer types match the NDK header declarations. The
            // handle is intentionally leaked: libandroid.so stays loaded for
            // the life of the process anyway (SDL links against it).
            unsafe {
                let lib = ::libc::dlopen(c"libandroid.so".as_ptr(), ::libc::RTLD_NOW);
                if lib.is_null() {
                    return None;
                }
                let sym = |name: &CStr| -> Option<*mut c_void> {
                    let p = ::libc::dlsym(lib, name.as_ptr());
                    if p.is_null() {
                        None
                    } else {
                        Some(p)
                    }
                };
                Some(Api {
                    get_manager: std::mem::transmute::<
                        *mut c_void,
                        unsafe extern "C" fn() -> *mut APerformanceHintManager,
                    >(sym(c"APerformanceHint_getManager")?),
                    get_preferred_update_rate_nanos: std::mem::transmute::<
                        *mut c_void,
                        unsafe extern "C" fn(*mut APerformanceHintManager) -> i64,
                    >(sym(c"APerformanceHint_getPreferredUpdateRateNanos")?),
                    create_session: std::mem::transmute::<
                        *mut c_void,
                        unsafe extern "C" fn(
                            *mut APerformanceHintManager,
                            *const i32,
                            usize,
                            i64,
                        ) -> *mut APerformanceHintSession,
                    >(sym(c"APerformanceHint_createSession")?),
                    report_actual_work_duration: std::mem::transmute::<
                        *mut c_void,
                        unsafe extern "C" fn(*mut APerformanceHintSession, i64) -> c_int,
                    >(sym(c"APerformanceHint_reportActualWorkDuration")?),
                    close_session: std::mem::transmute::<
                        *mut c_void,
                        unsafe extern "C" fn(*mut APerformanceHintSession),
                    >(sym(c"APerformanceHint_closeSession")?),
                })
            }
        }
    }

    /// CPU time consumed by the calling thread so far.
    fn thread_cpu_time() -> Duration {
        // SAFETY: an all-zero timespec is a valid value (and avoids naming
        // the padding fields the libc crate adds on some targets).
        let mut ts: ::libc::timespec = unsafe { std::mem::zeroed() };
        // SAFETY: valid out-pointer; CLOCK_THREAD_CPUTIME_ID always exists on
        // Android.
        let res = unsafe { ::libc::clock_gettime(::libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
        if res != 0 {
            return Duration::ZERO;
        }
        Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
    }

    pub struct HintSession {
        api: Api,
        session: *mut APerformanceHintSession,
        /// Minimum spacing between reports the HAL wants.
        min_report_interval: Duration,
        last_report: Option<Instant>,
        /// Thread CPU time at the previous report.
        cpu_time_at_last_report: Duration,
        /// Frames since the previous report.
        frames_since_report: u32,
        errors_logged: u32,
    }

    impl HintSession {
        pub fn open(target_frame_time: Duration) -> Option<HintSession> {
            let api = Api::load()?;
            // SAFETY: the function pointers were resolved from libandroid.so
            // and are called with the argument shapes the NDK documents.
            unsafe {
                let manager = (api.get_manager)();
                if manager.is_null() {
                    log!("ADPF performance hints unavailable on this device (no hint manager).");
                    return None;
                }
                let preferred_rate = (api.get_preferred_update_rate_nanos)(manager);
                let tid = ::libc::gettid();
                let session = (api.create_session)(
                    manager,
                    &tid,
                    1,
                    target_frame_time.as_nanos().min(i64::MAX as u128) as i64,
                );
                if session.is_null() {
                    log!("ADPF performance hints unavailable on this device (could not create a hint session).");
                    return None;
                }
                let min_report_interval = if preferred_rate > 0 {
                    Duration::from_nanos(preferred_rate as u64)
                } else {
                    Duration::ZERO
                };
                log!(
                    "ADPF performance hint session opened for the emulator thread (target {:.2} ms per frame, preferred update rate {:.2} ms).",
                    target_frame_time.as_secs_f64() * 1000.0,
                    min_report_interval.as_secs_f64() * 1000.0
                );
                Some(HintSession {
                    api,
                    session,
                    min_report_interval,
                    last_report: None,
                    cpu_time_at_last_report: thread_cpu_time(),
                    frames_since_report: 0,
                    errors_logged: 0,
                })
            }
        }

        pub fn frame_presented(&mut self) {
            self.frames_since_report += 1;
            let now = Instant::now();
            if let Some(last) = self.last_report {
                if now.duration_since(last) < self.min_report_interval {
                    return;
                }
            }
            let cpu_now = thread_cpu_time();
            let busy = cpu_now.saturating_sub(self.cpu_time_at_last_report);
            // Average CPU time per frame since the last report. Normally the
            // report interval is shorter than a frame, so this is simply the
            // last frame's CPU time.
            let per_frame = busy / self.frames_since_report.max(1);
            self.cpu_time_at_last_report = cpu_now;
            self.frames_since_report = 0;
            self.last_report = Some(now);
            // The HAL rejects zero durations.
            let nanos = per_frame.as_nanos().clamp(1, i64::MAX as u128) as i64;
            // SAFETY: valid session pointer from `open`.
            let res = unsafe { (self.api.report_actual_work_duration)(self.session, nanos) };
            if res != 0 && self.errors_logged < 3 {
                self.errors_logged += 1;
                log!(
                    "ADPF: reportActualWorkDuration failed with {} (will keep trying quietly).",
                    res
                );
            }
        }
    }

    impl Drop for HintSession {
        fn drop(&mut self) {
            // SAFETY: valid session pointer from `open`, closed exactly once.
            unsafe { (self.api.close_session)(self.session) };
        }
    }
}
