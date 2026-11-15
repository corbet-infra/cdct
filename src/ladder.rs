//! The live ladder: what the conductor decides when the executor reports the
//! card under pressure, and when a tenant it shed may come back.
//!
//! The executor observes (pod, node and broker events, the card's VRAM read at
//! each event) and reports a [`PressureObservation`]; the conductor decides.
//! [`crate::gpu::decide_ladder`] names the tenant to shed; [`decide_restore`]
//! names a shed tenant that may return. Both are pure functions of the
//! observation, the shed list and the cooldown deadline, so the daemon needs
//! no timer: every decision is taken at an event.
//!
//! A tenant comes back only when the VRAM it freed is free again. The freed
//! amount is measured, never declared: the VRAM in use when the shed was
//! decided, minus the VRAM in use at the first observation after the cooldown.

use cdrv::{KernelOwner, Timestamp};
use serde::{Deserialize, Serialize};

use crate::gpu::{GpuLadder, GpuTenant, StarvationSignal, VramFacts};
use crate::policy::GpuPolicy;

/// What the executor observed at one event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PressureObservation {
    /// The card's VRAM counters, when the host driver reports them.
    pub vram: Option<VramFacts>,
    /// The host-side tenants and their observed states.
    pub tenants: Vec<GpuTenant>,
    /// The starvation signals, each with the number of consecutive
    /// observations it has lasted.
    pub signals: Vec<StarvationSignal>,
}

/// A host tenant the ladder scaled to zero.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShedTenant {
    /// The tenant's name in the platform.
    pub tenant: String,
    /// Its ladder rung.
    pub ladder: GpuLadder,
    /// The replicas it ran before the shed. The executor records the live
    /// count before it stops the tenant; 0 until then, and a tenant recorded
    /// at 0 is never started again.
    pub replicas: u32,
    /// When the shed was decided.
    pub at: Timestamp,
    /// The VRAM in use when the shed was decided, in bytes.
    pub vram_used_at_shed: Option<u64>,
    /// The VRAM the shed freed, measured at the first observation after the
    /// cooldown; `None` until then.
    #[serde(default)]
    pub freed_bytes: Option<u64>,
}

/// What the ladder decides about a shed tenant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestoreDecision {
    /// Start the tenant again with the replicas it had.
    Restore {
        /// The tenant.
        tenant: String,
        /// Its ladder rung.
        ladder: GpuLadder,
        /// The replicas to start.
        replicas: u32,
    },
    /// Nothing returns now, with the reason.
    None {
        /// Why.
        reason: RestoreHold,
    },
}

/// Why no shed tenant returns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestoreHold {
    /// Nothing was shed (or every recorded count is zero).
    NothingShed,
    /// The card belongs to a guest.
    NotHostOwned,
    /// A shed or a restore is still landing.
    Cooldown {
        /// When the cooldown ends.
        until: Timestamp,
    },
    /// A tenant is starved right now.
    Starved,
    /// No VRAM reading, or the freed amount is not measured yet.
    NotMeasured,
    /// The VRAM the tenant freed is not free.
    NoRoom {
        /// Free VRAM now, in bytes.
        free_bytes: u64,
        /// The VRAM the tenant freed, in bytes.
        needed_bytes: u64,
    },
}

/// The VRAM a shed freed, measured from an observation taken after its
/// cooldown: the usage when the shed was decided minus the usage now.
#[must_use]
pub fn measure_freed(shed: &ShedTenant, vram: Option<VramFacts>) -> Option<u64> {
    let before = shed.vram_used_at_shed?;
    let now = vram?;
    Some(before.saturating_sub(now.used_bytes))
}

/// Decide whether a shed tenant returns now.
///
/// Gates, in order: something was shed with a known count, the host owns the
/// card, no cooldown runs, no starvation signal is graced, and the VRAM the
/// tenant freed is free again. The highest rung returns first (ties on the
/// name), one tenant per decision; the cooldown then separates the next.
#[must_use]
pub fn decide_restore(
    owner: &KernelOwner,
    observation: &PressureObservation,
    shed: &[ShedTenant],
    now: Timestamp,
    cooldown_until: Timestamp,
    policy: &GpuPolicy,
) -> RestoreDecision {
    let Some(candidate) = shed
        .iter()
        .filter(|tenant| tenant.replicas > 0)
        .max_by(|left, right| {
            left.ladder
                .priority()
                .cmp(&right.ladder.priority())
                .then_with(|| right.tenant.cmp(&left.tenant))
        })
    else {
        return RestoreDecision::None {
            reason: RestoreHold::NothingShed,
        };
    };
    if !owner.is_host() {
        return RestoreDecision::None {
            reason: RestoreHold::NotHostOwned,
        };
    }
    if now < cooldown_until {
        return RestoreDecision::None {
            reason: RestoreHold::Cooldown {
                until: cooldown_until,
            },
        };
    }
    if observation
        .signals
        .iter()
        .any(|signal| signal.is_graced(policy))
    {
        return RestoreDecision::None {
            reason: RestoreHold::Starved,
        };
    }
    let (Some(vram), Some(needed)) = (observation.vram, candidate.freed_bytes) else {
        return RestoreDecision::None {
            reason: RestoreHold::NotMeasured,
        };
    };
    let free = vram.total_bytes.saturating_sub(vram.used_bytes);
    if free < needed || vram.is_full(policy.vram_full_fraction) {
        return RestoreDecision::None {
            reason: RestoreHold::NoRoom {
                free_bytes: free,
                needed_bytes: needed,
            },
        };
    }
    RestoreDecision::Restore {
        tenant: candidate.tenant.clone(),
        ladder: candidate.ladder,
        replicas: candidate.replicas,
    }
}

/// Whether the device plugin is restarted now: the host owns the card, no
/// guard cooldown runs, and a guarded resource has read zero for at least the
/// policy's grace while the plugin runs. While a guest owns the card the
/// resources read zero by design, so nothing is restarted.
#[must_use]
pub fn decide_guard(
    owner: &KernelOwner,
    observation: &PressureObservation,
    now: Timestamp,
    guard_cooldown_until: Timestamp,
    policy: &GpuPolicy,
) -> bool {
    owner.is_host()
        && now >= guard_cooldown_until
        && observation.signals.iter().any(|signal| {
            matches!(
                signal,
                StarvationSignal::RegistrationZombie { starved_ticks, .. }
                    if *starved_ticks >= policy.guard_grace_ticks
            )
        })
}
