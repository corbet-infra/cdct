//! Pure parsers for the host's memory facts.
//!
//! The conductor never reads a file itself: the thin [`crate::reader`] turns
//! bytes into text, and these functions turn text into facts. Tests feed
//! fixture text, so every parsing rule is exercised without a host.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// A parse failure with the input that caused it.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ParseError {
    /// A required key was absent.
    #[error("{file} has no {key}")]
    Missing {
        /// The file the text came from.
        file: &'static str,
        /// The key that was required.
        key: &'static str,
    },
    /// A value was present but not a number.
    #[error("{file}: {key} has a non-numeric value {value:?}")]
    NotANumber {
        /// The file the text came from.
        file: &'static str,
        /// The key whose value failed.
        key: String,
        /// The offending value.
        value: String,
    },
}

/// The parts of `/proc/meminfo` an admission decision uses, in KiB.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemInfo {
    /// `MemTotal`.
    pub total_kib: u64,
    /// `MemAvailable`.
    pub available_kib: u64,
    /// `SwapTotal`; zero when the host has no swap.
    pub swap_total_kib: u64,
    /// `SwapFree`; zero when the host has no swap.
    pub swap_free_kib: u64,
}

impl MemInfo {
    /// `MemTotal` in MiB.
    #[must_use]
    pub const fn total_mib(&self) -> u64 {
        self.total_kib / 1024
    }

    /// `MemAvailable` in MiB.
    #[must_use]
    pub const fn available_mib(&self) -> u64 {
        self.available_kib / 1024
    }

    /// `SwapTotal` in MiB.
    #[must_use]
    pub const fn swap_total_mib(&self) -> u64 {
        self.swap_total_kib / 1024
    }

    /// `SwapFree` in MiB.
    #[must_use]
    pub const fn swap_free_mib(&self) -> u64 {
        self.swap_free_kib / 1024
    }

    /// Memory the host can ever give an instance: RAM plus swap.
    #[must_use]
    pub const fn capacity_mib(&self) -> u64 {
        self.total_mib().saturating_add(self.swap_total_mib())
    }
}

/// Parse `/proc/meminfo`.
///
/// `MemTotal` and `MemAvailable` are required: without either an admission
/// decision cannot be made safely, so the parse fails closed. The swap lines
/// are optional and default to zero (a host without swap).
pub fn parse_meminfo(text: &str) -> Result<MemInfo, ParseError> {
    let mut total = None;
    let mut available = None;
    let mut swap_total = 0;
    let mut swap_free = 0;
    for line in text.lines() {
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let mut fields = rest.split_whitespace();
        let Some(raw) = fields.next() else {
            continue;
        };
        let value: u64 = raw.parse().map_err(|_| ParseError::NotANumber {
            file: "meminfo",
            key: key.to_owned(),
            value: raw.to_owned(),
        })?;
        match key {
            "MemTotal" => total = Some(value),
            "MemAvailable" => available = Some(value),
            "SwapTotal" => swap_total = value,
            "SwapFree" => swap_free = value,
            _ => {}
        }
    }
    Ok(MemInfo {
        total_kib: total.ok_or(ParseError::Missing {
            file: "meminfo",
            key: "MemTotal",
        })?,
        available_kib: available.ok_or(ParseError::Missing {
            file: "meminfo",
            key: "MemAvailable",
        })?,
        swap_total_kib: swap_total,
        swap_free_kib: swap_free,
    })
}

/// The ZFS ARC counters that make ARC memory reclaimable, in bytes.
///
/// `size` and `c_min` bound how much of the ARC the shrinker must keep; the
/// four `evictable` counters are what it may release under pressure.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArcStats {
    /// The ARC's current size.
    pub size_bytes: u64,
    /// The ARC minimum, `c_min`.
    pub c_min_bytes: u64,
    /// Evictable MRU data bytes.
    pub mru_evictable_data: u64,
    /// Evictable MRU metadata bytes.
    pub mru_evictable_metadata: u64,
    /// Evictable MFU data bytes.
    pub mfu_evictable_data: u64,
    /// Evictable MFU metadata bytes.
    pub mfu_evictable_metadata: u64,
}

impl ArcStats {
    /// The sum of the four evictable counters.
    #[must_use]
    pub const fn evictable_sum_bytes(&self) -> u64 {
        self.mru_evictable_data
            .saturating_add(self.mru_evictable_metadata)
            .saturating_add(self.mfu_evictable_data)
            .saturating_add(self.mfu_evictable_metadata)
    }

    /// The ARC space the shrinker may release: the evictable sum, capped at
    /// `size - c_min`.
    ///
    /// Below `c_min` the kernel does not shrink the ARC at all, so no part of
    /// the evictable counters above that floor is usable headroom.
    #[must_use]
    pub const fn evictable_bytes(&self) -> u64 {
        let cap = self.size_bytes.saturating_sub(self.c_min_bytes);
        if self.evictable_sum_bytes() < cap {
            self.evictable_sum_bytes()
        } else {
            cap
        }
    }

