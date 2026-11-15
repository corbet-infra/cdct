//! Switch planning: a typed, ordered list of steps for the runtime.
//!
//! A plan is data. `cdct` never stops a guest, binds a device or starts a
//! session; it says what has to happen, in which order, and names the memory
//! the switch was admitted with.

use cdrv::{GuestOs, InstanceClass, InstanceId, StopMode};
use serde::{Deserialize, Serialize};

use crate::facts::MemorySnapshot;
use crate::gpu::{GpuState, GpuTenant, KernelOwner, TenantState};
use crate::memory::{MemoryAdmission, MemoryDecision, MemoryRefusal, decide_memory};
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
    /// The card changes hands, exactly once per plan. The runtime binds or
    /// unbinds the passthrough driver and resets the device.
    HandOverCard {
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
    },
    /// Sunshine moves with the card: the kernel that holds the card runs the
    /// Sunshine the owner connects to.
    SunshineFollows {
        /// The kernel that now holds the card.
        kernel: KernelOwner,
    },
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
}

/// A switch plan, with the memory it was admitted against.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SwitchPlan {
    /// The kernel that will own the card.
    pub target: KernelOwner,
    /// The memory admission for a guest target; `None` for a host target,
    /// whose tenants take no new fixed claim.
    pub memory: Option<MemoryAdmission>,
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
/// profiles never run together), hands the card over, starts the target and
/// moves Sunshine. A host target is the reverse: stop the guest, hand the card
/// back and restore the host tenants in ladder order.
#[must_use]
pub fn plan_switch(
    target: &KernelOwner,
    gpu: &GpuState,
    memory: &MemorySnapshot,
    policy: &Policy,
) -> SwitchOutcome {
    if &gpu.owner == target {
        return SwitchOutcome::AlreadyHeld {
            kernel: gpu.owner.clone(),
        };
    }
    match target {
        KernelOwner::Host => SwitchOutcome::Plan(host_plan(gpu)),
        KernelOwner::Guest(id) => guest_plan(id, gpu, memory, policy),
    }
}

fn guest_plan(
    id: &InstanceId,
    gpu: &GpuState,
    memory: &MemorySnapshot,
    policy: &Policy,
) -> SwitchOutcome {
    let observed = gpu.instance(id);
    if observed.is_some_and(|instance| instance.class == InstanceClass::Office) {
        return SwitchOutcome::Decline {
            reason: SwitchRefusal::OfficeProfileCannotHoldTheCard {
                instance: id.clone(),
            },
        };
    }
    let claim_mib = observed
        .and_then(|instance| instance.memory_mib)
        .unwrap_or(policy.memory.gpu_vm_claim_mib);
    let admission = match decide_memory(memory, claim_mib, &policy.memory) {
        MemoryDecision::Admit(admission) => admission,
        MemoryDecision::Wait(reason) => return SwitchOutcome::Wait { reason },
        MemoryDecision::Decline(reason) => {
            return SwitchOutcome::Decline {
                reason: SwitchRefusal::Memory(reason),
            };
        }
    };
    let mut steps = Vec::new();
    if let KernelOwner::Guest(current) = &gpu.owner
        && current != id
    {
        steps.push(PlanStep::StopInstance {
            instance: current.clone(),
            mode: StopMode::Clean,
        });
    }
    if gpu.owner.is_host() {
        for tenant in host_tenants_lowest_first(gpu) {
            steps.push(PlanStep::StopHostTenant {
                tenant: tenant.id.clone(),
                ladder: tenant.ladder,
            });
        }
    }
    if observed.is_some_and(|instance| instance.guest_os == GuestOs::Windows) {
        for office in running_office_profiles(gpu) {
            steps.push(PlanStep::StopInstance {
                instance: office.clone(),
                mode: StopMode::Clean,
            });
        }
    }
    steps.push(PlanStep::HandOverCard {
        from: gpu.owner.clone(),
        to: KernelOwner::Guest(id.clone()),
    });
    steps.push(PlanStep::StartInstance {
        instance: id.clone(),
    });
    steps.push(PlanStep::SunshineFollows {
        kernel: KernelOwner::Guest(id.clone()),
    });
    SwitchOutcome::Plan(SwitchPlan {
        target: KernelOwner::Guest(id.clone()),
        memory: Some(admission),
        steps,
    })
}

fn host_plan(gpu: &GpuState) -> SwitchPlan {
    let mut steps = Vec::new();
    if let KernelOwner::Guest(current) = &gpu.owner {
        steps.push(PlanStep::StopInstance {
            instance: current.clone(),
            mode: StopMode::Clean,
        });
    }
    steps.push(PlanStep::HandOverCard {
        from: gpu.owner.clone(),
        to: KernelOwner::Host,
    });
    let mut returning: Vec<&GpuTenant> = gpu
        .tenants
        .iter()
        .filter(|tenant| tenant.state != TenantState::Running)
        .collect();
    returning.sort_by(|left, right| {
        right
            .ladder
            .priority()
            .cmp(&left.ladder.priority())
            .then_with(|| left.id.cmp(&right.id))
    });
    for tenant in returning {
        steps.push(PlanStep::StartHostTenant {
            tenant: tenant.id.clone(),
            ladder: tenant.ladder,
        });
    }
    steps.push(PlanStep::SunshineFollows {
        kernel: KernelOwner::Host,
    });
    SwitchPlan {
        target: KernelOwner::Host,
        memory: None,
        steps,
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

fn running_office_profiles(gpu: &GpuState) -> Vec<&InstanceId> {
    gpu.instances
        .iter()
        .filter(|instance| instance.class == InstanceClass::Office && instance.state.is_live())
        .map(|instance| &instance.id)
        .collect()
}
