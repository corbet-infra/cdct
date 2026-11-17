//! The daemon over a temporary Unix socket: status, switch, subscribe and the
//! guest channel that identifies its instance.

use std::path::{Path, PathBuf};

use cdct::client::{Client, Endpoint};
use cdct::config::{Config, InstanceConfig, TargetConfig, TenantConfig};
use cdct::holders::read_table;
use cdct::protocol::{Availability, Channel, Reply, Request, Status};
use cdct::{DisplacedTenant, KernelOwner, MemoryRefusal, PlanStep, Policy, SwitchOutcome};
use cdrv::{GpuLadder, GuestOs, InstanceClass, InstanceId};

use super::{GIB_MIB, TempTree, guest, meminfo_text, pressure_text};

fn proc_fixture(tree: &TempTree, available_mib: u64) -> PathBuf {
    let root = tree.path().join("proc");
    std::fs::create_dir_all(root.join("pressure")).expect("the proc fixture directory");
    std::fs::write(
        root.join("meminfo"),
        meminfo_text(128 * GIB_MIB, available_mib, 0, 0),
    )
    .expect("the meminfo fixture");
    std::fs::write(root.join("pressure/memory"), pressure_text(0.0, 0.0))
        .expect("the pressure fixture");
    root
}

fn configuration(tree: &TempTree, available_mib: u64) -> Config {
    Config {
        state: tree.path().join("holders.json"),
        socket: tree.path().join("control.sock"),
        host: "synthetic-host".to_owned(),
        gpu_device: "example-gpu".to_owned(),
        proc_root: proc_fixture(tree, available_mib),
        facts: None,
        socket_mode: 0o660,
        socket_gid: None,
        policy: Policy::default(),
        instances: vec![InstanceConfig {
            id: InstanceId::from("mac"),
            class: InstanceClass::Desktop,
            guest_os: GuestOs::MacOs,
            install: None,
            memory_mib: None,
            resident_mib: None,
            channel: Some(tree.path().join("mac.sock")),
        }],
        targets: vec![
            TargetConfig {
                name: "linux".to_owned(),
                label: "Back to Linux".to_owned(),
                holder: "Linux".to_owned(),
                kernel: KernelOwner::Host,
            },
            TargetConfig {
                name: "macos".to_owned(),
                label: "Boot macOS".to_owned(),
                holder: "macOS".to_owned(),
                kernel: guest("mac"),
            },
        ],
        tenants: Vec::new(),
    }
}

fn start(tree: &TempTree, available_mib: u64) -> cdct::daemon::Running {
    cdct::daemon::start(configuration(tree, available_mib)).expect("the daemon starts")
}

fn connect(path: &Path) -> Client {
    Client::connect(&Endpoint::LocalSocket(path.to_path_buf())).expect("the client connects")
}

fn status_of(reply: Reply) -> Status {
    match reply {
        Reply::Status { status } => status,
        other => panic!("expected a status, got {other:?}"),
    }
}

fn switch(client: &mut Client, target: KernelOwner) -> SwitchOutcome {
    match client
        .ask(&Request::Switch { target })
        .expect("the switch is answered")
    {
        Reply::Switch { outcome } => outcome,
        other => panic!("expected a switch outcome, got {other:?}"),
    }
}

#[test]
fn the_daemon_answers_status_over_a_temporary_socket() {
    let tree = TempTree::new("daemon-status");
    let running = start(&tree, 40 * GIB_MIB);
    let mut client = connect(running.local_socket());

    let status = status_of(client.ask(&Request::Status).expect("status is answered"));
    assert_eq!(status.owner, KernelOwner::Host);
    assert_eq!(status.owner_label, "Linux");
    assert!(status.holders.is_empty());
    let macos = status
        .targets
        .iter()
        .find(|target| target.name == "macos")
        .expect("the macOS target is configured");
    assert_eq!(macos.availability, Availability::Ready);
    let linux = status
        .targets
        .iter()
        .find(|target| target.name == "linux")
        .expect("the Linux target is configured");
    assert_eq!(linux.availability, Availability::AlreadyHeld);
}

