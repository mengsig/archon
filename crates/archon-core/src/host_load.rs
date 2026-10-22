//! The host-load hold's pure part (ARCHITECTURE §7.3): parsing `/proc`
//! readings and judging them against `[limits]` with hysteresis, so dispatch
//! does not flap around a limit.

use serde::{Deserialize, Serialize};

use crate::error::ArchonError;

/// A held host resumes once load is below this fraction of its limit.
pub const RESUME_LOAD_FRACTION: f64 = 0.9;
/// A held host resumes once available memory is above its limit times this.
pub const RESUME_MEMORY_FACTOR: f64 = 1.1;

const KIB_PER_GIB: f64 = 1024.0 * 1024.0;

/// One reading of the host.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct HostSample {
    /// The 1-minute load average divided by the CPU count.
    pub load_per_cpu: f64,
    /// `MemAvailable`, in GiB.
    pub available_gb: f64,
}

/// The `[limits]` keys the hold judges by; `None` leaves that check off.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HostLimits {
    pub max_load_per_cpu: Option<f64>,
    pub min_available_memory_gb: Option<f64>,
}

/// A measured value past its limit.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Breach {
    pub measured: f64,
    pub limit: f64,
}

/// Why dispatch is held for the host: at least one of the two is set.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostLoadHold {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load: Option<Breach>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<Breach>,
}

impl HostLoadHold {
    /// What happened, naming the measured values and the limits.
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if let Some(b) = &self.load {
            parts.push(format!(
                "host load {:.2}/cpu is above [limits] max_load_per_cpu {:.2}",
                b.measured, b.limit
            ));
        }
        if let Some(b) = &self.memory {
            parts.push(format!(
                "available memory {:.2} GiB is below [limits] min_available_memory_gb {:.2}",
                b.measured, b.limit
            ));
        }
        assert!(!parts.is_empty(), "a host hold names a breach");
        format!("Dispatch held: {}", parts.join("; "))
    }

    /// What the operator can do about it.
    pub fn next_action(&self) -> String {
        let mut steps = Vec::new();
        if let Some(b) = &self.load {
            steps.push(format!(
                "waiting for host load to drop below {:.2}/cpu; raise [limits] max_load_per_cpu to change",
                b.limit * RESUME_LOAD_FRACTION
            ));
        }
        if let Some(b) = &self.memory {
            steps.push(format!(
                "waiting for available memory to rise above {:.2} GiB; lower [limits] min_available_memory_gb to change",
                b.limit * RESUME_MEMORY_FACTOR
            ));
        }
        assert!(!steps.is_empty(), "a host hold names a breach");
        steps.join("; ")
    }
}

/// Judge `sample` against `limits`. A resource already held (`previous`)
/// stays held until it clears its limit by the hysteresis margin.
pub fn evaluate(
    previous: Option<&HostLoadHold>,
    sample: HostSample,
    limits: HostLimits,
) -> Option<HostLoadHold> {
    assert!(
        sample.load_per_cpu >= 0.0 && sample.available_gb >= 0.0,
        "a host reading is not negative"
    );
    let load = limits.max_load_per_cpu.and_then(|limit| {
        let held = previous.is_some_and(|p| p.load.is_some());
        let over = if held {
            sample.load_per_cpu >= limit * RESUME_LOAD_FRACTION
        } else {
            sample.load_per_cpu > limit
        };
        over.then_some(Breach {
            measured: sample.load_per_cpu,
            limit,
        })
    });
    let memory = limits.min_available_memory_gb.and_then(|limit| {
        let held = previous.is_some_and(|p| p.memory.is_some());
        let under = if held {
            sample.available_gb <= limit * RESUME_MEMORY_FACTOR
        } else {
            sample.available_gb < limit
        };
        under.then_some(Breach {
            measured: sample.available_gb,
            limit,
        })
    });
    (load.is_some() || memory.is_some()).then_some(HostLoadHold { load, memory })
}

/// The 1-minute load average, the first field of `/proc/loadavg`.
pub fn parse_loadavg(text: &str) -> Result<f64, ArchonError> {
    let field = text.split_whitespace().next().ok_or_else(|| {
        ArchonError::Unavailable("/proc/loadavg is empty; expected \"0.52 0.58 ...\"".into())
    })?;
    let load: f64 = field.parse().map_err(|e| {
        ArchonError::Unavailable(format!(
            "/proc/loadavg starts with {field:?}, not a number: {e}"
        ))
    })?;
    if !load.is_finite() || load < 0.0 {
        return Err(ArchonError::Unavailable(format!(
            "/proc/loadavg reports a load of {load}"
        )));
    }
    Ok(load)
}

