//! Process CPU time.
//!
//! Wall-clock frame rate alone cannot answer phase 0's question. A harness that
//! holds 30 fps by pinning all four Braswell cores has not passed — the client
//! machine still has to run the user's actual work, and the wizard alongside it.
//! So every run reports CPU time as a share of one core and as a share of the
//! whole machine.

use std::time::Duration;

/// CPU time consumed by this process at a point in time.
#[derive(Debug, Clone, Copy, Default)]
pub struct CpuSnapshot {
    user: Duration,
    kernel: Duration,
}

/// CPU time consumed between two snapshots.
#[derive(Debug, Clone, Copy, Default)]
pub struct CpuUsage {
    pub user: Duration,
    pub kernel: Duration,
    /// Logical cores visible to the process, as reported by the OS.
    pub cores: usize,
}

impl CpuSnapshot {
    pub fn now() -> Self {
        read().unwrap_or_default()
    }

    /// CPU consumed since this snapshot was taken.
    pub fn elapsed_since(self) -> CpuUsage {
        let now = Self::now();
        CpuUsage {
            user: now.user.saturating_sub(self.user),
            kernel: now.kernel.saturating_sub(self.kernel),
            cores: std::thread::available_parallelism().map_or(1, |n| n.get()),
        }
    }
}

impl CpuUsage {
    pub fn total(&self) -> Duration {
        self.user + self.kernel
    }

    /// Share of a single core, where 1.0 means one core fully busy.
    pub fn cores_busy(&self, elapsed: Duration) -> f64 {
        let secs = elapsed.as_secs_f64();
        if secs <= 0.0 {
            return 0.0;
        }
        self.total().as_secs_f64() / secs
    }

    /// Share of the whole machine, 0.0..=1.0 when nothing else is running.
    pub fn machine_share(&self, elapsed: Duration) -> f64 {
        if self.cores == 0 {
            return 0.0;
        }
        self.cores_busy(elapsed) / self.cores as f64
    }

    pub fn render(&self, elapsed: Duration) -> String {
        format!(
            "  время CPU               {:.2} с (польз. {:.2} + сист. {:.2})\n\
             \x20 занято ядер            {:.2} из {} ({:.1}% машины)\n",
            self.total().as_secs_f64(),
            self.user.as_secs_f64(),
            self.kernel.as_secs_f64(),
            self.cores_busy(elapsed),
            self.cores,
            self.machine_share(elapsed) * 100.0,
        )
    }
}

#[cfg(windows)]
fn read() -> Option<CpuSnapshot> {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};

    // FILETIME counts 100-nanosecond intervals. For the kernel and user fields
    // it is a duration, not a date, despite the type name.
    fn to_duration(ft: FILETIME) -> Duration {
        let ticks = ((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64;
        Duration::from_nanos(ticks.saturating_mul(100))
    }

    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();

    // SAFETY: all four out-parameters are live, correctly typed and independent.
    // The pseudo-handle from GetCurrentProcess needs no closing.
    unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &mut creation,
            &mut exit,
            &mut kernel,
            &mut user,
        )
        .ok()?;
    }

    Some(CpuSnapshot {
        user: to_duration(user),
        kernel: to_duration(kernel),
    })
}

#[cfg(unix)]
fn read() -> Option<CpuSnapshot> {
    fn to_duration(tv: libc::timeval) -> Duration {
        Duration::new(tv.tv_sec.max(0) as u64, (tv.tv_usec.max(0) as u32) * 1000)
    }

    // SAFETY: `usage` is a live, correctly sized rusage; RUSAGE_SELF needs no
    // other state.
    let usage = unsafe {
        let mut usage: libc::rusage = std::mem::zeroed();
        if libc::getrusage(libc::RUSAGE_SELF, &mut usage) != 0 {
            return None;
        }
        usage
    };

    Some(CpuSnapshot {
        user: to_duration(usage.ru_utime),
        kernel: to_duration(usage.ru_stime),
    })
}

#[cfg(not(any(windows, unix)))]
fn read() -> Option<CpuSnapshot> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn burning_cpu_shows_up_in_the_snapshot() {
        // Burn until the counter moves, rather than for a fixed count of
        // iterations.
        //
        // A fixed count cannot be right in both profiles. Windows quantises
        // process CPU time at 15.6 ms, and the eight million multiplies this
        // used to do were sized against a debug build: with the optimiser on
        // they finish well inside one tick, the counter reads zero, and the
        // test fails. It did exactly that — `cargo test` green, `cargo test
        // --release` red, deterministically, for as long as both existed.
        //
        // The deadline is a backstop for a machine whose counter never moves
        // at all, so the failure is a failed assertion and not a hung suite.
        let start = CpuSnapshot::now();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut acc = 0u64;
        let mut usage = start.elapsed_since();
        while usage.total() == Duration::ZERO && std::time::Instant::now() < deadline {
            for i in 0..2_000_000u64 {
                acc = acc.wrapping_add(i.wrapping_mul(2_654_435_761));
            }
            std::hint::black_box(acc);
            usage = start.elapsed_since();
        }

        assert!(usage.total() > Duration::ZERO, "за 5 с сжигания счётчик ЦП не сдвинулся");
        assert!(usage.cores >= 1);
    }

    #[test]
    fn shares_are_zero_for_a_zero_length_run() {
        let usage = CpuUsage { user: Duration::from_secs(1), kernel: Duration::ZERO, cores: 4 };
        assert_eq!(usage.cores_busy(Duration::ZERO), 0.0);
        assert_eq!(usage.machine_share(Duration::ZERO), 0.0);
    }

    #[test]
    fn one_core_fully_busy_reads_as_one_core() {
        let usage = CpuUsage { user: Duration::from_secs(2), kernel: Duration::ZERO, cores: 4 };
        assert!((usage.cores_busy(Duration::from_secs(2)) - 1.0).abs() < 1e-9);
        assert!((usage.machine_share(Duration::from_secs(2)) - 0.25).abs() < 1e-9);
    }
}