#[test]
fn a_switch_is_decided_recorded_and_persisted() {
    let tree = TempTree::new("daemon-switch");
    let config = configuration(&tree, 40 * GIB_MIB);
    let running = cdct::daemon::start(config.clone()).expect("the daemon starts");
    let mut client = connect(running.local_socket());

    let outcome = switch(&mut client, guest("mac"));
    match &outcome {
        SwitchOutcome::Plan(plan) => {
            let admission = plan.memory.expect("a guest target carries its admission");
            assert_eq!(admission.required_mib, 24 * GIB_MIB);
            assert_eq!(plan.target, guest("mac"));
        }
        other => panic!("expected a plan, got {other:?}"),
    }

    let status = status_of(client.ask(&Request::Status).expect("status is answered"));
    assert_eq!(status.owner, guest("mac"));
    assert_eq!(status.owner_label, "macOS");
    assert_eq!(status.holders.len(), 1);
    let holder = &status.holders[0];
    assert_eq!(holder.instance, Some(InstanceId::from("mac")));
    assert_eq!(holder.memory_mib, 24 * GIB_MIB);
    assert_eq!(holder.devices, vec!["example-gpu".to_owned()]);

    let persisted = read_table(&config.state)
        .expect("the table reads")
        .expect("the table exists");
    assert_eq!(persisted.gpu_owner, guest("mac"));
    assert_eq!(persisted.holders.len(), 1);
}

#[test]
fn a_switch_without_ram_waits_and_changes_nothing() {
    let tree = TempTree::new("daemon-wait");
    let running = start(&tree, 10 * GIB_MIB);
    let mut client = connect(running.local_socket());

    let outcome = switch(&mut client, guest("mac"));
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
    let status = status_of(client.ask(&Request::Status).expect("status is answered"));
    assert_eq!(status.owner, KernelOwner::Host);
    assert!(status.holders.is_empty(), "nothing is displaced for RAM");
}

#[test]
fn planning_a_switch_changes_nothing() {
    let tree = TempTree::new("daemon-plan");
    let running = start(&tree, 40 * GIB_MIB);
    let mut client = connect(running.local_socket());

    let reply = client
        .ask(&Request::PlanSwitch {
            target: guest("mac"),
        })
        .expect("the plan is answered");
    match reply {
        Reply::Plan {
            outcome: SwitchOutcome::Plan(_),
        } => {}
        other => panic!("expected a plan, got {other:?}"),
    }
    let status = status_of(client.ask(&Request::Status).expect("status is answered"));
    assert_eq!(status.owner, KernelOwner::Host);
    assert!(status.holders.is_empty());
}

#[test]
fn a_subscriber_receives_the_change_as_an_event_then_a_snapshot() {
    let tree = TempTree::new("daemon-subscribe");
    let running = start(&tree, 40 * GIB_MIB);
    let mut watcher = connect(running.local_socket());
    watcher
        .send(&Request::Subscribe)
        .expect("the subscription is sent");
    match watcher.receive().expect("the subscription is answered") {
        Some(Reply::Subscribed {
            channel: Channel::Local,
            status,
        }) => assert_eq!(status.owner, KernelOwner::Host),
        other => panic!("expected the subscription snapshot, got {other:?}"),
    }

    let mut actor = connect(running.local_socket());
    switch(&mut actor, guest("mac"));

    assert!(matches!(
        watcher.receive().expect("the granted plan arrives"),
        Some(Reply::Granted { .. })
    ));
    match watcher.receive().expect("the event arrives") {
        Some(Reply::Event { event }) => {
            assert_eq!(event.kind, "admission.granted");
            assert_eq!(
                event.subject,
                cdrv::Subject::Holder(cdrv::HolderId::from("card-mac"))
            );
        }
        other => panic!("expected the admission event, got {other:?}"),
    }
    let status = status_of(
        watcher
            .receive()
            .expect("the snapshot arrives")
            .expect("the snapshot is present"),
    );
    assert_eq!(status.owner, guest("mac"));

    switch(&mut actor, KernelOwner::Host);
    assert!(matches!(
        watcher.receive().expect("the granted plan arrives"),
        Some(Reply::Granted { .. })
    ));
    // The return is only granted: the guest keeps the card until the executor
    // reports the plan complete.
    let status = status_of(
        watcher
            .receive()
            .expect("the snapshot arrives")
            .expect("the snapshot is present"),
    );
    assert_eq!(status.owner, guest("mac"));
    assert!(
        matches!(
            switch(&mut actor, KernelOwner::Host),
            SwitchOutcome::Plan(_)
        ),
        "a return that has not completed can be asked for again"
    );
    assert!(matches!(
        watcher.receive().expect("the second grant arrives"),
        Some(Reply::Granted { .. })
    ));
    let _ = watcher.receive().expect("its snapshot arrives");

    assert_eq!(
        actor
            .completed(&KernelOwner::Host)
            .expect("the completion is accepted"),
        KernelOwner::Host
    );
    match watcher.receive().expect("the release event arrives") {
        Some(Reply::Event { event }) => assert_eq!(event.kind, "holder.released"),
        other => panic!("expected the release event, got {other:?}"),
    }
    let status = status_of(
        watcher
            .receive()
            .expect("the snapshot arrives")
            .expect("the snapshot is present"),
    );
    assert_eq!(status.owner, KernelOwner::Host);
    assert!(status.holders.is_empty());
}

