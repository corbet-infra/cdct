//! The live ladder's restore decision: a shed tenant returns only when the
//! VRAM it freed is free again, never while a tenant starves or a cooldown runs.

use cdct::{
    GpuLadder, GpuPolicy, GpuTenant, KernelOwner, PressureObservation, RestoreDecision,
    RestoreHold, ShedTenant, StarvationSignal, TenantState, VramFacts, decide_guard,
    decide_restore, measure_freed,
};
use cdrv::Timestamp;

use super::guest;

const GIB: u64 = 1 << 30;

fn vram(used_gib: u64) -> Option<VramFacts> {
    Some(VramFacts {
        used_bytes: used_gib * GIB,
        total_bytes: 16 * GIB,
    })
}

fn shed(freed_gib: Option<u64>, replicas: u32) -> ShedTenant {
    ShedTenant {
        tenant: "render/farm".to_owned(),
        ladder: GpuLadder::BestEffort,
        replicas,
        at: Timestamp::from_millis(1_000),
        vram_used_at_shed: Some(15 * GIB),
        freed_bytes: freed_gib.map(|gib| gib * GIB),
    }
}

fn observation(used_gib: u64, signals: Vec<StarvationSignal>) -> PressureObservation {
    PressureObservation {
        vram: vram(used_gib),
        tenants: vec![GpuTenant {
            id: "llm/broker".to_owned(),
            ladder: GpuLadder::Interactive,
            state: TenantState::Running,
        }],
        signals,
    }
}

fn decide(
    owner: &KernelOwner,
    seen: &PressureObservation,
    list: &[ShedTenant],
    now_ms: u64,
    cooldown_ms: u64,
) -> RestoreDecision {
    decide_restore(
        owner,
        seen,
        list,
        Timestamp::from_millis(now_ms),
        Timestamp::from_millis(cooldown_ms),
        &GpuPolicy::default(),
    )
}

#[test]
fn the_freed_vram_is_measured_from_the_usage_at_the_shed() {
    assert_eq!(measure_freed(&shed(None, 1), vram(11)), Some(4 * GIB));
    // Usage that grew since the shed freed nothing (never a negative amount).
    assert_eq!(measure_freed(&shed(None, 1), vram(16)), Some(0));
    assert_eq!(measure_freed(&shed(None, 1), None), None);
}

#[test]
fn a_shed_tenant_returns_when_its_vram_is_free_again() {
    let list = [shed(Some(4), 2)];
    let host = KernelOwner::Host;
    assert_eq!(
        decide(&host, &observation(10, Vec::new()), &list, 50_000, 20_000),
        RestoreDecision::Restore {
            tenant: "render/farm".to_owned(),
            ladder: GpuLadder::BestEffort,
            replicas: 2,
        }
    );
    // 13 of 16 GiB in use: 3 GiB free, the tenant freed 4.
    assert_eq!(
        decide(&host, &observation(13, Vec::new()), &list, 50_000, 20_000),
        RestoreDecision::None {
            reason: RestoreHold::NoRoom {
                free_bytes: 3 * GIB,
                needed_bytes: 4 * GIB,
            },
        }
    );
}

#[test]
fn nothing_returns_while_a_cooldown_runs_a_tenant_starves_or_a_guest_holds_the_card() {
    let list = [shed(Some(4), 2)];
    let host = KernelOwner::Host;
    assert_eq!(
        decide(&host, &observation(4, Vec::new()), &list, 10_000, 20_000),
        RestoreDecision::None {
            reason: RestoreHold::Cooldown {
                until: Timestamp::from_millis(20_000),
            },
        }
    );
    let starved = vec![StarvationSignal::PodNotReady {
        tenant: "llm/broker".to_owned(),
        ladder: GpuLadder::Interactive,
        starved_ticks: 2,
    }];
    assert_eq!(
        decide(&host, &observation(4, starved), &list, 50_000, 20_000),
        RestoreDecision::None {
            reason: RestoreHold::Starved,
        }
    );
    assert_eq!(
        decide(
            &guest("mac"),
            &observation(4, Vec::new()),
            &list,
            50_000,
            20_000
        ),
        RestoreDecision::None {
            reason: RestoreHold::NotHostOwned,
        }
    );
}

#[test]
fn an_unmeasured_or_uncounted_shed_does_not_return() {
    let host = KernelOwner::Host;
    assert_eq!(
        decide(
            &host,
            &observation(4, Vec::new()),
            &[shed(None, 2)],
            50_000,
            0
        ),
        RestoreDecision::None {
            reason: RestoreHold::NotMeasured,
        }
    );
    assert_eq!(
        decide(
            &host,
            &observation(4, Vec::new()),
            &[shed(Some(4), 0)],
            50_000,
            0
        ),
        RestoreDecision::None {
            reason: RestoreHold::NothingShed,
        }
    );
}

fn zombie(ticks: u32) -> StarvationSignal {
    StarvationSignal::RegistrationZombie {
        resource: "example.com/gpu-slot".to_owned(),
        starved_ticks: ticks,
    }
}

#[test]
fn the_device_plugin_restarts_after_its_grace_and_only_on_the_host() {
    let policy = GpuPolicy::default();
    let host = KernelOwner::Host;
    let at = Timestamp::from_millis(10_000);
    let none = Timestamp::from_millis(0);
    assert!(!decide_guard(
        &host,
        &observation(4, vec![zombie(4)]),
        at,
        none,
        &policy
    ));
    assert!(decide_guard(
        &host,
        &observation(4, vec![zombie(5)]),
        at,
        none,
        &policy
    ));
    // While a guest holds the card the resources read zero by design.
    assert!(!decide_guard(
        &guest("mac"),
        &observation(4, vec![zombie(9)]),
        at,
        none,
        &policy
    ));
    // One restart per cooldown.
    assert!(!decide_guard(
        &host,
        &observation(4, vec![zombie(9)]),
        at,
        Timestamp::from_millis(20_000),
        &policy
    ));
    assert_eq!(
        policy.guard_cooldown_until(at),
        Timestamp::from_millis(310_000)
    );
}
