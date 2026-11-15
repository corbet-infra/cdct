//! Switch planning: a typed, ordered list of steps for the runtime.
//!
//! A plan is data. `cdct` never stops a guest, binds a device or starts a
//! session; it says what has to happen, in which order, names the memory the
//! switch was admitted with, and records the host tenants it displaced so the
//! switch back restores exactly that set.

use cdrv::{GuestOs, InstanceClass, InstanceId, StopMode};
use serde::{Deserialize, Serialize};

use crate::facts::MemorySnapshot;
use crate::gpu::{
    DisplacedTenant, GpuState, GpuTenant, InstanceObservation, KernelOwner, TenantState,
};
use crate::memory::{MemoryAdmission, MemoryDecision, MemoryRefusal, decide_memory_with_credit};
use crate::policy::Policy;

/// One step the runtime executes, in plan order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStep {
    /// A host tenant leaves the card. A pod is scaled to zero; the desktop
    /// session keeps running and only releases the card, as the platform
    /// requires.
    StopHostTenant {
        /// The tenant.
        tenant: String,
        /// Its ladder rung.
        ladder: crate::gpu::GpuLadder,
    },
    /// An instance stops cleanly. A GPU handover never hibernates: the office
    /// profile is shut down, never suspended.
    StopInstance {
        /// The instance.
        instance: InstanceId,
        /// The stop mode (always `clean` in a plan `cdct` produces).
        mode: StopMode,
    },
    /// The card changes hands, exactly once per plan. The runtime resolves the
    /// device name through the hardware inventory, binds or unbinds the
    /// passthrough driver and resets every function of the card.
    HandOverCard {
        /// The card's device name in the hardware inventory, never an address.
        device: String,
        /// The kernel that releases the card.
        from: KernelOwner,
        /// The kernel that receives it.
        to: KernelOwner,
    },
    /// A guest instance starts.
    StartInstance {
        /// The instance.
        instance: InstanceId,
    },
    /// A host tenant returns to the card.
    StartHostTenant {
        /// The tenant.
        tenant: String,
        /// Its ladder rung.
        ladder: crate::gpu::GpuLadder,
        /// How many replicas to start: the count stored when it was stopped.
        #[serde(default = "crate::gpu::one_replica")]
        replicas: u32,
    },
    /// Sunshine moves with the card: the kernel that holds the card runs the
    /// Sunshine the owner connects to. In a guest plan it comes right before
    /// the hand-over (the host's Sunshine holds the video encoder and must
    /// leave the card first); in a host plan it is the last step.
    SunshineFollows {
        /// The kernel that now holds the card.
        kernel: KernelOwner,
    },
    /// The GPU device plugin lost its registration (the card's resources read
    /// zero while it runs): the runtime restarts it, and a fresh process
    /// registers again.
    RestartDevicePlugin,
}

/// Why a switch is declined.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SwitchRefusal {
    /// The RAM admission declined the target's fixed claim.
    Memory(MemoryRefusal),
    /// The office profile has no GPU and can never be a GPU target.
    OfficeProfileCannotHoldTheCard {
        /// The office instance.
        instance: InstanceId,
    },
    /// The target is not an observed instance: nothing is planned to start.
    UnknownTarget {
        /// The instance that was asked for.
        instance: InstanceId,
    },
    /// The target is observed but is not a GPU (desktop) instance.
    NotADesktopInstance {
        /// The instance that was asked for.
        instance: InstanceId,
        /// The class it actually has.
        class: InstanceClass,
    },
}

/// A switch plan, with the memory it was admitted against and what it
/// displaced.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SwitchPlan {
    /// The kernel that will own the card.
    pub target: KernelOwner,
    /// The memory admission for a guest target; `None` for a host target,
    /// whose tenants take no new fixed claim.
    pub memory: Option<MemoryAdmission>,
    /// The host tenants this plan stops. The runtime records them with the
    /// holder so a later switch back to the host restores exactly this set.
    pub displaced: Vec<DisplacedTenant>,
    /// The steps, in execution order.
    pub steps: Vec<PlanStep>,
}

/// The outcome of asking for a switch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SwitchOutcome {
    /// The target already owns the card; no step is needed.
    AlreadyHeld {
        /// The current owner.
        kernel: KernelOwner,
    },
    /// The switch may proceed.
    Plan(SwitchPlan),
    /// The RAM is not available now; nothing is displaced.
    Wait {
        /// Why the target waits.
        reason: MemoryRefusal,
    },
    /// The switch can never proceed as asked.
    Decline {
        /// Why it is declined.
        reason: SwitchRefusal,
    },
}