#[test]
fn a_guest_channel_listener_identifies_its_instance() {
    let tree = TempTree::new("daemon-guest");
    let running = start(&tree, 40 * GIB_MIB);
    let path = running
        .guest_socket(&InstanceId::from("mac"))
        .expect("the guest channel is bound")
        .to_path_buf();
    let mut guest_client = connect(&path);
    guest_client
        .send(&Request::Subscribe)
        .expect("the subscription is sent");
    match guest_client
        .receive()
        .expect("the subscription is answered")
    {
        Some(Reply::Subscribed {
            channel: Channel::Guest { instance },
            status,
        }) => {
            assert_eq!(instance, InstanceId::from("mac"));
            assert_eq!(status.owner, KernelOwner::Host);
        }
        other => panic!("expected a guest subscription, got {other:?}"),
    }
}

#[test]
fn a_second_daemon_cannot_take_the_writer_lock() {
    let tree = TempTree::new("daemon-lock");
    let config = configuration(&tree, 40 * GIB_MIB);
    let _running = cdct::daemon::start(config.clone()).expect("the first daemon starts");
    let second = cdct::daemon::start(config)
        .err()
        .expect("the second daemon is refused");
    assert!(
        matches!(
            second,
            cdct::daemon::ServeError::Store(cdct::holders::StoreError::Locked { .. })
        ),
        "the second daemon reports the conflict: {second}"
    );
}

#[test]
fn a_stale_socket_file_does_not_stop_the_daemon_starting() {
    let tree = TempTree::new("daemon-stale");
    let config = configuration(&tree, 100 * 1024);
    drop(std::os::unix::net::UnixListener::bind(&config.socket).expect("a leftover socket"));
    assert!(config.socket.exists());
    let running = cdct::daemon::start(config).expect("the daemon replaces the stale socket");
    drop(running);
}

fn declared_tenants() -> Vec<TenantConfig> {
    vec![
        TenantConfig {
            id: "llm/broker".to_owned(),
            ladder: GpuLadder::Interactive,
        },
        TenantConfig {
            id: "render/farm".to_owned(),
            ladder: GpuLadder::BestEffort,
        },
    ]
}

fn plan_of(client: &mut Client, request: Request) -> cdct::SwitchPlan {
    match client.ask(&request).expect("the plan is answered") {
        Reply::Plan {
            outcome: SwitchOutcome::Plan(plan),
        } => plan,
        other => panic!("expected a plan, got {other:?}"),
    }
}

