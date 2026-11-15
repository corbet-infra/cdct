//! The ported memory guard and the RAM admission decision.
//!
//! The guard is the one the rest of the fleet uses: headroom is the kernel's
//! `MemAvailable` plus the ZFS ARC the shrinker may release, that ARC capped at
//! `size - c_min`. RAM is never taken from a running instance: a target that
//! does not fit waits (or is declined when the claim cannot fit in physical
//! RAM), and swap may cover a bounded gap only while memory pressure is calm.
//!
//! A GPU VM's claim is pinned by VFIO, so it must fit in `MemTotal`: swap can
//! only cover part of a claim by moving *other* host memory out, never by
//! paging the pinned claim itself.

use serde::{Deserialize, Serialize};

use crate::facts::{MemorySnapshot, Pressure};
use crate::policy::{MemoryPolicy, SwapPolicy};

/// The headroom the guard may spend, in MiB.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryHeadroom {
    /// The kernel's `MemAvailable`, in MiB.
    pub mem_available_mib: u64,
    /// Reclaimable ZFS ARC, capped at `size - c_min`, in MiB.
    pub evictable_arc_mib: u64,
}

impl MemoryHeadroom {
    /// The full headroom: available memory plus reclaimable ARC.
    #[must_use]
    pub const fn available_mib(&self) -> u64 {
        self.mem_available_mib
            .saturating_add(self.evictable_arc_mib)
    }

    /// Compute the headroom from parsed facts.
    #[must_use]
    pub fn from_snapshot(snapshot: &MemorySnapshot) -> Self {
        Self {
            mem_available_mib: snapshot.meminfo.available_mib(),
            evictable_arc_mib: snapshot.arcstats.map_or(0, |stats| stats.evictable_mib()),
        }
    }
}

/// The grant a target receives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryAdmission {
    /// The claim that was granted, in MiB.
    pub required_mib: u64,
    /// The headroom the decision saw, including memory the plan frees, in MiB.
    pub available_mib: u64,
    /// The part of the headroom that comes from instances the plan stops
    /// before the target starts, in MiB.
    pub credited_mib: u64,
    /// The part of the claim that only swap can cover, in MiB (zero for a
    /// grant that fits in free memory).
    pub via_swap_mib: u64,
}

/// Why a target is not admitted now, or never.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryRefusal {
    /// The headroom does not cover the claim, and swap may not or cannot cover
    /// the rest. Memory can free, so the target waits rather than displacing
    /// anyone.
    NotEnoughFree {
        /// The headroom seen, in MiB.
        available_mib: u64,
        /// The claim, in MiB.
        required_mib: u64,
        /// The uncovered part, in MiB.
        gap_mib: u64,
        /// The host's free swap, in MiB.
        swap_free_mib: u64,
        /// The largest gap swap may cover, in MiB.
        max_swap_gap_mib: u64,
    },
    /// Swap could cover the gap, but memory pressure is already too high for
    /// that to be safe.
    SwapNotAllowedUnderPressure {
        /// The gap swap would have to cover, in MiB.
        gap_mib: u64,
        /// The observed `some avg10`, when reported.
        some_avg10: Option<f32>,
        /// The observed `full avg10`, when reported.
        full_avg10: Option<f32>,
        /// The `some avg10` limit.
        some_limit: f32,
        /// The `full avg10` limit.
        full_limit: f32,
    },
    /// The claim is larger than the host's physical RAM. The claim is pinned,
    /// so swap cannot make it fit: waiting cannot help either.
    ClaimExceedsHostCapacity {
        /// The claim, in MiB.
        required_mib: u64,
        /// Physical RAM (`MemTotal`), in MiB.
        ram_mib: u64,
    },
}

/// The one RAM outcome: a grant, a wait, or a decline.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryDecision {
    /// The claim fits (possibly with swap).
    Admit(MemoryAdmission),
    /// The claim may fit once memory frees; nothing is displaced.
    Wait(MemoryRefusal),
    /// The claim can never fit on this host.
    Decline(MemoryRefusal),
}

/// Whether memory pressure is low enough for swap to cover a gap.
///
/// A missing PSI line counts as severe: swap is never allowed on incomplete
/// pressure facts.
#[must_use]
pub fn pressure_allows_swap(pressure: &Pressure, policy: &SwapPolicy) -> bool {
    match (pressure.some_avg10(), pressure.full_avg10()) {
        (Some(some), Some(full)) => {
            some <= policy.some_avg10_limit && full <= policy.full_avg10_limit
        }
        _ => false,
    }
}

/// Decide RAM for a target with a fixed claim.
///
/// The rules, in order:
/// 1. a claim beyond physical RAM is declined (the claim is pinned, so swap
///    cannot make it fit);
/// 2. a claim covered by headroom is admitted;
/// 3. a gap within `max_gap` and free swap is admitted while pressure is calm;
/// 4. the same gap under pressure waits for pressure to subside;
/// 5. every other shortfall waits. Nothing is displaced.
#[must_use]
pub fn decide_memory(
    snapshot: &MemorySnapshot,
    required_mib: u64,
    policy: &MemoryPolicy,
) -> MemoryDecision {
    decide_memory_with_credit(snapshot, required_mib, 0, policy)
}

/// Decide RAM for a target, crediting memory the plan itself frees.
///
/// `credited_mib` is the resident memory of instances the switch plan stops
/// before the target starts (the guest that currently holds the card, and the
/// office profile paired with a Windows target). Those stops are already in the
/// plan, so the target may rely on that memory; unlike a plain shortfall, it is
/// not a reason to wait. The rules are otherwise those of [`decide_memory`], and
/// the physical-RAM gate ignores the credit: freeing memory cannot make a
/// pinned claim fit in RAM it does not have.
#[must_use]
pub fn decide_memory_with_credit(
    snapshot: &MemorySnapshot,
    required_mib: u64,
    credited_mib: u64,
    policy: &MemoryPolicy,
) -> MemoryDecision {
    let headroom = MemoryHeadroom::from_snapshot(snapshot);
    let available_mib = headroom.available_mib().saturating_add(credited_mib);
    let swap_free_mib = snapshot.meminfo.swap_free_mib();
    let ram_mib = snapshot.meminfo.total_mib();
    if required_mib > ram_mib {
        return MemoryDecision::Decline(MemoryRefusal::ClaimExceedsHostCapacity {
            required_mib,
            ram_mib,
        });
    }
    if available_mib >= required_mib {
        return MemoryDecision::Admit(MemoryAdmission {
            required_mib,
            available_mib,
            credited_mib,
            via_swap_mib: 0,
        });
    }
    let gap_mib = required_mib - available_mib;
    let swap_may_cover =
        policy.swap.enabled && gap_mib <= swap_free_mib && gap_mib <= policy.swap.max_gap_mib;
    if swap_may_cover {
        if pressure_allows_swap(&snapshot.pressure, &policy.swap) {
            return MemoryDecision::Admit(MemoryAdmission {
                required_mib,
                available_mib,
                credited_mib,
                via_swap_mib: gap_mib,
            });
        }
        return MemoryDecision::Wait(MemoryRefusal::SwapNotAllowedUnderPressure {
            gap_mib,
            some_avg10: snapshot.pressure.some_avg10(),
            full_avg10: snapshot.pressure.full_avg10(),
            some_limit: policy.swap.some_avg10_limit,
            full_limit: policy.swap.full_avg10_limit,
        });
    }
    MemoryDecision::Wait(MemoryRefusal::NotEnoughFree {
        available_mib,
        required_mib,
        gap_mib,
        swap_free_mib,
        max_swap_gap_mib: policy.swap.max_gap_mib,
    })
}
