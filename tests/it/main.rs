//! The conductor's specification: one test binary, read as sentences.
//!
//! Every scenario uses fixture text and synthetic names only. No test reads the
//! live host, opens a device or runs a host tool.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use cdct::{
    Conductor, DisplacedTenant, FactsFile, GpuLadder, GpuState, GpuTenant, Holder, HolderState,
    HolderStore, HolderTable, InstanceObservation, KernelOwner, LadderDecision, LadderHold,
    LadderInput, MemoryDecision, MemoryHeadroom, MemoryRefusal, MemorySnapshot,
    ObservedDeviceDriver, ObservedHost, ObservedInstance, PlanStep, Policy, ProcReader, RawFacts,
    ReconcileReason, ReconcileReport, Repair, StarvationSignal, SwitchOutcome, SwitchPlan,
    SwitchRefusal, TenantState, VramFacts, holders::read_table, parse_arcstats, parse_meminfo,
    parse_pressure,
};
use cdrv::{
    EventLog, GuestOs, HolderId, InstanceClass, InstanceId, InstanceState, LeaseId, Source,
    StopMode, Timestamp,
};
use fs2::FileExt;

mod config;
#[cfg(unix)]
mod daemon;
mod protocol;
mod tray;

const GIB_MIB: u64 = 1024;
const GIB_BYTES: u64 = 1024 * 1024 * 1024;

fn meminfo_text(
    total_mib: u64,
    available_mib: u64,
    swap_total_mib: u64,
    swap_free_mib: u64,
) -> String {
    format!(
        "MemTotal: {} kB\nMemFree: 0 kB\nMemAvailable: {} kB\nSwapTotal: {} kB\nSwapFree: {} kB\n",
        total_mib * 1024,
        available_mib * 1024,
        swap_total_mib * 1024,
        swap_free_mib * 1024
    )
}

fn pressure_text(some_avg10: f32, full_avg10: f32) -> String {
    format!(
        "some avg10={some_avg10:.2} avg60=1.00 avg300=2.00 total=100\n\
         full avg10={full_avg10:.2} avg60=0.50 avg300=1.00 total=50\n"
    )
}

fn arcstats_text(size_bytes: u64, c_min_bytes: u64, evictable: [u64; 4]) -> String {
    format!(
        "name type data\n\
         size 4 {size_bytes}\n\
         c_min 4 {c_min_bytes}\n\
         mru_evictable_data 4 {}\n\
         mru_evictable_metadata 4 {}\n\
         mfu_evictable_data 4 {}\n\
         mfu_evictable_metadata 4 {}\n",
        evictable[0], evictable[1], evictable[2], evictable[3]
    )
}

fn snapshot_with(
    available_mib: u64,
    swap_total_mib: u64,
    swap_free_mib: u64,
    some: f32,
    full: f32,
) -> MemorySnapshot {
    MemorySnapshot::parse(&RawFacts {
        meminfo: meminfo_text(128 * GIB_MIB, available_mib, swap_total_mib, swap_free_mib),
        arcstats: None,
        pressure_memory: pressure_text(some, full),
    })
    .expect("the memory fixture parses")
}

fn snapshot(available_mib: u64) -> MemorySnapshot {
    snapshot_with(available_mib, 0, 0, 0.0, 0.0)
}

fn small_host_snapshot(
    available_mib: u64,
    swap_total_mib: u64,
    swap_free_mib: u64,
) -> MemorySnapshot {
    MemorySnapshot::parse(&RawFacts {
        meminfo: meminfo_text(16 * GIB_MIB, available_mib, swap_total_mib, swap_free_mib),
        arcstats: None,
        pressure_memory: pressure_text(0.0, 0.0),
    })
    .expect("the memory fixture parses")
}

fn tenant(id: &str, ladder: GpuLadder, state: TenantState) -> GpuTenant {
    GpuTenant {
        id: id.to_owned(),
        ladder,
        state,
    }
}

fn observation(
    id: &str,
    class: InstanceClass,
    guest_os: GuestOs,
    state: InstanceState,
) -> InstanceObservation {
    InstanceObservation {
        id: InstanceId::from(id),
        class,
        guest_os,
        state,
        memory_mib: None,
        resident_mib: None,
        install: None,
    }
}

fn windows_install(id: &str, class: InstanceClass, install: &str) -> InstanceObservation {
    InstanceObservation {
        install: Some(install.to_owned()),
        ..observation(id, class, GuestOs::Windows, InstanceState::Stopped)
    }
}

fn host_gpu() -> GpuState {
    let mut office = windows_install("office", InstanceClass::Office, "win-ssd");
    office.state = InstanceState::Running;
    let mut other_office = windows_install("office-other", InstanceClass::Office, "win-other");
    other_office.state = InstanceState::Running;
    GpuState {
        device: "example-gpu".to_owned(),
        owner: KernelOwner::Host,
        bound_to_vfio: false,
        vram: Some(VramFacts {
            used_bytes: 15 * GIB_BYTES,
            total_bytes: 16 * GIB_BYTES,
        }),
        tenants: vec![
            tenant("lxc-desktop", GpuLadder::Desktop, TenantState::Running),
            tenant("llm", GpuLadder::Interactive, TenantState::Running),
            tenant("comfyui", GpuLadder::BestEffort, TenantState::Running),
        ],
        instances: vec![
            observation(
                "mac",
                InstanceClass::Desktop,
                GuestOs::MacOs,
                InstanceState::Stopped,
            ),
            windows_install("windows-desktop", InstanceClass::Desktop, "win-ssd"),
            windows_install("windows-other", InstanceClass::Desktop, "win-other"),
            office,
            other_office,
            observation(
                "dev",
                InstanceClass::Dev,
                GuestOs::Linux,
                InstanceState::Running,
            ),
        ],
    }
}

fn guest(id: &str) -> KernelOwner {
    KernelOwner::Guest(InstanceId::from(id))
}