/// Plan a switch of the card to `target`.
///
/// A guest target is a GPU VM with a fixed claim (24 GiB by default): it is
/// admitted by the memory guard first, then the plan stops what is on the card,
/// stops the Windows office profile when the target is Windows (the two
/// profiles never run together), moves Sunshine off the card, hands the card over
/// and starts the target. A host target is the reverse: stop the guest, hand the card
/// back and restore exactly the tenants the guest switch displaced, in ladder
/// order (highest first).
#[must_use]
pub fn plan_switch(
    target: &KernelOwner,
    gpu: &GpuState,
    memory: &MemorySnapshot,
    displaced: &[DisplacedTenant],
    policy: &Policy,
) -> SwitchOutcome {
    if &gpu.owner == target {
        return SwitchOutcome::AlreadyHeld {
            kernel: gpu.owner.clone(),
        };
    }
    match target {
        KernelOwner::Host => SwitchOutcome::Plan(host_plan(gpu, displaced)),
        KernelOwner::Guest(id) => guest_plan(id, gpu, memory, policy),
    }
}

fn guest_plan(
    id: &InstanceId,
    gpu: &GpuState,
    memory: &MemorySnapshot,
    policy: &Policy,
) -> SwitchOutcome {
    let Some(observed) = gpu.instance(id) else {
        return SwitchOutcome::Decline {
            reason: SwitchRefusal::UnknownTarget {
                instance: id.clone(),
            },
        };
    };
    if observed.class == InstanceClass::Office {
        return SwitchOutcome::Decline {
            reason: SwitchRefusal::OfficeProfileCannotHoldTheCard {
                instance: id.clone(),
            },
        };
    }
    if observed.class != InstanceClass::Desktop {
        return SwitchOutcome::Decline {
            reason: SwitchRefusal::NotADesktopInstance {
                instance: id.clone(),
                class: observed.class,
            },
        };
    }
    // What the plan stops before the target starts: the guest that currently
    // holds the card, and (for a Windows target) the office profile. Their
    // resident memory is credited to the admission, because the plan frees it
    // before the target needs it.
    let current = gpu.owner.guest().filter(|current| *current != id);
    let offices: Vec<&InstanceObservation> = if observed.guest_os == GuestOs::Windows {
        paired_office_profiles(gpu, observed.install.as_deref())
    } else {
        Vec::new()
    };
    let credited_mib = current
        .and_then(|current| gpu.instance(current))
        .into_iter()
        .chain(offices.iter().copied())
        .filter(|instance| instance.state.is_live())
        .filter_map(|instance| instance.resident_mib)
        .fold(0_u64, u64::saturating_add);

    let claim_mib = observed
        .memory_mib
        .unwrap_or(policy.memory.gpu_vm_claim_mib);
    let admission = match decide_memory_with_credit(memory, claim_mib, credited_mib, &policy.memory)
    {
        MemoryDecision::Admit(admission) => admission,
        MemoryDecision::Wait(reason) => return SwitchOutcome::Wait { reason },
        MemoryDecision::Decline(reason) => {
            return SwitchOutcome::Decline {
                reason: SwitchRefusal::Memory(reason),
            };
        }
    };
    let mut steps = Vec::new();
    if let Some(current) = current {
        steps.push(PlanStep::StopInstance {
            instance: current.clone(),
            mode: StopMode::Clean,
        });
    }
    let mut displaced = Vec::new();
    if gpu.owner.is_host() {
        for tenant in host_tenants_lowest_first(gpu) {
            displaced.push(DisplacedTenant {
                tenant: tenant.id.clone(),
                ladder: tenant.ladder,
                // Placeholder: cwld overwrites it from the driver's count
                // before the stop. Zero until then, so a grant nobody executed
                // (or one whose count could not be read) restores nothing.
                replicas: 0,
            });
            steps.push(PlanStep::StopHostTenant {
                tenant: tenant.id.clone(),
                ladder: tenant.ladder,
            });
        }
    }
    for office in &offices {
        steps.push(PlanStep::StopInstance {
            instance: office.id.clone(),
            mode: StopMode::Clean,
        });
    }
    // The host's Sunshine holds the card's video encoder, so it leaves the
    // card before the hand-over.
    steps.push(PlanStep::SunshineFollows {
        kernel: KernelOwner::Guest(id.clone()),
    });
    steps.push(PlanStep::HandOverCard {
        device: gpu.device.clone(),
        from: gpu.owner.clone(),
        to: KernelOwner::Guest(id.clone()),
    });
    steps.push(PlanStep::StartInstance {
        instance: id.clone(),
    });
    SwitchOutcome::Plan(SwitchPlan {
        target: KernelOwner::Guest(id.clone()),
        memory: Some(admission),
        displaced,
        steps,
    })
}