#[test]
fn declared_tenants_are_displaced_and_restored_without_a_facts_file() {
    let tree = TempTree::new("daemon-tenants");
    let mut config = configuration(&tree, 40 * GIB_MIB);
    config.tenants = declared_tenants();
    let running = cdct::daemon::start(config).expect("the daemon starts");
    let mut client = connect(running.local_socket());

    let plan = plan_of(
        &mut client,
        Request::PlanSwitch {
            target: guest("mac"),
        },
    );
    let stops: Vec<&str> = plan
        .steps
        .iter()
        .filter_map(|step| match step {
            PlanStep::StopHostTenant { tenant, .. } => Some(tenant.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        stops,
        vec!["render/farm", "llm/broker"],
        "lowest rung first"
    );
    let displaced: Vec<&str> = plan.displaced.iter().map(|d| d.tenant.as_str()).collect();
    assert_eq!(displaced, stops);

    switch(&mut client, guest("mac"));
    // cwld records the counts it read before the stops (placeholders are 0).
    let counted: Vec<_> = plan
        .displaced
        .iter()
        .map(|tenant| cdct::DisplacedTenant {
            replicas: 1,
            ..tenant.clone()
        })
        .collect();
    client
        .record_displaced(&counted)
        .expect("the counts are recorded");
    let back = plan_of(
        &mut client,
        Request::PlanSwitch {
            target: KernelOwner::Host,
        },
    );
    let mut starts: Vec<&str> = back
        .steps
        .iter()
        .filter_map(|step| match step {
            PlanStep::StartHostTenant { tenant, .. } => Some(tenant.as_str()),
            _ => None,
        })
        .collect();
    starts.sort_unstable();
    assert_eq!(starts, vec!["llm/broker", "render/farm"]);
}

#[test]
fn a_subscriber_receives_the_granted_plan_before_the_events_and_the_status() {
    let tree = TempTree::new("daemon-granted");
    let mut config = configuration(&tree, 40 * GIB_MIB);
    config.tenants = declared_tenants();
    let running = cdct::daemon::start(config).expect("the daemon starts");
    let mut watcher = connect(running.local_socket());
    watcher
        .send(&Request::Subscribe)
        .expect("the subscription is sent");
    assert!(matches!(
        watcher.receive().expect("the subscription is answered"),
        Some(Reply::Subscribed { .. })
    ));

    let mut actor = connect(running.local_socket());
    let SwitchOutcome::Plan(given) = switch(&mut actor, guest("mac")) else {
        panic!("expected a plan");
    };

    match watcher.receive().expect("the plan arrives") {
        Some(Reply::Granted { plan }) => assert_eq!(plan, given),
        other => panic!("expected the granted plan first, got {other:?}"),
    }
    assert!(matches!(
        watcher.receive().expect("the event arrives"),
        Some(Reply::Event { .. })
    ));
    assert!(matches!(
        watcher.receive().expect("the status arrives"),
        Some(Reply::Status { .. })
    ));
}

#[test]
fn a_switch_that_changes_nothing_pushes_no_granted_plan() {
    let tree = TempTree::new("daemon-not-granted");
    let running = start(&tree, 40 * GIB_MIB);
    let mut watcher = connect(running.local_socket());
    watcher
        .send(&Request::Subscribe)
        .expect("the subscription is sent");
    watcher.receive().expect("the subscription is answered");

    let mut actor = connect(running.local_socket());
    assert!(matches!(
        switch(&mut actor, KernelOwner::Host),
        SwitchOutcome::AlreadyHeld { .. }
    ));
    switch(&mut actor, guest("mac"));
    // The no-op switch pushed only a status; the first granted plan is the
    // real grant.
    assert!(matches!(
        watcher.receive().expect("the status arrives"),
        Some(Reply::Status { .. })
    ));
    assert!(matches!(
        watcher.receive().expect("the grant arrives"),
        Some(Reply::Granted { .. })
    ));
}

fn restore_counts(plan: &cdct::SwitchPlan) -> Vec<(String, u32)> {
    plan.steps
        .iter()
        .filter_map(|step| match step {
            PlanStep::StartHostTenant {
                tenant, replicas, ..
            } => Some((tenant.clone(), *replicas)),
            _ => None,
        })
        .collect()
}

#[test]
fn recorded_replica_counts_reach_the_switch_back_and_survive_a_restart() {
    let tree = TempTree::new("daemon-record");
    let mut config = configuration(&tree, 40 * GIB_MIB);
    config.tenants = vec![declared_tenants().remove(0)];
    let running = cdct::daemon::start(config.clone()).expect("the daemon starts");
    let mut client = connect(running.local_socket());
    switch(&mut client, guest("mac"));

    let recorded = vec![DisplacedTenant {
        tenant: "llm/broker".to_owned(),
        ladder: GpuLadder::Interactive,
        replicas: 3,
    }];
    let holder = client
        .record_displaced(&recorded)
        .expect("the local socket may record");
    assert_eq!(holder, "card-mac");
    let back = plan_of(
        &mut client,
        Request::PlanSwitch {
            target: KernelOwner::Host,
        },
    );
    assert_eq!(restore_counts(&back), vec![("llm/broker".to_owned(), 3)]);

    drop(client);
    drop(running);
    // A connection thread may still hold the old daemon's state for a moment
    // after the drop; the lock frees as soon as it sees the closed socket.
    let running = (0..200)
        .find_map(|_| {
            cdct::daemon::start(config.clone()).ok().or_else(|| {
                std::thread::sleep(std::time::Duration::from_millis(10));
                None
            })
        })
        .expect("the daemon restarts");
    let mut client = connect(running.local_socket());
    let back = plan_of(
        &mut client,
        Request::PlanSwitch {
            target: KernelOwner::Host,
        },
    );
    assert_eq!(restore_counts(&back), vec![("llm/broker".to_owned(), 3)]);
}

#[test]
fn a_guest_channel_cannot_record_displaced_tenants() {
    let tree = TempTree::new("daemon-record-guest");
    let running = start(&tree, 40 * GIB_MIB);
    let mut local = connect(running.local_socket());
    switch(&mut local, guest("mac"));
    let path = running
        .guest_socket(&InstanceId::from("mac"))
        .expect("the guest channel is bound")
        .to_path_buf();
    let mut guest_client = connect(&path);
    let error = guest_client
        .record_displaced(&[])
        .expect_err("the guest channel is refused");
    assert!(error.to_string().contains("local socket"), "{error}");
}

#[test]
fn recording_needs_a_guest_that_holds_the_card() {
    let tree = TempTree::new("daemon-record-host");
    let running = start(&tree, 40 * GIB_MIB);
    let mut client = connect(running.local_socket());
    let error = client
        .record_displaced(&[])
        .expect_err("the host owns the card");
    assert!(error.to_string().contains("host kernel"), "{error}");
}

fn ladder_observation(used_gib: u64, starved: bool) -> cdct::PressureObservation {
    let gib = 1_u64 << 30;
    cdct::PressureObservation {
        vram: Some(cdct::VramFacts {
            used_bytes: used_gib * gib,
            total_bytes: 16 * gib,
        }),
        tenants: vec![
            cdct::GpuTenant {
                id: "llm/broker".to_owned(),
                ladder: GpuLadder::Interactive,
                state: if starved {
                    cdct::TenantState::NotReady
                } else {
                    cdct::TenantState::Running
                },
            },
            cdct::GpuTenant {
                id: "render/farm".to_owned(),
                ladder: GpuLadder::BestEffort,
                state: if starved {
                    cdct::TenantState::Running
                } else {
                    cdct::TenantState::Stopped
                },
            },
        ],
        signals: if starved {
            vec![cdct::StarvationSignal::PodNotReady {
                tenant: "llm/broker".to_owned(),
                ladder: GpuLadder::Interactive,
                starved_ticks: 2,
            }]
        } else {
            Vec::new()
        },
    }
}

#[test]
fn the_live_ladder_sheds_records_the_count_and_restores_when_the_vram_is_free() {
    let tree = TempTree::new("daemon-ladder");
    let mut config = configuration(&tree, 64 * GIB_MIB);
    // No cooldown, so the restore can follow in the same test.
    config.policy.gpu.cooldown_secs = 0;
    let running = cdct::daemon::start(config).expect("the daemon starts");
    let mut executor = connect(running.local_socket());
    executor.send(&Request::Subscribe).expect("subscribe");
    assert!(matches!(
        executor.receive().expect("the snapshot"),
        Some(Reply::Subscribed { .. })
    ));
    let mut reporter = connect(running.local_socket());

    // 15 of 16 GiB in use and the broker starved: the best-effort tenant goes.
    let cdct::PressureAnswer {
        ladder,
        restore,
        wake_at,
        ..
    } = reporter
        .pressure(&ladder_observation(15, true))
        .expect("the observation is answered");
    assert_eq!(wake_at, None, "without a cooldown nothing has to wait");
    assert!(
        matches!(&ladder, cdct::LadderDecision::ScaleLowestPriorityToZero { tenant, .. } if tenant == "render/farm"),
        "{ladder:?}"
    );
    assert_eq!(restore, None);
    let Some(Reply::Granted { plan }) = executor.receive().expect("the shed plan") else {
        panic!("the executor receives the shed plan first");
    };
    assert_eq!(plan.target, KernelOwner::Host);
    assert_eq!(
        plan.steps,
        vec![PlanStep::StopHostTenant {
            tenant: "render/farm".to_owned(),
            ladder: GpuLadder::BestEffort,
        }]
    );
    // The executor records the live count before the stop.
    reporter
        .record_displaced(&[DisplacedTenant {
            tenant: "render/farm".to_owned(),
            ladder: GpuLadder::BestEffort,
            replicas: 2,
        }])
        .expect("the count is recorded with the shed");
    let table = read_table(tree.path().join("holders.json"))
        .expect("the table reads")
        .expect("the table exists");
    assert_eq!(table.shed.len(), 1);
    assert_eq!(table.shed[0].replicas, 2);

    // The pressure is gone and 4 GiB were freed: 10 of 16 GiB in use leaves room.
    let restore = reporter
        .pressure(&ladder_observation(11, false))
        .expect("the observation is answered")
        .restore;
    assert_eq!(
        restore,
        Some(cdct::RestoreDecision::Restore {
            tenant: "render/farm".to_owned(),
            ladder: GpuLadder::BestEffort,
            replicas: 2,
        })
    );
    let plan = loop {
        match executor.receive().expect("the stream continues") {
            Some(Reply::Granted { plan }) => break plan,
            Some(_) => {}
            None => panic!("the stream ended before the restore plan"),
        }
    };
    assert_eq!(
        plan.steps,
        vec![PlanStep::StartHostTenant {
            tenant: "render/farm".to_owned(),
            ladder: GpuLadder::BestEffort,
            replicas: 2,
        }]
    );
    let table = read_table(tree.path().join("holders.json"))
        .expect("the table reads")
        .expect("the table exists");
    assert!(table.shed.is_empty());
}

#[test]
fn a_shed_tenant_asks_for_the_next_observation_when_the_cooldown_ends() {
    let tree = TempTree::new("daemon-ladder-wake");
    let mut config = configuration(&tree, 64 * GIB_MIB);
    config.policy.gpu.cooldown_secs = 30;
    let running = cdct::daemon::start(config).expect("the daemon starts");
    let mut reporter = connect(running.local_socket());
    let before = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();

    let shed = reporter
        .pressure(&ladder_observation(15, true))
        .expect("the observation is answered");
    assert!(matches!(
        shed.ladder,
        cdct::LadderDecision::ScaleLowestPriorityToZero { .. }
    ));
    let wake_at = u128::from(
        shed.wake_at
            .expect("a shed tenant waits for its restore")
            .as_millis(),
    );
    assert!(
        (before + 29_000..=before + 31_000).contains(&wake_at),
        "the wake-up is the cooldown's end, 30 s after the shed: {} ms",
        wake_at - before
    );

    // The executor records the live count before the stop.
    reporter
        .record_displaced(&[DisplacedTenant {
            tenant: "render/farm".to_owned(),
            ladder: GpuLadder::BestEffort,
            replicas: 2,
        }])
        .expect("the count is recorded with the shed");

    // VRAM is free again, but the cooldown runs: the restore holds, and the
    // conductor asks for the observation at the same deadline.
    let held = reporter
        .pressure(&ladder_observation(11, false))
        .expect("the observation is answered");
    assert!(
        matches!(
            held.restore,
            Some(cdct::RestoreDecision::None {
                reason: cdct::RestoreHold::Cooldown { .. }
            })
        ),
        "{:?}",
        held.restore
    );
    assert_eq!(held.wake_at, shed.wake_at);
}

#[test]
fn a_guest_channel_cannot_report_pressure() {
    let tree = TempTree::new("daemon-ladder-guest");
    let running = start(&tree, 64 * GIB_MIB);
    let channel = running
        .guest_socket(&InstanceId::from("mac"))
        .expect("the guest channel")
        .to_path_buf();
    let mut guest_side = connect(&channel);
    assert!(guest_side.pressure(&ladder_observation(15, true)).is_err());
}
