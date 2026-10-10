//! Per-statement run time for `.timer`: wall clock from [`Instant`], user/sys
//! CPU from `getrusage(RUSAGE_SELF)`. The CPU figures are process-wide, so a
//! parallel engine shows `user` well above `real` -- the same signal
//! `hyperfine`'s User column gives, now available inside the REPL.

use std::fmt;
use std::time::{Duration, Instant};

/// Wall and CPU time consumed by one statement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RunTime {
    /// Wall-clock time.
    pub real: Duration,
    /// User-mode CPU time across all threads.
    pub user: Duration,
    /// Kernel-mode CPU time across all threads.
    pub sys: Duration,
}

impl fmt::Display for RunTime {
    /// `sqlite3`'s `.timer` shape: `Run Time: real 0.012 user 0.010 sys 0.001`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Run Time: real {:.3} user {:.3} sys {:.3}",
            self.real.as_secs_f64(),
            self.user.as_secs_f64(),
            self.sys.as_secs_f64()
        )
    }
}

/// A started stopwatch; [`Timer::stop`] yields the [`RunTime`] since
/// [`Timer::start`]. Public so a one-shot CLI mode can print the same line
/// the REPL does.
#[derive(Debug)]
pub struct Timer {
    start: Instant,
    cpu: CpuTimes,
}

impl Timer {
    /// Snapshot the clocks now.
    pub fn start() -> Self {
        Timer {
            start: Instant::now(),
            cpu: cpu_times(),
        }
    }

    /// Time elapsed since [`Timer::start`].
    pub fn stop(self) -> RunTime {
        let now = cpu_times();
        RunTime {
            real: self.start.elapsed(),
            user: now.user.saturating_sub(self.cpu.user),
            sys: now.sys.saturating_sub(self.cpu.sys),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct CpuTimes {
    user: Duration,
    sys: Duration,
}

// `getrusage` has no safe stdlib equivalent; the call only writes into a
// zeroed struct we own, so the unsafe surface is this one function.
#[allow(unsafe_code)]
fn cpu_times() -> CpuTimes {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    // SAFETY: RUSAGE_SELF is a valid `who`, and `usage` points to writable
    // memory of exactly `sizeof(struct rusage)`.
    let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) };
    if rc != 0 {
        return CpuTimes {
            user: Duration::ZERO,
            sys: Duration::ZERO,
        };
    }
    // SAFETY: getrusage returned 0, so it fully initialized the struct.
    let usage = unsafe { usage.assume_init() };
    CpuTimes {
        user: timeval_to_duration(usage.ru_utime),
        sys: timeval_to_duration(usage.ru_stime),
    }
}

fn timeval_to_duration(tv: libc::timeval) -> Duration {
    let secs = u64::try_from(tv.tv_sec).unwrap_or(0);
    let micros = u64::try_from(tv.tv_usec).unwrap_or(0);
    Duration::from_secs(secs) + Duration::from_micros(micros)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_matches_sqlite3_shape() {
        let rt = RunTime {
            real: Duration::from_micros(12_345),
            user: Duration::from_millis(10),
            sys: Duration::from_millis(1),
        };
        assert_eq!(rt.to_string(), "Run Time: real 0.012 user 0.010 sys 0.001");
    }

    #[test]
    fn timer_measures_cpu_spent_between_start_and_stop() {
        let timer = Timer::start();
        // Burn a little CPU so `user` is non-zero on any reasonable machine.
        let mut acc = 0u64;
        for i in 0..5_000_000u64 {
            acc = acc.wrapping_mul(31).wrapping_add(i);
        }
        assert_ne!(acc, 1);
        let rt = timer.stop();
        assert!(rt.real > Duration::ZERO);
        assert!(rt.user + rt.sys > Duration::ZERO);
    }
}