fn conductor() -> Conductor {
    Conductor::with_defaults()
}

/// Plan a switch with no recorded displacement (the host holds the card).
fn switch(target: &KernelOwner, gpu: &GpuState, memory: &MemorySnapshot) -> SwitchOutcome {
    conductor().switch(target, gpu, memory, &HolderTable::new())
}

fn steps(outcome: &SwitchOutcome) -> &[PlanStep] {
    match outcome {
        SwitchOutcome::Plan(plan) => &plan.steps,
        other => panic!("expected a plan, got {other:?}"),
    }
}

fn position(steps: &[PlanStep], wanted: impl Fn(&PlanStep) -> bool) -> Option<usize> {
    steps.iter().position(wanted)
}

// ---------------------------------------------------------------------------
// The ported memory guard
// ---------------------------------------------------------------------------

#[test]
fn the_guard_adds_reclaimable_arc_to_mem_available() {
    let parsed = MemorySnapshot::parse(&RawFacts {
        meminfo: meminfo_text(128 * GIB_MIB, 16 * GIB_MIB, 0, 0),
        arcstats: Some(arcstats_text(
            32 * GIB_BYTES,
            4 * GIB_BYTES,
            [2 * GIB_BYTES, 2 * GIB_BYTES, 2 * GIB_BYTES, 2 * GIB_BYTES],
        )),
        pressure_memory: pressure_text(0.0, 0.0),
    })
    .expect("the guard fixture parses");
    let headroom = MemoryHeadroom::from_snapshot(&parsed);
    assert_eq!(headroom.mem_available_mib, 16 * GIB_MIB);
    assert_eq!(headroom.evictable_arc_mib, 8 * GIB_MIB);
    assert_eq!(headroom.available_mib(), 24 * GIB_MIB);
}

#[test]
fn the_guard_caps_evictable_arc_at_size_minus_c_min() {
    let parsed = MemorySnapshot::parse(&RawFacts {
        meminfo: meminfo_text(128 * GIB_MIB, 16 * GIB_MIB, 0, 0),
        arcstats: Some(arcstats_text(
            16 * GIB_BYTES,
            12 * GIB_BYTES,
            [10 * GIB_BYTES; 4],
        )),
        pressure_memory: pressure_text(0.0, 0.0),
    })
    .expect("the guard fixture parses");
    let headroom = MemoryHeadroom::from_snapshot(&parsed);
    assert_eq!(headroom.evictable_arc_mib, 4 * GIB_MIB);
    assert_eq!(headroom.available_mib(), 20 * GIB_MIB);
}

#[test]
fn the_guard_counts_no_arc_when_the_host_has_no_zfs() {
    let headroom = MemoryHeadroom::from_snapshot(&snapshot(16 * GIB_MIB));
    assert_eq!(headroom.evictable_arc_mib, 0);
    assert_eq!(headroom.available_mib(), 16 * GIB_MIB);
}

#[test]
fn the_memory_parsers_read_the_kernel_formats() {
    let mem = parse_meminfo(&meminfo_text(
        128 * GIB_MIB,
        40 * GIB_MIB,
        32 * GIB_MIB,
        31 * GIB_MIB,
    ))
    .expect("meminfo parses");
    assert_eq!(mem.total_mib(), 128 * GIB_MIB);
    assert_eq!(mem.available_mib(), 40 * GIB_MIB);
    assert_eq!(mem.swap_total_mib(), 32 * GIB_MIB);
    assert_eq!(mem.swap_free_mib(), 31 * GIB_MIB);

    let arc = parse_arcstats(&arcstats_text(
        32 * GIB_BYTES,
        4 * GIB_BYTES,
        [2 * GIB_BYTES, 2 * GIB_BYTES, 2 * GIB_BYTES, 2 * GIB_BYTES],
    ))
    .expect("arcstats parses");
    assert_eq!(arc.evictable_sum_bytes(), 8 * GIB_BYTES);
    assert_eq!(arc.evictable_bytes(), 8 * GIB_BYTES);

    let pressure = parse_pressure(&pressure_text(1.5, 0.25)).expect("pressure parses");
    assert_eq!(pressure.some_avg10(), Some(1.5));
    assert_eq!(pressure.full_avg10(), Some(0.25));
}

#[test]
fn missing_or_malformed_facts_fail_closed() {
    assert!(parse_meminfo("MemTotal: 1024 kB\n").is_err());
    assert!(parse_meminfo("MemTotal: lots kB\nMemAvailable: 1 kB\n").is_err());
    assert!(parse_pressure("average: nothing\n").is_err());
    assert!(parse_arcstats("size 4 not-a-number\n").is_err());
}

// ---------------------------------------------------------------------------
// RAM admission
// ---------------------------------------------------------------------------

#[test]
fn a_claim_covered_by_headroom_is_admitted() {
    let decision = conductor().memory(&snapshot(40 * GIB_MIB), 24 * GIB_MIB);
    match decision {
        MemoryDecision::Admit(admission) => {
            assert_eq!(admission.required_mib, 24 * GIB_MIB);
            assert_eq!(admission.available_mib, 40 * GIB_MIB);
            assert_eq!(admission.credited_mib, 0);
            assert_eq!(admission.via_swap_mib, 0);
        }
        other => panic!("expected an admission, got {other:?}"),
    }
}

#[test]
fn a_shortfall_without_swap_waits_and_displaces_nothing() {
    let decision = conductor().memory(&snapshot(10 * GIB_MIB), 24 * GIB_MIB);
    assert_eq!(
        decision,
        MemoryDecision::Wait(MemoryRefusal::NotEnoughFree {
            available_mib: 10 * GIB_MIB,
            required_mib: 24 * GIB_MIB,
            gap_mib: 14 * GIB_MIB,
            swap_free_mib: 0,
            max_swap_gap_mib: 8 * GIB_MIB,
        })
    );
}

