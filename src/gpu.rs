//! The GPU model: one kernel owns the card, and inside the host kernel the
//! tenants share it by the platform's priority ladder.
//!
//! The four starvation signals are typed inputs. The one arbitration rule is a
//! decision that names the lowest-priority running tenant to scale to zero;
//! `cdct` never performs the scale, the bounce or any other device action.

use cdrv::{GuestOs, InstanceClass, InstanceId, InstanceState};
use serde::{Deserialize, Serialize};

use crate::policy::GpuPolicy;

/// Which kernel currently owns the card.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KernelOwner {
    /// The host kernel; its tenants share the card by the ladder.
    Host,
    /// One guest kernel; no host tenant may hold the card.
    Guest(InstanceId),
}

impl KernelOwner {
    /// Whether this is the host kernel.
    #[must_use]
    pub fn is_host(&self) -> bool {
        matches!(self, Self::Host)
    }

    /// The guest instance, when a guest owns the card.
    #[must_use]
    pub fn guest(&self) -> Option<&InstanceId> {
        match self {
            Self::Host => None,
            Self::Guest(id) => Some(id),
        }
    }
}

impl std::fmt::Display for KernelOwner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Host => f.write_str("host"),
            Self::Guest(id) => write!(f, "guest:{id}"),
        }
    }
}

/// The platform's host-side GPU priority ladder, highest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum GpuLadder {
    /// `gpu-desktop`: the interactive desktop session.
    #[serde(rename = "gpu-desktop")]
    Desktop,
    /// `gpu-interactive`: latency-sensitive serving tenants.
    #[serde(rename = "gpu-interactive")]
    Interactive,
    /// `gpu-besteffort`: batch and hobby tenants.
    #[serde(rename = "gpu-besteffort")]
    BestEffort,
}

impl GpuLadder {
    /// The platform PriorityClass value of this rung.
    #[must_use]
    pub const fn priority(self) -> u32 {
        match self {
            Self::Desktop => 1_000_000,
            Self::Interactive => 1_000,
            Self::BestEffort => 100,
        }
    }

    /// The platform's name for this rung.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Desktop => "gpu-desktop",
            Self::Interactive => "gpu-interactive",
            Self::BestEffort => "gpu-besteffort",
        }
    }
}

impl std::fmt::Display for GpuLadder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// One host-side GPU tenant: a pod or the desktop session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpuTenant {
    /// The tenant's name in the platform (a workload name, never host data).
    pub id: String,
    /// Its ladder rung.
    pub ladder: GpuLadder,
    /// Its observed state.
    pub state: TenantState,
}

/// What a GPU tenant is doing right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TenantState {
    /// Running and holding the card.
    Running,
    /// Not ready (usually OOM-restarting) and starved for VRAM.
    NotReady,
    /// Not holding the card.
    Stopped,
}

/// One instance the fact source observed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstanceObservation {
    /// The instance.
    pub id: InstanceId,
    /// Its class.
    pub class: InstanceClass,
    /// Its guest OS.
    pub guest_os: GuestOs,
    /// Its observed state.
    pub state: InstanceState,
    /// Its fixed memory claim, when the class has one.
    pub memory_mib: Option<u64>,
}

/// VRAM counters as the driver reads them from sysfs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct VramFacts {
    /// VRAM in use, in bytes.
    pub used_bytes: u64,
    /// VRAM in total, in bytes.
    pub total_bytes: u64,
}

impl VramFacts {
    /// Whether used VRAM is above `fraction` of total; a card with no reported
    /// total is never "full" (the caller then has no pressure signal).
    #[must_use]
    pub fn is_full(&self, fraction: f32) -> bool {
        if self.total_bytes == 0 {
            return false;
        }
        let used = self.used_bytes as f64;
        let total = self.total_bytes as f64;
        used / total > f64::from(fraction)
    }
}

/// The observed state of the card and every instance near it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpuState {
    /// The hardware-layer name of the card.
    pub device: String,
    /// The one kernel that owns it.
    pub owner: KernelOwner,
    /// Whether the card is currently bound to the passthrough driver.
    pub bound_to_vfio: bool,
    /// The VRAM counters, when the driver reports them.
    pub vram: Option<VramFacts>,
    /// The host-side tenants.
    pub tenants: Vec<GpuTenant>,
    /// Every instance the fact source knows.
    pub instances: Vec<InstanceObservation>,
}

impl Default for GpuState {
    fn default() -> Self {
        Self {
            device: "gpu".to_owned(),
            owner: KernelOwner::Host,
            bound_to_vfio: false,
            vram: None,
            tenants: Vec::new(),
            instances: Vec::new(),
        }
    }
}

impl GpuState {
    /// Whether VRAM is above the policy's full fraction; a host without a VRAM
    /// reading is never "full".
    #[must_use]
    pub fn vram_full(&self, policy: &GpuPolicy) -> bool {
        self.vram
            .is_some_and(|vram| vram.is_full(policy.vram_full_fraction))
    }

    /// One observed instance by identifier.
    #[must_use]
    pub fn instance(&self, id: &InstanceId) -> Option<&InstanceObservation> {
        self.instances.iter().find(|instance| &instance.id == id)
    }

    /// Whether an instance is running (Starting, Running or Stopping; all of
    /// them hold what they were granted).
    #[must_use]
    pub fn is_live(&self, id: &InstanceId) -> bool {
        self.instance(id)
            .is_some_and(|instance| instance.state.is_live())
    }
}

