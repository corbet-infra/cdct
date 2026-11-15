//! Plain-language descriptions of decisions, status and events.
//!
//! The CLI prints these lines and the tray uses the same strings for a greyed
//! menu entry, so a wait or decline reads the same everywhere. They are pure
//! functions of protocol data: no formatting depends on a terminal or a GUI.

use cdrv::{Event, Subject};

use crate::holders::HolderState;
use crate::memory::MemoryRefusal;
use crate::plan::{PlanStep, SwitchOutcome, SwitchRefusal};
use crate::protocol::{Availability, Channel, Status};

/// Describe a switch outcome as the lines an operator reads.
#[must_use]
pub fn outcome(outcome: &SwitchOutcome) -> Vec<String> {
    match outcome {
        SwitchOutcome::AlreadyHeld { kernel } => {
            vec![format!("the card already belongs to {kernel}")]
        }
        SwitchOutcome::Plan(plan) => {
            let mut lines = vec![format!("plan: switch the card to {}", plan.target)];
            if let Some(memory) = &plan.memory {
                lines.push(format!(
                    "  memory: {} MiB admitted ({} MiB available, {} MiB freed by the plan, \
                     {} MiB from swap)",
                    memory.required_mib,
                    memory.available_mib,
                    memory.credited_mib,
                    memory.via_swap_mib
                ));
            }
            if !plan.displaced.is_empty() {
                let names: Vec<String> = plan
                    .displaced
                    .iter()
                    .map(|displaced| format!("{} x{}", displaced.tenant, displaced.replicas))
                    .collect();
                lines.push(format!("  displaced: {}", names.join(", ")));
            }
            for (index, step) in plan.steps.iter().enumerate() {
                lines.push(format!("  {}. {}", index + 1, step_line(step)));
            }
            lines
        }
        SwitchOutcome::Wait { reason } => vec![
            "wait: the target is not admitted now".to_owned(),
            format!("  {}", memory_refusal(reason)),
        ],
        SwitchOutcome::Decline { reason } => vec![
            "decline: the target cannot be admitted".to_owned(),
            format!("  {}", switch_refusal(reason)),
        ],
    }
}

/// Describe the status snapshot.
#[must_use]
pub fn status(status: &Status) -> Vec<String> {
    let mut lines = vec![
        format!("gpu owner: {} ({})", status.owner, status.owner_label),
        format!("sequence: {}", status.seq),
        format!("holders: {}", status.holders.len()),
    ];
    for holder in &status.holders {
        let instance = holder
            .instance
            .as_ref()
            .map_or_else(|| "-".to_owned(), ToString::to_string);
        let tenant = holder.tenant.as_deref().unwrap_or("-");
        lines.push(format!(
            "  {} instance={} tenant={} class={:?} priority={} memory={}MiB devices={} state={}",
            holder.id,
            instance,
            tenant,
            holder.class,
            holder.priority,
            holder.memory_mib,
            if holder.devices.is_empty() {
                "-".to_owned()
            } else {
                holder.devices.join(",")
            },
            holder_state(holder.state)
        ));
    }
    if !status.targets.is_empty() {
        lines.push("targets:".to_owned());
        for target in &status.targets {
            lines.push(format!(
                "  {} -> {}: {}",
                target.label,
                target.kernel,
                availability(&target.availability)
            ));
        }
    }
    lines
}

/// Describe a target's availability.
#[must_use]
pub fn availability(availability: &Availability) -> String {
    match availability {
        Availability::Ready => "ready".to_owned(),
        Availability::AlreadyHeld => "already held".to_owned(),
        Availability::Wait { reason } => format!("waits: {}", memory_refusal(reason)),
        Availability::Decline { reason } => format!("declined: {}", switch_refusal(reason)),
    }
}

/// Describe one pushed event.
#[must_use]
pub fn event(event: &Event) -> String {
    format!(
        "[{}] {} {} {}{}",
        event.seq,
        event.at.as_millis(),
        event.kind,
        subject(&event.subject),
        if event.data.is_null() {
            String::new()
        } else {
            format!(" {}", event.data)
        }
    )
}

/// Describe the channel a subscription was opened on.
#[must_use]
pub fn channel(channel: &Channel) -> String {
    match channel {
        Channel::Local => "the local socket".to_owned(),
        Channel::Guest { instance } => format!("guest {instance}"),
    }
}

/// Describe why a target waits for RAM.
#[must_use]
pub fn memory_refusal(reason: &MemoryRefusal) -> String {
    match reason {
        MemoryRefusal::NotEnoughFree {
            available_mib,
            required_mib,
            gap_mib,
            ..
        } => format!(
            "not enough free memory: {required_mib} MiB needed, {available_mib} MiB available \
             ({gap_mib} MiB short); nothing is displaced"
        ),
        MemoryRefusal::SwapNotAllowedUnderPressure {
            gap_mib,
            some_avg10,
            full_avg10,
            some_limit,
            full_limit,
        } => format!(
            "swap would have to cover {gap_mib} MiB but memory pressure is too high \
             (some {some_avg10:?}% / {some_limit}%, full {full_avg10:?}% / {full_limit}%)"
        ),
        MemoryRefusal::ClaimExceedsHostCapacity {
            required_mib,
            ram_mib,
        } => format!(
            "the claim ({required_mib} MiB) exceeds this host's physical RAM ({ram_mib} MiB); \
             the claim is pinned, so swap cannot make it fit"
        ),
    }
}

/// Describe why a target is declined.
#[must_use]
pub fn switch_refusal(reason: &SwitchRefusal) -> String {
    match reason {
        SwitchRefusal::Memory(reason) => memory_refusal(reason),
        SwitchRefusal::OfficeProfileCannotHoldTheCard { instance } => format!(
            "{instance} is the office profile, which has no GPU and can never hold the card"
        ),
        SwitchRefusal::UnknownTarget { instance } => {
            format!("{instance} is not an observed instance, so nothing could be started")
        }
        SwitchRefusal::NotADesktopInstance { instance, class } => {
            format!("{instance} is a {class:?} instance, not a desktop one")
        }
    }
}

fn step_line(step: &PlanStep) -> String {
    match step {
        PlanStep::StopHostTenant { tenant, ladder } => {
            format!("stop host tenant {tenant} ({ladder})")
        }
        PlanStep::StopInstance { instance, mode } => {
            format!("stop instance {instance} ({mode:?})")
        }
        PlanStep::HandOverCard { device, from, to } => {
            format!("hand the card {device} over: {from} -> {to}")
        }
        PlanStep::StartInstance { instance } => format!("start instance {instance}"),
        PlanStep::StartHostTenant {
            tenant,
            ladder,
            replicas,
        } => format!("start host tenant {tenant} ({ladder}), {replicas} replica(s)"),
        PlanStep::SunshineFollows { kernel } => format!("Sunshine follows {kernel}"),
        PlanStep::RestartDevicePlugin => "restart the GPU device plugin".to_owned(),
    }
}

fn subject(subject: &Subject) -> String {
    match subject {
        Subject::Instance(instance) => format!("instance:{instance}"),
        Subject::Holder(holder) => format!("holder:{holder}"),
        Subject::Device(device) => format!("device:{device}"),
        Subject::Dataset(dataset) => format!("dataset:{dataset}"),
        Subject::Tenant(tenant) => format!("tenant:{tenant}"),
        Subject::Host => "host".to_owned(),
    }
}

fn holder_state(state: HolderState) -> &'static str {
    match state {
        HolderState::Granted => "granted",
        HolderState::Running => "running",
        HolderState::Releasing => "releasing",
    }
}