fn host_plan(gpu: &GpuState, displaced: &[DisplacedTenant]) -> SwitchPlan {
    let mut steps = Vec::new();
    if let KernelOwner::Guest(current) = &gpu.owner {
        steps.push(PlanStep::StopInstance {
            instance: current.clone(),
            mode: StopMode::Clean,
        });
    }
    steps.push(PlanStep::HandOverCard {
        device: gpu.device.clone(),
        from: gpu.owner.clone(),
        to: KernelOwner::Host,
    });
    // A tenant recorded at zero replicas was not running (or never counted):
    // it was not displaced, so it is not started.
    let mut returning: Vec<&DisplacedTenant> = displaced
        .iter()
        .filter(|tenant| tenant.replicas > 0)
        .collect();
    returning.sort_by(|left, right| {
        right
            .ladder
            .priority()
            .cmp(&left.ladder.priority())
            .then_with(|| left.tenant.cmp(&right.tenant))
    });
    for tenant in returning {
        steps.push(PlanStep::StartHostTenant {
            tenant: tenant.tenant.clone(),
            ladder: tenant.ladder,
            replicas: tenant.replicas,
        });
    }
    steps.push(PlanStep::SunshineFollows {
        kernel: KernelOwner::Host,
    });
    SwitchPlan {
        target: KernelOwner::Host,
        memory: None,
        displaced: Vec::new(),
        steps,
    }
}

/// The plan for a ladder shed: the card stays on the host, one tenant leaves
/// it. The tenant is listed as displaced so the runtime records its live
/// replica count before the stop, which is what [`restore_plan`] starts.
#[must_use]
pub fn shed_plan(tenant: &str, ladder: crate::gpu::GpuLadder) -> SwitchPlan {
    SwitchPlan {
        target: KernelOwner::Host,
        memory: None,
        displaced: vec![DisplacedTenant {
            tenant: tenant.to_owned(),
            ladder,
            replicas: 0,
        }],
        steps: vec![PlanStep::StopHostTenant {
            tenant: tenant.to_owned(),
            ladder,
        }],
    }
}

/// The plan that returns a shed tenant to the card with the replicas it had.
#[must_use]
pub fn restore_plan(tenant: &str, ladder: crate::gpu::GpuLadder, replicas: u32) -> SwitchPlan {
    SwitchPlan {
        target: KernelOwner::Host,
        memory: None,
        displaced: Vec::new(),
        steps: vec![PlanStep::StartHostTenant {
            tenant: tenant.to_owned(),
            ladder,
            replicas,
        }],
    }
}

/// The plan that restarts the GPU device plugin; the card stays where it is.
#[must_use]
pub fn guard_plan() -> SwitchPlan {
    SwitchPlan {
        target: KernelOwner::Host,
        memory: None,
        displaced: Vec::new(),
        steps: vec![PlanStep::RestartDevicePlugin],
    }
}

fn host_tenants_lowest_first(gpu: &GpuState) -> Vec<&GpuTenant> {
    let mut tenants: Vec<&GpuTenant> = gpu
        .tenants
        .iter()
        .filter(|tenant| tenant.state != TenantState::Stopped)
        .collect();
    tenants.sort_by(|left, right| {
        left.ladder
            .priority()
            .cmp(&right.ladder.priority())
            .then_with(|| left.id.cmp(&right.id))
    });
    tenants
}

/// The live office profiles that share the target's install.
///
/// Pairing is by install (disk), never by guest OS: two Windows installs are
/// unrelated and must not stop each other's profile. A target with no known
/// install pairs with nothing.
fn paired_office_profiles<'a>(
    gpu: &'a GpuState,
    install: Option<&str>,
) -> Vec<&'a InstanceObservation> {
    let Some(install) = install else {
        return Vec::new();
    };
    gpu.instances
        .iter()
        .filter(|instance| {
            instance.class == InstanceClass::Office
                && instance.state.is_live()
                && instance.install.as_deref() == Some(install)
        })
        .collect()
}