/// One of the four starvation signals the platform's pressure watcher acts on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StarvationSignal {
    /// A GPU pod stays not-ready while the card is full.
    PodNotReady {
        /// The starved tenant.
        tenant: String,
        /// Its ladder rung.
        ladder: GpuLadder,
        /// How many consecutive ticks it has been starved.
        starved_ticks: u32,
    },
    /// The desktop session's graphics VRAM spills to system RAM (GTT).
    DesktopGttSpill {
        /// GTT in use now, in bytes.
        gtt_used_bytes: u64,
        /// GTT in use on the previous tick, in bytes.
        gtt_previous_bytes: u64,
        /// How many consecutive ticks the spill has been seen.
        starved_ticks: u32,
    },
    /// The device plugin's guarded resource reads zero while the plugin runs:
    /// the registration zombie starves every tenant.
    RegistrationZombie {
        /// The extended resource that reads zero.
        resource: String,
        /// How many consecutive ticks it has read zero.
        starved_ticks: u32,
    },
    /// A shared model server keeps a model non-ready while the card is full.
    BrokerModelStarved {
        /// How many models are stuck non-ready.
        stuck_models: u32,
        /// How many consecutive ticks the starvation has been seen.
        starved_ticks: u32,
    },
}

impl StarvationSignal {
    /// The priority the starved tenant effectively holds.
    ///
    /// A pod signal uses its own rung. The desktop and a registration zombie
    /// sit at the synthetic top: the desktop is never a victim, and a zombie
    /// starves every tenant because none can be admitted.
    #[must_use]
    pub fn priority(&self, policy: &GpuPolicy) -> u32 {
        match self {
            Self::PodNotReady { ladder, .. } => ladder.priority(),
            Self::DesktopGttSpill { .. } | Self::RegistrationZombie { .. } => {
                policy.desktop_priority
            }
            Self::BrokerModelStarved { .. } => policy.broker_priority,
        }
    }

    /// Whether the signal has lasted long enough, and is real at all, to
    /// justify a kill.
    #[must_use]
    pub fn is_graced(&self, policy: &GpuPolicy) -> bool {
        match self {
            Self::PodNotReady { starved_ticks, .. }
            | Self::RegistrationZombie { starved_ticks, .. } => {
                *starved_ticks >= policy.grace_ticks
            }
            Self::DesktopGttSpill {
                gtt_used_bytes,
                gtt_previous_bytes,
                starved_ticks,
            } => {
                *starved_ticks >= policy.grace_ticks
                    && gtt_used_bytes.saturating_sub(*gtt_previous_bytes) > policy.gtt_delta_bytes
            }
            Self::BrokerModelStarved {
                stuck_models,
                starved_ticks,
            } => *stuck_models > 0 && *starved_ticks >= policy.grace_ticks,
        }
    }
}

/// Every input one ladder decision sees.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LadderInput {
    /// Which kernel owns the card.
    pub owner: KernelOwner,
    /// Whether VRAM is above the full fraction.
    pub vram_full: bool,
    /// Ticks left in the post-kill cooldown.
    pub cooldown_ticks: u32,
    /// The host-side tenants.
    pub tenants: Vec<GpuTenant>,
    /// The starvation signals observed this tick.
    pub signals: Vec<StarvationSignal>,
}

/// What the ladder decides to do about the card.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LadderDecision {
    /// Scale the named tenant to zero; the signal that justified it is data.
    ScaleLowestPriorityToZero {
        /// The tenant that leaves the card.
        tenant: String,
        /// Its ladder rung.
        ladder: GpuLadder,
        /// The signal that justified the kill.
        signal: StarvationSignal,
    },
    /// Nothing to do, with the reason.
    None {
        /// Why no action is taken.
        reason: LadderHold,
    },
}

/// Why the ladder leaves the card alone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LadderHold {
    /// The card belongs to a guest; host tenants do not share it.
    NotHostOwned,
    /// A previous kill is still landing.
    Cooldown {
        /// Ticks left.
        remaining_ticks: u32,
    },
    /// The card is not full, so no pressure is real.
    CardNotFull,
    /// No signal has been starved for long enough.
    NoGracedSignal,
    /// No running tenant has a lower priority than the starved signal.
    NoLowerPriorityTenant,
}

/// Decide the ladder action for one tick.
///
/// Order of gates: the host must own the card, no cooldown may be running, the
/// card must be full, a signal must be graced, and a running tenant must be
/// strictly below the starved signal's priority. The victim is the
/// lowest-priority such tenant; ties break on the tenant name so a decision is
/// reproducible.
#[must_use]
pub fn decide_ladder(input: &LadderInput, policy: &GpuPolicy) -> LadderDecision {
    if !input.owner.is_host() {
        return LadderDecision::None {
            reason: LadderHold::NotHostOwned,
        };
    }
    if input.cooldown_ticks > 0 {
        return LadderDecision::None {
            reason: LadderHold::Cooldown {
                remaining_ticks: input.cooldown_ticks,
            },
        };
    }
    if !input.vram_full {
        return LadderDecision::None {
            reason: LadderHold::CardNotFull,
        };
    }
    let Some(signal) = input
        .signals
        .iter()
        .filter(|signal| signal.is_graced(policy))
        .max_by_key(|signal| signal.priority(policy))
    else {
        return LadderDecision::None {
            reason: LadderHold::NoGracedSignal,
        };
    };
    let signal_priority = signal.priority(policy);
    let victim = input
        .tenants
        .iter()
        .filter(|tenant| {
            tenant.state == TenantState::Running && tenant.ladder.priority() < signal_priority
        })
        .min_by(|left, right| {
            left.ladder
                .priority()
                .cmp(&right.ladder.priority())
                .then_with(|| left.id.cmp(&right.id))
        });
    let Some(victim) = victim else {
        return LadderDecision::None {
            reason: LadderHold::NoLowerPriorityTenant,
        };
    };
    LadderDecision::ScaleLowestPriorityToZero {
        tenant: victim.id.clone(),
        ladder: victim.ladder,
        signal: signal.clone(),
    }
}
