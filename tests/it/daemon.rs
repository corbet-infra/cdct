//! The daemon over a temporary Unix socket: status, switch, subscribe and the
//! guest channel that identifies its instance.

use std::path::{Path, PathBuf};

use cdct::client::{Client, Endpoint};
use cdct::config::{Config, InstanceConfig, TargetConfig};
use cdct::holders::read_table;
use cdct::protocol::{Availability, Channel, Reply, Request, Status};
use cdct::{KernelOwner, MemoryRefusal, Policy, SwitchOutcome};
use cdrv::{GuestOs, InstanceClass, InstanceId};

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