#[test]
fn a_pinned_claim_larger_than_physical_ram_is_declined() {
    let memory = small_host_snapshot(4 * GIB_MIB, 2 * GIB_MIB, 2 * GIB_MIB);
    let decision = conductor().memory(&memory, 24 * GIB_MIB);
    assert_eq!(
        decision,
        MemoryDecision::Decline(MemoryRefusal::ClaimExceedsHostCapacity {
            required_mib: 24 * GIB_MIB,
            ram_mib: 16 * GIB_MIB,
        })
    );
}

#[test]
fn swap_cannot_make_a_pinned_claim_fit_beyond_physical_ram() {
    // 16 GiB RAM and 32 GiB swap: RAM plus swap covers 24 GiB, but the claim is
    // pinned and can never fit in 16 GiB of RAM.
    let memory = small_host_snapshot(8 * GIB_MIB, 32 * GIB_MIB, 32 * GIB_MIB);
    assert_eq!(
        conductor().memory(&memory, 24 * GIB_MIB),
        MemoryDecision::Decline(MemoryRefusal::ClaimExceedsHostCapacity {
            required_mib: 24 * GIB_MIB,
            ram_mib: 16 * GIB_MIB,
        })
    );
}

#[test]
fn swap_covers_a_gap_while_memory_pressure_is_calm() {
    let memory = snapshot_with(16 * GIB_MIB, 32 * GIB_MIB, 16 * GIB_MIB, 2.0, 0.5);
    match conductor().memory(&memory, 24 * GIB_MIB) {
        MemoryDecision::Admit(admission) => assert_eq!(admission.via_swap_mib, 8 * GIB_MIB),
        other => panic!("expected swap to cover the gap, got {other:?}"),
    }
}

#[test]
fn swap_is_not_used_under_memory_pressure() {
    let memory = snapshot_with(16 * GIB_MIB, 32 * GIB_MIB, 16 * GIB_MIB, 20.0, 10.0);
    match conductor().memory(&memory, 24 * GIB_MIB) {
        MemoryDecision::Wait(MemoryRefusal::SwapNotAllowedUnderPressure {
            gap_mib,
            some_avg10,
            full_avg10,
            ..
        }) => {
            assert_eq!(gap_mib, 8 * GIB_MIB);
            assert_eq!(some_avg10, Some(20.0));
            assert_eq!(full_avg10, Some(10.0));
        }
        other => panic!("expected a wait under pressure, got {other:?}"),
    }
}