/// `MemAvailable` of `/proc/meminfo`, in GiB.
pub fn parse_meminfo_available_gb(text: &str) -> Result<f64, ArchonError> {
    let line = text
        .lines()
        .find_map(|l| l.strip_prefix("MemAvailable:"))
        .ok_or_else(|| {
            ArchonError::Unavailable(
                "/proc/meminfo has no MemAvailable line (the kernel is older than 3.14)".into(),
            )
        })?;
    let kib: f64 = line
        .trim()
        .trim_end_matches("kB")
        .trim()
        .parse()
        .map_err(|e| {
            ArchonError::Unavailable(format!(
                "/proc/meminfo MemAvailable is {:?}, not a size in kB: {e}",
                line.trim()
            ))
        })?;
    Ok(kib / KIB_PER_GIB)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMITS: HostLimits = HostLimits {
        max_load_per_cpu: Some(1.5),
        min_available_memory_gb: Some(2.0),
    };

    fn sample(load_per_cpu: f64, available_gb: f64) -> HostSample {
        HostSample {
            load_per_cpu,
            available_gb,
        }
    }

    #[test]
    fn loadavg_fixture_reads_the_one_minute_field() {
        let load = parse_loadavg("3.21 2.50 1.10 4/1234 56789\n").unwrap();
        assert_eq!(load, 3.21);
    }

    #[test]
    fn loadavg_garbage_is_refused_with_what_was_read() {
        for bad in ["", "   \n", "abc 1 2", "-1.0 0 0", "NaN 0 0"] {
            let error = parse_loadavg(bad).unwrap_err().to_string();
            assert!(error.contains("/proc/loadavg"), "{bad:?}: {error}");
        }
    }

    #[test]
    fn meminfo_fixture_reads_mem_available_in_gib() {
        let text = "MemTotal:       16384000 kB\nMemFree:          100000 kB\n\
                    MemAvailable:    2097152 kB\nBuffers:           1000 kB\n";
        assert_eq!(parse_meminfo_available_gb(text).unwrap(), 2.0);
    }

    #[test]
    fn meminfo_without_mem_available_or_with_junk_is_refused() {
        for bad in ["MemTotal: 1 kB\n", "MemAvailable: lots kB\n", ""] {
            let error = parse_meminfo_available_gb(bad).unwrap_err().to_string();
            assert!(error.contains("/proc/meminfo"), "{bad:?}: {error}");
        }
    }

    #[test]
    fn load_holds_above_the_limit_and_releases_only_below_ninety_percent() {
        let held = evaluate(None, sample(1.6, 8.0), LIMITS).expect("over the limit");
        assert_eq!(held.load.unwrap().measured, 1.6);
        assert!(held.memory.is_none());
        assert!(
            evaluate(None, sample(1.5, 8.0), LIMITS).is_none(),
            "at the limit"
        );
        let still = evaluate(Some(&held), sample(1.4, 8.0), LIMITS);
        assert!(still.is_some(), "1.4 is not below 1.35");
        assert!(evaluate(still.as_ref(), sample(1.35, 8.0), LIMITS).is_some());
        assert!(evaluate(still.as_ref(), sample(1.34, 8.0), LIMITS).is_none());
    }

    #[test]
    fn memory_holds_below_the_limit_and_releases_only_above_ten_percent_over() {
        let held = evaluate(None, sample(0.1, 1.9), LIMITS).expect("under the limit");
        assert!(held.load.is_none());
        assert!(
            evaluate(None, sample(0.1, 2.0), LIMITS).is_none(),
            "at the limit"
        );
        let still = evaluate(Some(&held), sample(0.1, 2.1), LIMITS);
        assert!(still.is_some(), "2.1 is not above 2.2");
        assert!(evaluate(still.as_ref(), sample(0.1, 2.3), LIMITS).is_none());
    }

    #[test]
    fn a_limit_left_unset_is_never_judged() {
        let off = HostLimits {
            max_load_per_cpu: None,
            min_available_memory_gb: None,
        };
        assert!(evaluate(None, sample(99.0, 0.0), off).is_none());
    }

    #[test]
    fn both_breaches_are_named_with_the_next_step() {
        let held = evaluate(None, sample(3.2, 0.5), LIMITS).unwrap();
        let said = held.describe();
        assert!(said.contains("3.20/cpu") && said.contains("1.50"), "{said}");
        assert!(said.contains("0.50 GiB") && said.contains("2.00"), "{said}");
        let next = held.next_action();
        assert!(next.contains("below 1.35/cpu"), "{next}");
        assert!(next.contains("raise [limits] max_load_per_cpu"), "{next}");
        assert!(next.contains("above 2.20 GiB"), "{next}");
    }
}