    /// The reclaimable ARC in MiB.
    #[must_use]
    pub const fn evictable_mib(&self) -> u64 {
        self.evictable_bytes() / 1_048_576
    }
}

/// Parse `/proc/spl/kstat/zfs/arcstats`.
///
/// The file is whitespace-separated `name type data` rows. Unknown rows are
/// ignored; a counter that is absent counts as zero, which is the safe
/// direction (less headroom). A malformed value on a counter we use fails the
/// parse rather than silently inflating headroom.
pub fn parse_arcstats(text: &str) -> Result<ArcStats, ParseError> {
    let mut stats = ArcStats::default();
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let (Some(name), Some(_kind), Some(raw)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let slot = match name {
            "size" => &mut stats.size_bytes,
            "c_min" => &mut stats.c_min_bytes,
            "mru_evictable_data" => &mut stats.mru_evictable_data,
            "mru_evictable_metadata" => &mut stats.mru_evictable_metadata,
            "mfu_evictable_data" => &mut stats.mfu_evictable_data,
            "mfu_evictable_metadata" => &mut stats.mfu_evictable_metadata,
            _ => continue,
        };
        *slot = raw.parse().map_err(|_| ParseError::NotANumber {
            file: "arcstats",
            key: name.to_owned(),
            value: raw.to_owned(),
        })?;
    }
    Ok(stats)
}

/// One line of a PSI file: the pressure shares and the total stall time.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PressureShare {
    /// The 10-second average stall share, in percent.
    pub avg10: f32,
    /// The 60-second average stall share, in percent.
    pub avg60: f32,
    /// The 300-second average stall share, in percent.
    pub avg300: f32,
    /// Total stall time in microseconds.
    pub total_us: u64,
}

/// The `some` and `full` lines of `/proc/pressure/memory`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Pressure {
    /// The `some` line: at least one task stalled.
    pub some: Option<PressureShare>,
    /// The `full` line: every task stalled.
    pub full: Option<PressureShare>,
}

impl Pressure {
    /// The `some avg10` share, when reported.
    #[must_use]
    pub fn some_avg10(&self) -> Option<f32> {
        self.some.map(|share| share.avg10)
    }

    /// The `full avg10` share, when reported.
    #[must_use]
    pub fn full_avg10(&self) -> Option<f32> {
        self.full.map(|share| share.avg10)
    }
}

/// Parse `/proc/pressure/memory`.
///
/// A file with neither a `some` nor a `full` line is an error; a missing line
/// of the pair is allowed (the caller treats a missing `full` line as severe
/// pressure, never as calm).
pub fn parse_pressure(text: &str) -> Result<Pressure, ParseError> {
    let mut pressure = Pressure {
        some: None,
        full: None,
    };
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let Some(kind) = fields.next() else {
            continue;
        };
        let slot = match kind {
            "some" => &mut pressure.some,
            "full" => &mut pressure.full,
            _ => continue,
        };
        let mut share = PressureShare {
            avg10: 0.0,
            avg60: 0.0,
            avg300: 0.0,
            total_us: 0,
        };
        for field in fields {
            let Some((key, value)) = field.split_once('=') else {
                continue;
            };
            match key {
                "avg10" | "avg60" | "avg300" => {
                    let parsed: f32 = value.parse().map_err(|_| ParseError::NotANumber {
                        file: "pressure/memory",
                        key: key.to_owned(),
                        value: value.to_owned(),
                    })?;
                    match key {
                        "avg10" => share.avg10 = parsed,
                        "avg60" => share.avg60 = parsed,
                        _ => share.avg300 = parsed,
                    }
                }
                "total" => {
                    share.total_us = value.parse().map_err(|_| ParseError::NotANumber {
                        file: "pressure/memory",
                        key: key.to_owned(),
                        value: value.to_owned(),
                    })?;
                }
                _ => {}
            }
        }
        *slot = Some(share);
    }
    if pressure.some.is_none() && pressure.full.is_none() {
        return Err(ParseError::Missing {
            file: "pressure/memory",
            key: "some or full",
        });
    }
    Ok(pressure)
}

/// The raw text facts an admission decision starts from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawFacts {
    /// The contents of `/proc/meminfo`.
    pub meminfo: String,
    /// The contents of the ZFS `arcstats` file, when the host has ZFS.
    pub arcstats: Option<String>,
    /// The contents of `/proc/pressure/memory`.
    pub pressure_memory: String,
}

/// The parsed memory facts a decision uses.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct MemorySnapshot {
    /// The kernel's memory counters.
    pub meminfo: MemInfo,
    /// The ARC counters, when the host has ZFS.
    pub arcstats: Option<ArcStats>,
    /// The memory pressure shares.
    pub pressure: Pressure,
}

impl MemorySnapshot {
    /// Parse the three raw text facts.
    pub fn parse(raw: &RawFacts) -> Result<Self, ParseError> {
        Ok(Self {
            meminfo: parse_meminfo(&raw.meminfo)?,
            arcstats: raw.arcstats.as_deref().map(parse_arcstats).transpose()?,
            pressure: parse_pressure(&raw.pressure_memory)?,
        })
    }
}