#[test]
fn a_gap_larger_than_the_swap_allowance_waits() {
    let memory = snapshot_with(8 * GIB_MIB, 64 * GIB_MIB, 64 * GIB_MIB, 0.0, 0.0);
    match conductor().memory(&memory, 24 * GIB_MIB) {
        MemoryDecision::Wait(MemoryRefusal::NotEnoughFree { gap_mib, .. }) => {
            assert_eq!(gap_mib, 16 * GIB_MIB);
        }
        other => panic!("expected a wait beyond the swap allowance, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Switch planning
// ---------------------------------------------------------------------------

/// Assert the one-kernel invariant of a plan: the card changes hands exactly
/// once, every stop precedes the handover, and every start follows it. The
/// handover names the card as the hardware inventory does, never an address.
fn assert_one_kernel_plan(outcome: &SwitchOutcome, from: &KernelOwner, to: &KernelOwner) {
    let steps = steps(outcome);
    let handovers: Vec<usize> = steps
        .iter()
        .enumerate()
        .filter_map(|(index, step)| matches!(step, PlanStep::HandOverCard { .. }).then_some(index))
        .collect();
    assert_eq!(handovers.len(), 1, "exactly one handover: {steps:?}");
    let handover = handovers[0];
    assert_eq!(
        steps[handover],
        PlanStep::HandOverCard {
            device: host_gpu().device,
            from: from.clone(),
            to: to.clone(),
        }
    );
    for (index, step) in steps.iter().enumerate() {
        match step {
            PlanStep::StopHostTenant { .. } | PlanStep::StopInstance { .. } => {
                assert!(
                    index < handover,
                    "a stop only happens before the handover: {steps:?}"
                );
            }
            PlanStep::StartInstance { .. } | PlanStep::StartHostTenant { .. } => {
                assert!(
                    index > handover,
                    "a start only happens after the handover: {steps:?}"
                );
            }
            PlanStep::HandOverCard { .. } | PlanStep::SunshineFollows { .. } => {}
        }
    }
}

#[test]
fn a_switch_to_a_gpu_vm_with_enough_ram_plans_the_full_handover() {
    let outcome = switch(&guest("mac"), &host_gpu(), &snapshot(40 * GIB_MIB));
    assert_one_kernel_plan(&outcome, &KernelOwner::Host, &guest("mac"));
    match &outcome {
        SwitchOutcome::Plan(plan) => {
            let admission = plan
                .memory
                .expect("a guest target is admitted with a claim");
            assert_eq!(admission.required_mib, 24 * GIB_MIB);
            assert_eq!(admission.via_swap_mib, 0);
            assert_eq!(
                plan.steps,
                vec![
                    PlanStep::StopHostTenant {
                        tenant: "comfyui".to_owned(),
                        ladder: GpuLadder::BestEffort,
                    },
                    PlanStep::StopHostTenant {
                        tenant: "llm".to_owned(),
                        ladder: GpuLadder::Interactive,
                    },
                    PlanStep::StopHostTenant {
                        tenant: "lxc-desktop".to_owned(),
                        ladder: GpuLadder::Desktop,
                    },
                    PlanStep::HandOverCard {
                        device: "example-gpu".to_owned(),
                        from: KernelOwner::Host,
                        to: guest("mac"),
                    },
                    PlanStep::StartInstance {
                        instance: InstanceId::from("mac"),
                    },
                    PlanStep::SunshineFollows {
                        kernel: guest("mac")
                    },
                ]
            );
        }
        other => panic!("expected a plan, got {other:?}"),
    }
}

#[test]
fn a_switch_to_a_gpu_vm_without_enough_ram_waits_with_its_reason() {
    let outcome = switch(&guest("mac"), &host_gpu(), &snapshot(10 * GIB_MIB));
    assert_eq!(
        outcome,
        SwitchOutcome::Wait {
            reason: MemoryRefusal::NotEnoughFree {
                available_mib: 10 * GIB_MIB,
                required_mib: 24 * GIB_MIB,
                gap_mib: 14 * GIB_MIB,
                swap_free_mib: 0,
                max_swap_gap_mib: 8 * GIB_MIB,
            },
        }
    );
}

#[test]
fn a_switch_to_a_gpu_vm_larger_than_the_host_is_declined() {
    let memory = small_host_snapshot(4 * GIB_MIB, 2 * GIB_MIB, 2 * GIB_MIB);
    let outcome = switch(&guest("mac"), &host_gpu(), &memory);
    assert_eq!(
        outcome,
        SwitchOutcome::Decline {
            reason: SwitchRefusal::Memory(MemoryRefusal::ClaimExceedsHostCapacity {
                required_mib: 24 * GIB_MIB,
                ram_mib: 16 * GIB_MIB,
            }),
        }
    );
}

#[test]
fn a_switch_between_guests_stops_the_current_guest_cleanly_before_the_handover() {
    let mut gpu = host_gpu();
    gpu.owner = guest("linux-desktop");
    gpu.bound_to_vfio = true;
    gpu.tenants.iter_mut().for_each(|tenant| {
        tenant.state = TenantState::Stopped;
    });
    let outcome = switch(&guest("mac"), &gpu, &snapshot(40 * GIB_MIB));
    let steps = steps(&outcome);
    assert_eq!(
        steps[0],
        PlanStep::StopInstance {
            instance: InstanceId::from("linux-desktop"),
            mode: StopMode::Clean,
        }
    );
    assert_one_kernel_plan(&outcome, &guest("linux-desktop"), &guest("mac"));
}

#[test]
fn the_windows_desktop_requires_the_office_profile_stopped_cleanly_not_hibernated() {
    let outcome = switch(
        &guest("windows-desktop"),
        &host_gpu(),
        &snapshot(40 * GIB_MIB),
    );
    let steps = steps(&outcome);
    let office = position(steps, |step| {
        matches!(
            step,
            PlanStep::StopInstance { instance, mode: StopMode::Clean }
                if instance == &InstanceId::from("office")
        )
    })
    .expect("the running office profile is stopped");
    let handover =
        position(steps, |step| matches!(step, PlanStep::HandOverCard { .. })).expect("a handover");
    let start = position(steps, |step| {
        matches!(step, PlanStep::StartInstance { instance } if instance == &InstanceId::from("windows-desktop"))
    })
    .expect("the target starts");
    assert!(office < handover, "office stops before the card moves");
    assert!(handover < start, "the target starts after the card moves");
    assert!(
        !steps.iter().any(|step| matches!(
            step,
            PlanStep::StopInstance {
                mode: StopMode::Hibernate,
                ..
            }
        )),
        "a GPU handover never hibernates: {steps:?}"
    );
}

#[test]
fn the_handover_names_the_card_as_the_hardware_inventory_does() {
    let mut gpu = host_gpu();
    gpu.device = "example-card-2".to_owned();
    let outcome = switch(&guest("mac"), &gpu, &snapshot(40 * GIB_MIB));
    let steps = steps(&outcome);
    let handover = &steps[position(steps, |step| matches!(step, PlanStep::HandOverCard { .. }))
        .expect("the plan hands the card over")];
    assert_eq!(
        *handover,
        PlanStep::HandOverCard {
            device: "example-card-2".to_owned(),
            from: KernelOwner::Host,
            to: guest("mac"),
        }
    );

    // The plan is data the runtime resolves: a device name, never an address.
    let encoded = serde_json::to_value(handover).expect("the step serialises");
    assert_eq!(encoded["hand_over_card"]["device"], "example-card-2");
    assert!(encoded["hand_over_card"].get("address").is_none());
}

#[test]
fn a_guest_switch_credits_the_memory_of_the_guest_it_stops() {
    let mut gpu = host_gpu();
    gpu.owner = guest("mac");
    gpu.bound_to_vfio = true;
    gpu.tenants.iter_mut().for_each(|tenant| {
        tenant.state = TenantState::Stopped;
    });
    let mac = gpu
        .instances
        .iter_mut()
        .find(|instance| instance.id == InstanceId::from("mac"))
        .expect("the fixture has mac");
    mac.state = InstanceState::Running;
    mac.resident_mib = Some(24 * GIB_MIB);
    gpu.instances
        .iter_mut()
        .find(|instance| instance.id == InstanceId::from("office"))
        .expect("the fixture has office")
        .state = InstanceState::Stopped;

    // 10 GiB free plus the 24 GiB the plan frees by stopping the Mac desktop.
    let outcome = switch(&guest("windows-desktop"), &gpu, &snapshot(10 * GIB_MIB));
    let SwitchOutcome::Plan(plan) = outcome else {
        panic!("expected a plan, got {outcome:?}");
    };
    let admission = plan.memory.expect("the guest target is admitted");
    assert_eq!(admission.required_mib, 24 * GIB_MIB);
    assert_eq!(admission.credited_mib, 24 * GIB_MIB);
    assert_eq!(admission.available_mib, 34 * GIB_MIB);
    assert_eq!(admission.via_swap_mib, 0);
    assert_eq!(
        plan.steps[0],
        PlanStep::StopInstance {
            instance: InstanceId::from("mac"),
            mode: StopMode::Clean,
        }
    );
}

#[test]
fn a_windows_switch_credits_the_office_profile_it_stops() {
    let mut gpu = host_gpu();
    let office = gpu
        .instances
        .iter_mut()
        .find(|instance| instance.id == InstanceId::from("office"))
        .expect("the fixture has office");
    office.state = InstanceState::Running;
    office.resident_mib = Some(4 * GIB_MIB);

    // 22 GiB free plus the 4 GiB the office profile frees: admitted without
    // touching swap.
    let outcome = switch(&guest("windows-desktop"), &gpu, &snapshot(22 * GIB_MIB));
    let SwitchOutcome::Plan(plan) = outcome else {
        panic!("expected a plan, got {outcome:?}");
    };
    let admission = plan.memory.expect("the guest target is admitted");
    assert_eq!(admission.credited_mib, 4 * GIB_MIB);
    assert_eq!(admission.available_mib, 26 * GIB_MIB);
    assert_eq!(admission.via_swap_mib, 0);
}

#[test]
fn a_switch_to_an_unobserved_guest_is_declined() {
    assert_eq!(
        switch(&guest("ghost"), &host_gpu(), &snapshot(40 * GIB_MIB)),
        SwitchOutcome::Decline {
            reason: SwitchRefusal::UnknownTarget {
                instance: InstanceId::from("ghost"),
            },
        },
        "a plan never starts an instance that is not there"
    );
}

#[test]
fn a_switch_to_a_non_desktop_instance_is_declined() {
    assert_eq!(
        switch(&guest("dev"), &host_gpu(), &snapshot(40 * GIB_MIB)),
        SwitchOutcome::Decline {
            reason: SwitchRefusal::NotADesktopInstance {
                instance: InstanceId::from("dev"),
                class: InstanceClass::Dev,
            },
        }
    );
}

#[test]
fn office_pairing_is_by_install_not_by_guest_os() {
    // Two Windows installs, each with its office profile running. Switching to
    // one desktop stops only the office profile on the same install.
    let outcome = switch(
        &guest("windows-desktop"),
        &host_gpu(),
        &snapshot(40 * GIB_MIB),
    );
    let stopped = steps(&outcome)
        .iter()
        .filter_map(|step| match step {
            PlanStep::StopInstance { instance, .. } => Some(instance.as_str()),
            _ => None,
        })
        .collect::<Vec<&str>>();
    assert_eq!(stopped, vec!["office"], "only win-ssd's office stops");

    let outcome = switch(
        &guest("windows-other"),
        &host_gpu(),
        &snapshot(40 * GIB_MIB),
    );
    let stopped = steps(&outcome)
        .iter()
        .filter_map(|step| match step {
            PlanStep::StopInstance { instance, .. } => Some(instance.as_str()),
            _ => None,
        })
        .collect::<Vec<&str>>();
    assert_eq!(
        stopped,
        vec!["office-other"],
        "only win-other's office stops"
    );
}

#[test]
fn a_switch_to_the_office_profile_is_declined() {
    let outcome = switch(&guest("office"), &host_gpu(), &snapshot(40 * GIB_MIB));
    assert_eq!(
        outcome,
        SwitchOutcome::Decline {
            reason: SwitchRefusal::OfficeProfileCannotHoldTheCard {
                instance: InstanceId::from("office"),
            },
        }
    );
}

fn plan_of(outcome: SwitchOutcome) -> SwitchPlan {
    match outcome {
        SwitchOutcome::Plan(plan) => plan,
        other => panic!("expected a plan, got {other:?}"),
    }
}

fn table_holding(instance: &str, displaced: Vec<DisplacedTenant>) -> HolderTable {
    let mut table = HolderTable::new();
    let mut log = EventLog::new("synthetic-host", Source::Cdct, 1);
    let mut holder = card_holder(&format!("gpu-{instance}"), instance);
    holder.displaced = displaced;
    table.admit(holder, &mut log, Timestamp::from_secs(1));
    table
}

/// The host-side tenants all stopped, with `instance` holding the card.
fn guest_holds_card(gpu: &mut GpuState, instance: &str) {
    gpu.owner = guest(instance);
    gpu.bound_to_vfio = true;
    gpu.tenants.iter_mut().for_each(|tenant| {
        tenant.state = TenantState::Stopped;
    });
    if let Some(observed) = gpu
        .instances
        .iter_mut()
        .find(|observed| observed.id == InstanceId::from(instance))
    {
        observed.state = InstanceState::Running;
    }
}

#[test]
fn a_switch_back_to_host_restores_exactly_the_displaced_tenants_in_ladder_order_with_their_stored_replicas()
 {
    let displaced = vec![
        DisplacedTenant {
            tenant: "comfyui".to_owned(),
            ladder: GpuLadder::BestEffort,
            replicas: 1,
        },
        DisplacedTenant {
            tenant: "llm".to_owned(),
            ladder: GpuLadder::Interactive,
            replicas: 3,
        },
        DisplacedTenant {
            tenant: "lxc-desktop".to_owned(),
            ladder: GpuLadder::Desktop,
            replicas: 1,
        },
    ];
    let mut gpu = host_gpu();
    guest_holds_card(&mut gpu, "mac");
    let table = table_holding("mac", displaced);
    let outcome = conductor().switch(&KernelOwner::Host, &gpu, &snapshot(40 * GIB_MIB), &table);
    assert_one_kernel_plan(&outcome, &guest("mac"), &KernelOwner::Host);
    assert_eq!(
        steps(&outcome),
        &[
            PlanStep::StopInstance {
                instance: InstanceId::from("mac"),
                mode: StopMode::Clean,
            },
            PlanStep::HandOverCard {
                device: "example-gpu".to_owned(),
                from: guest("mac"),
                to: KernelOwner::Host,
            },
            PlanStep::StartHostTenant {
                tenant: "lxc-desktop".to_owned(),
                ladder: GpuLadder::Desktop,
                replicas: 1,
            },
            PlanStep::StartHostTenant {
                tenant: "llm".to_owned(),
                ladder: GpuLadder::Interactive,
                replicas: 3,
            },
            PlanStep::StartHostTenant {
                tenant: "comfyui".to_owned(),
                ladder: GpuLadder::BestEffort,
                replicas: 1,
            },
            PlanStep::SunshineFollows {
                kernel: KernelOwner::Host,
            },
        ]
    );
}

#[test]
fn a_guest_switch_records_what_it_displaced_and_leaves_idle_tenants_alone() {
    let mut gpu = host_gpu();
    // `comfyui` was already scaled to zero (an idle tenant behind the front):
    // the switch must neither stop nor later restart it.
    gpu.tenants
        .iter_mut()
        .find(|tenant| tenant.id == "comfyui")
        .expect("the fixture has comfyui")
        .state = TenantState::Stopped;
    let plan = plan_of(switch(&guest("mac"), &gpu, &snapshot(40 * GIB_MIB)));
    assert_eq!(
        plan.displaced,
        vec![
            DisplacedTenant {
                tenant: "llm".to_owned(),
                ladder: GpuLadder::Interactive,
                replicas: 1,
            },
            DisplacedTenant {
                tenant: "lxc-desktop".to_owned(),
                ladder: GpuLadder::Desktop,
                replicas: 1,
            },
        ],
        "only the tenants the switch actually stopped are recorded"
    );

    let table = table_holding("mac", plan.displaced.clone());
    guest_holds_card(&mut gpu, "mac");
    let outcome = conductor().switch(&KernelOwner::Host, &gpu, &snapshot(40 * GIB_MIB), &table);
    let starts: Vec<&PlanStep> = steps(&outcome)
        .iter()
        .filter(|step| matches!(step, PlanStep::StartHostTenant { .. }))
        .collect();
    assert_eq!(
        starts,
        vec![
            &PlanStep::StartHostTenant {
                tenant: "lxc-desktop".to_owned(),
                ladder: GpuLadder::Desktop,
                replicas: 1,
            },
            &PlanStep::StartHostTenant {
                tenant: "llm".to_owned(),
                ladder: GpuLadder::Interactive,
                replicas: 1,
            },
        ],
        "the idle tenant stays as it was: {outcome:?}"
    );
}

#[test]
fn switching_to_the_current_owner_is_already_held() {
    assert_eq!(
        switch(&KernelOwner::Host, &host_gpu(), &snapshot(40 * GIB_MIB)),
        SwitchOutcome::AlreadyHeld {
            kernel: KernelOwner::Host
        }
    );
    let mut gpu = host_gpu();
    gpu.owner = guest("mac");
    assert_eq!(
        switch(&guest("mac"), &gpu, &snapshot(40 * GIB_MIB)),
        SwitchOutcome::AlreadyHeld {
            kernel: guest("mac")
        }
    );
}

// ---------------------------------------------------------------------------
// The GPU ladder
// ---------------------------------------------------------------------------

fn ladder_input(signals: Vec<StarvationSignal>) -> LadderInput {
    LadderInput {
        owner: KernelOwner::Host,
        vram_full: true,
        now: Timestamp::from_secs(100),
        cooldown_until: Timestamp(0),
        tenants: vec![
            tenant("lxc-desktop", GpuLadder::Desktop, TenantState::Running),
            tenant("llm", GpuLadder::Interactive, TenantState::Running),
            tenant("comfyui", GpuLadder::BestEffort, TenantState::Running),
        ],
        signals,
    }
}

fn starvation_signals() -> Vec<StarvationSignal> {
    vec![
        StarvationSignal::PodNotReady {
            tenant: "llm".to_owned(),
            ladder: GpuLadder::Interactive,
            starved_ticks: 2,
        },
        StarvationSignal::DesktopGttSpill {
            gtt_used_bytes: 2 * GIB_BYTES,
            gtt_previous_bytes: GIB_BYTES,
            starved_ticks: 2,
        },
        StarvationSignal::RegistrationZombie {
            resource: "example.com/gpu-slot".to_owned(),
            starved_ticks: 5,
        },
        StarvationSignal::BrokerModelStarved {
            stuck_models: 1,
            starved_ticks: 2,
        },
    ]
}

#[test]
fn the_ladder_decides_under_each_starvation_signal() {
    for signal in starvation_signals() {
        let decision = conductor().ladder(&ladder_input(vec![signal.clone()]));
        assert_eq!(
            decision,
            LadderDecision::ScaleLowestPriorityToZero {
                tenant: "comfyui".to_owned(),
                ladder: GpuLadder::BestEffort,
                signal,
            },
            "every signal sheds the lowest-priority tenant"
        );
    }
}

#[test]
fn the_ladder_waits_for_the_grace_ticks() {
    let decision = conductor().ladder(&ladder_input(vec![StarvationSignal::PodNotReady {
        tenant: "llm".to_owned(),
        ladder: GpuLadder::Interactive,
        starved_ticks: 1,
    }]));
    assert_eq!(
        decision,
        LadderDecision::None {
            reason: LadderHold::NoGracedSignal
        }
    );
}

#[test]
fn the_ladder_ignores_gtt_noise() {
    let decision = conductor().ladder(&ladder_input(vec![StarvationSignal::DesktopGttSpill {
        gtt_used_bytes: GIB_BYTES + 1024 * 1024,
        gtt_previous_bytes: GIB_BYTES,
        starved_ticks: 5,
    }]));
    assert_eq!(
        decision,
        LadderDecision::None {
            reason: LadderHold::NoGracedSignal
        }
    );
}

#[test]
fn the_ladder_does_nothing_when_the_card_is_not_full() {
    let mut input = ladder_input(starvation_signals());
    input.vram_full = false;
    assert_eq!(
        conductor().ladder(&input),
        LadderDecision::None {
            reason: LadderHold::CardNotFull
        }
    );
}

#[test]
fn the_ladder_waits_until_the_reclaim_cooldown_deadline() {
    let mut input = ladder_input(starvation_signals());
    // The decision is made at 100 s; the cooldown runs until 130 s.
    input.cooldown_until = Timestamp::from_secs(130);
    assert_eq!(
        conductor().ladder(&input),
        LadderDecision::None {
            reason: LadderHold::Cooldown {
                until: Timestamp::from_secs(130)
            }
        }
    );
}

#[test]
fn the_ladder_resumes_once_the_cooldown_deadline_has_passed() {
    let mut input = ladder_input(starvation_signals());
    input.now = Timestamp::from_secs(130);
    input.cooldown_until = Timestamp::from_secs(130);
    assert_eq!(
        conductor().ladder(&input),
        LadderDecision::ScaleLowestPriorityToZero {
            tenant: "comfyui".to_owned(),
            ladder: GpuLadder::BestEffort,
            signal: StarvationSignal::RegistrationZombie {
                resource: "example.com/gpu-slot".to_owned(),
                starved_ticks: 5,
            },
        },
        "the ladder acts again at the deadline"
    );
}

#[test]
fn the_cooldown_deadline_is_policy_time_not_ticks() {
    let policy = Policy::default().gpu;
    assert_eq!(policy.cooldown_secs, 18);
    assert_eq!(
        policy.cooldown_until(Timestamp::from_secs(10)),
        Timestamp::from_secs(28)
    );
    assert_eq!(
        policy.cooldown_until(Timestamp::from_millis(1_500)),
        Timestamp::from_millis(19_500)
    );
}

#[test]
fn the_ladder_never_evicts_a_tenant_at_or_above_the_starved_priority() {
    let mut input = ladder_input(vec![StarvationSignal::PodNotReady {
        tenant: "llm".to_owned(),
        ladder: GpuLadder::Interactive,
        starved_ticks: 2,
    }]);
    input.tenants = vec![tenant(
        "lxc-desktop",
        GpuLadder::Desktop,
        TenantState::Running,
    )];
    assert_eq!(
        conductor().ladder(&input),
        LadderDecision::None {
            reason: LadderHold::NoLowerPriorityTenant
        }
    );
}

#[test]
fn the_ladder_never_acts_while_a_guest_owns_the_card() {
    let mut input = ladder_input(starvation_signals());
    input.owner = guest("mac");
    assert_eq!(
        conductor().ladder(&input),
        LadderDecision::None {
            reason: LadderHold::NotHostOwned
        }
    );
}

// ---------------------------------------------------------------------------
// The holder table
// ---------------------------------------------------------------------------

fn card_holder(id: &str, instance: &str) -> Holder {
    Holder {
        id: HolderId::from(id),
        lease: LeaseId::from(format!("lease-{id}")),
        instance: Some(InstanceId::from(instance)),
        tenant: None,
        class: InstanceClass::Desktop,
        priority: GpuLadder::Desktop.priority(),
        memory_mib: 24 * GIB_MIB,
        cpu_cores: 6.0,
        devices: vec!["example-gpu".to_owned()],
        displaced: Vec::new(),
        state: HolderState::Running,
        since: Timestamp(0),
        last_event_seq: 0,
    }
}

#[test]
fn admitting_the_card_to_a_guest_moves_ownership_and_releasing_returns_it() {
    let mut table = HolderTable::new();
    let mut log = EventLog::new("synthetic-host", Source::Cdct, 1);
    let granted = table.admit(
        card_holder("gpu-mac", "mac"),
        &mut log,
        Timestamp::from_secs(1),
    );
    assert_eq!(granted.kind, "admission.granted");
    assert_eq!(table.gpu_owner, guest("mac"));
    assert_eq!(table.holders[0].last_event_seq, granted.seq);
    assert_eq!(table.seq, granted.seq);

    let released = table
        .release(
            &HolderId::from("gpu-mac"),
            &mut log,
            Timestamp::from_secs(2),
            "switched back",
        )
        .expect("the holder is released");
    assert_eq!(released.kind, "holder.released");
    assert_eq!(table.gpu_owner, KernelOwner::Host);
    assert!(table.holders.is_empty());
    assert_eq!(table.seq, released.seq);
}

#[test]
fn the_holder_store_round_trips_and_keeps_one_writer() {
    let tree = TempTree::new("store");
    let path = tree.path().join("holders.json");
    {
        let mut store = HolderStore::open(&path).expect("open the store");
        let mut table = store.load().expect("an empty store loads an empty table");
        assert!(table.holders.is_empty());
        let mut log = EventLog::new("synthetic-host", Source::Cdct, 1);
        table.admit(
            card_holder("gpu-mac", "mac"),
            &mut log,
            Timestamp::from_secs(1),
        );
        store.save(&table).expect("save the table");

        let probe = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .expect("open a probe");
        assert!(
            probe.try_lock_exclusive().is_err(),
            "a second writer is locked out while the store is open"
        );
    }
    let mut store = HolderStore::open(&path).expect("reopen the store");
    let table = store.load().expect("load the table");
    assert_eq!(table.holders.len(), 1);
    assert_eq!(table.gpu_owner, guest("mac"));
}

#[test]
fn reading_a_missing_holder_table_is_absent_not_an_error() {
    let tree = TempTree::new("absent");
    assert_eq!(
        read_table(tree.path().join("holders.json")).expect("no error"),
        None
    );
}

// ---------------------------------------------------------------------------
// Reconcile after a crash
// ---------------------------------------------------------------------------

#[test]
fn reconcile_releases_a_holder_whose_vm_is_gone_and_returns_the_unheld_card() {
    let mut log = EventLog::new("synthetic-host", Source::Cdct, 1);
    let mut table = HolderTable::new();
    table.admit(
        card_holder("gpu-mac", "mac"),
        &mut log,
        Timestamp::from_secs(1),
    );
    let observed = ObservedHost {
        instances: Vec::new(),
        device: "example-gpu".to_owned(),
        device_driver: ObservedDeviceDriver::Passthrough,
    };
    let report: ReconcileReport =
        conductor().reconcile(table, &observed, &mut log, Timestamp::from_secs(2));
    assert_eq!(
        report.repairs,
        vec![
            Repair::ReleaseHolder {
                holder: HolderId::from("gpu-mac"),
                instance: InstanceId::from("mac"),
                reason: ReconcileReason::InstanceGone,
            },
            Repair::ReturnCardToHost {
                device: "example-gpu".to_owned(),
                reason: ReconcileReason::BoundWithoutHolder,
            },
        ]
    );
    assert!(report.table.holders.is_empty());
    assert_eq!(report.table.gpu_owner, KernelOwner::Host);
    assert_eq!(report.events.len(), 2);
    assert!(
        report
            .events
            .iter()
            .all(|event| event.kind == "holder.reconciled")
    );
}

#[test]
fn reconcile_reports_a_live_instance_without_a_holder_and_does_not_adopt_it() {
    let mut log = EventLog::new("synthetic-host", Source::Cdct, 1);
    let observed = ObservedHost {
        instances: vec![ObservedInstance {
            id: InstanceId::from("ci-job"),
            state: InstanceState::Running,
        }],
        device: "example-gpu".to_owned(),
        device_driver: ObservedDeviceDriver::Host,
    };
    let report = conductor().reconcile(
        HolderTable::new(),
        &observed,
        &mut log,
        Timestamp::from_secs(2),
    );
    assert_eq!(
        report.repairs,
        vec![Repair::ReportOrphan {
            instance: InstanceId::from("ci-job"),
            reason: ReconcileReason::NoHolder,
        }]
    );
    assert!(report.table.holders.is_empty(), "no lease is invented");
    assert_eq!(report.events.len(), 1);
    assert_eq!(report.events[0].severity, cdrv::Severity::Warn);
    assert_eq!(report.events[0].kind, "holder.reconciled");
}

// ---------------------------------------------------------------------------
// Facts documents, the live reader, and policy overrides
// ---------------------------------------------------------------------------

#[test]
fn the_facts_document_carries_raw_text_and_the_observed_gpu() {
    let tree = TempTree::new("facts");
    let facts = FactsFile {
        meminfo: meminfo_text(128 * GIB_MIB, 40 * GIB_MIB, 0, 0),
        arcstats: None,
        pressure_memory: pressure_text(0.0, 0.0),
        gpu: host_gpu(),
    };
    let path = tree.path().join("facts.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&facts).expect("serialise")).expect("write");
    let read = FactsFile::read(&path).expect("read the facts");
    assert_eq!(read, facts);
    let memory = read.memory_snapshot(&path).expect("parse the memory facts");
    assert_eq!(
        MemoryHeadroom::from_snapshot(&memory).available_mib(),
        40 * GIB_MIB
    );
    assert_eq!(read.gpu.owner, KernelOwner::Host);
}

#[test]
fn the_reader_reads_a_fixture_proc_root() {
    let tree = TempTree::new("proc");
    std::fs::create_dir_all(tree.path().join("spl/kstat/zfs")).expect("arcstats dir");
    std::fs::create_dir_all(tree.path().join("pressure")).expect("pressure dir");
    std::fs::write(
        tree.path().join("meminfo"),
        meminfo_text(128 * GIB_MIB, 16 * GIB_MIB, 0, 0),
    )
    .expect("meminfo");
    std::fs::write(
        tree.path().join("spl/kstat/zfs/arcstats"),
        arcstats_text(
            32 * GIB_BYTES,
            4 * GIB_BYTES,
            [2 * GIB_BYTES, 2 * GIB_BYTES, 2 * GIB_BYTES, 2 * GIB_BYTES],
        ),
    )
    .expect("arcstats");
    std::fs::write(tree.path().join("pressure/memory"), pressure_text(1.0, 0.5)).expect("pressure");
    let reader = ProcReader::new(tree.path());
    let raw = reader.raw_facts().expect("read the fixture");
    let snapshot = MemorySnapshot::parse(&raw).expect("parse the fixture");
    assert_eq!(
        MemoryHeadroom::from_snapshot(&snapshot).available_mib(),
        24 * GIB_MIB
    );
    std::fs::remove_file(tree.path().join("spl/kstat/zfs/arcstats")).expect("remove arcstats");
    assert!(
        reader.arcstats().expect("read without ZFS").is_none(),
        "a host without ZFS has no ARC"
    );
}

#[test]
fn policy_json_overrides_only_the_named_fields() {
    let policy = Policy::from_json(r#"{"memory":{"gpu_vm_claim_mib":16384}}"#)
        .expect("the partial policy parses");
    assert_eq!(policy.memory.gpu_vm_claim_mib, 16 * GIB_MIB);
    assert_eq!(policy.gpu.vram_full_fraction, 0.85);
    assert_eq!(policy.gpu.grace_ticks, 2);
    assert!(policy.memory.swap.enabled);
}

// ---------------------------------------------------------------------------
// Temporary fixture directories
// ---------------------------------------------------------------------------

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempTree {
    root: PathBuf,
}

impl TempTree {
    fn new(label: &str) -> Self {
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("cdct-it-{}-{label}-{id}", std::process::id()));
        std::fs::create_dir_all(&root).expect("create the fixture directory");
        Self { root }
    }

    fn path(&self) -> &Path {
        &self.root
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn a_holder_table_round_trip_keeps_the_replica_count() {
    let table = table_holding(
        "mac",
        vec![DisplacedTenant {
            tenant: "llm".to_owned(),
            ladder: GpuLadder::Interactive,
            replicas: 3,
        }],
    );
    let text = serde_json::to_string(&table).expect("the table serializes");
    let back: HolderTable = serde_json::from_str(&text).expect("the table reads back");
    assert_eq!(back.holders[0].displaced[0].replicas, 3);
}

#[test]
fn an_old_table_without_replicas_reads_as_one() {
    let tenant: DisplacedTenant =
        serde_json::from_str(r#"{"tenant":"llm","ladder":"gpu-interactive"}"#)
            .expect("an old entry");
    assert_eq!(tenant.replicas, 1);
}
