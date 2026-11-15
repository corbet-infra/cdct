//! Policy data: every threshold the conductor uses, with sensible defaults.
//!
//! Nothing in the decision code carries a bare constant. A deployment overrides
//! these values as JSON (the CLI's `--policy`); a missing field keeps its
//! default, so a file names only what the deployment changes.

use cdrv::Timestamp;
use serde::{Deserialize, Serialize};

/// Everything one deployment may tune.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Policy {
    /// RAM admission policy.
    pub memory: MemoryPolicy,
    /// GPU ladder policy.
    pub gpu: GpuPolicy,
}

/// RAM admission policy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MemoryPolicy {
    /// The fixed claim of every GPU VM, in MiB (24 GiB).
    pub gpu_vm_claim_mib: u64,
    /// How much swap may cover a shortfall.
    pub swap: SwapPolicy,
}

impl Default for MemoryPolicy {
    fn default() -> Self {
        Self {
            gpu_vm_claim_mib: 24 * 1024,
            swap: SwapPolicy::default(),
        }
    }
}

/// When swap may cover the gap between available memory and a claim.
///
/// Swap is far slower than RAM, so it is used only for a bounded gap and only
/// while memory pressure shows the host is not already thrashing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SwapPolicy {
    /// Whether swap may cover a gap at all.
    pub enabled: bool,
    /// The largest gap, in MiB, that swap may cover.
    pub max_gap_mib: u64,
    /// `some avg10` at or below which swap is acceptable, in percent.
    pub some_avg10_limit: f32,
    /// `full avg10` at or below which swap is acceptable, in percent.
    pub full_avg10_limit: f32,
}

impl Default for SwapPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            max_gap_mib: 8 * 1024,
            some_avg10_limit: 10.0,
            full_avg10_limit: 5.0,
        }
    }
}

/// GPU ladder and starvation policy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GpuPolicy {
    /// The VRAM-used fraction above which the card counts as full (0.85).
    pub vram_full_fraction: f32,
    /// GTT growth per tick below which spill is noise, in bytes (64 MiB).
    pub gtt_delta_bytes: u64,
    /// Ticks a tenant must stay starved before it justifies a kill.
    pub grace_ticks: u32,
    /// How long after a reclaim the ladder waits for the freed VRAM to land.
    pub cooldown_secs: u64,
    /// The synthetic priority of the desktop session; above the top ladder rung.
    pub desktop_priority: u32,
    /// The synthetic priority of the broker model-load starvation signal.
    pub broker_priority: u32,
    /// Consecutive observations a guarded resource must read zero, while the
    /// device plugin runs, before the plugin is restarted (5).
    pub guard_grace_ticks: u32,
    /// How long after a device plugin restart the next one may be decided (300 s).
    pub guard_cooldown_secs: u64,
}

impl GpuPolicy {
    /// The instant a device plugin restart at `restarted_at` finishes its cooldown.
    #[must_use]
    pub fn guard_cooldown_until(&self, restarted_at: Timestamp) -> Timestamp {
        Timestamp::from_millis(
            restarted_at
                .as_millis()
                .saturating_add(self.guard_cooldown_secs.saturating_mul(1000)),
        )
    }

    /// The instant a reclaim at `reclaimed_at` finishes its cooldown.
    ///
    /// The daemon is event-driven, so the cooldown is a deadline in time, not a
    /// count of ticks nobody is guaranteed to deliver.
    #[must_use]
    pub fn cooldown_until(&self, reclaimed_at: Timestamp) -> Timestamp {
        Timestamp::from_millis(
            reclaimed_at
                .as_millis()
                .saturating_add(self.cooldown_secs.saturating_mul(1000)),
        )
    }
}

impl Default for GpuPolicy {
    fn default() -> Self {
        Self {
            vram_full_fraction: 0.85,
            gtt_delta_bytes: 64 * 1024 * 1024,
            grace_ticks: 2,
            cooldown_secs: 18,
            desktop_priority: 2_000_000,
            broker_priority: 1_000,
            guard_grace_ticks: 5,
            guard_cooldown_secs: 300,
        }
    }
}

/// A policy load failure.
#[derive(Debug, thiserror::Error)]
pub enum PolicyError {
    /// The policy file could not be read.
    #[error("cannot read policy {path}: {source}")]
    Read {
        /// The file that failed.
        path: String,
        /// The underlying I/O error.
        source: std::io::Error,
    },
    /// The policy file is not valid JSON for these types.
    #[error("policy {path} is invalid: {source}")]
    Json {
        /// The file that failed.
        path: String,
        /// The underlying JSON error.
        source: serde_json::Error,
    },
}

impl Policy {
    /// Parse a policy from JSON text. Missing fields take their defaults.
    pub fn from_json(text: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(text)
    }

    /// Read a policy file; `None` keeps the defaults.
    pub fn load(path: Option<&std::path::Path>) -> Result<Self, PolicyError> {
        let Some(path) = path else {
            return Ok(Self::default());
        };
        let text = std::fs::read_to_string(path).map_err(|source| PolicyError::Read {
            path: path.display().to_string(),
            source,
        })?;
        Self::from_json(&text).map_err(|source| PolicyError::Json {
            path: path.display().to_string(),
            source,
        })
    }
}
