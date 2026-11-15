//! The wire protocol: every message is one JSON line and round-trips.

use cdct::protocol::{
    self, Availability, Channel, HolderSummary, Reply, Request, Status, TargetStatus,
};
use cdct::{GpuLadder, HolderState, KernelOwner, MemoryRefusal, SwitchOutcome, SwitchRefusal};
use cdrv::{EventLog, InstanceClass, InstanceId, Source, Timestamp};

use super::guest;

fn target(
    name: &str,
    label: &str,
    kernel: KernelOwner,
    availability: Availability,
) -> TargetStatus {
    TargetStatus {
        name: name.to_owned(),
        label: label.to_owned(),
        kernel,
        availability,
    }
}

fn status_fixture() -> Status {
    Status {
        owner: KernelOwner::Host,
        owner_label: "Linux".to_owned(),
        seq: 7,
        holders: vec![HolderSummary {
            id: "card-mac".to_owned(),
            instance: Some(InstanceId::from("mac")),
            tenant: None,
            class: InstanceClass::Desktop,
            priority: GpuLadder::Desktop.priority(),
            memory_mib: 24 * 1024,
            devices: vec!["example-gpu".to_owned()],
            state: HolderState::Granted,
        }],
        targets: vec![
            target(
                "linux",
                "Back to Linux",
                KernelOwner::Host,
                Availability::AlreadyHeld,
            ),
            target(
                "macos",
                "Boot macOS",
                guest("mac"),
                Availability::Wait {
                    reason: MemoryRefusal::NotEnoughFree {
                        available_mib: 10 * 1024,
                        required_mib: 24 * 1024,
                        gap_mib: 14 * 1024,
                        swap_free_mib: 0,
                        max_swap_gap_mib: 8 * 1024,
                    },
                },
            ),
            target(
                "windows",
                "Boot Windows",
                guest("windows-desktop"),
                Availability::Decline {
                    reason: SwitchRefusal::UnknownTarget {
                        instance: InstanceId::from("windows-desktop"),
                    },
                },
            ),
        ],
    }
}

#[test]
fn every_request_round_trips() {
    let requests = [
        Request::Status,
        Request::Subscribe,
        Request::PlanSwitch {
            target: guest("mac"),
        },
        Request::Switch {
            target: KernelOwner::Host,
        },
    ];
    for request in requests {
        let line = protocol::encode(&request).expect("the request encodes");
        assert!(!line.contains('\n'), "one message is one line");
        assert_eq!(
            protocol::decode::<Request>(&line).expect("the request decodes"),
            request
        );
    }
}

#[test]
fn the_request_tags_are_stable() {
    assert_eq!(
        protocol::encode(&Request::Status).expect("encode"),
        r#"{"request":"status"}"#
    );
    assert_eq!(
        protocol::encode(&Request::Subscribe).expect("encode"),
        r#"{"request":"subscribe"}"#
    );
    assert_eq!(
        protocol::encode(&Request::Switch {
            target: KernelOwner::Host
        })
        .expect("encode"),
        r#"{"request":"switch","target":"host"}"#
    );
    assert_eq!(
        protocol::encode(&Request::PlanSwitch {
            target: guest("mac")
        })
        .expect("encode"),
        r#"{"request":"plan_switch","target":{"guest":"mac"}}"#
    );
}

#[test]
fn every_reply_round_trips() {
    let status = status_fixture();
    let mut log = EventLog::new("synthetic-host", Source::Cdct, 1);
    let event = log.emit(
        Timestamp::from_secs(5),
        "admission.granted",
        cdrv::Subject::Holder(cdrv::HolderId::from("card-mac")),
        cdrv::Severity::Info,
        serde_json::json!({"memory_mib": 24 * 1024}),
    );
    let outcome = SwitchOutcome::Wait {
        reason: MemoryRefusal::NotEnoughFree {
            available_mib: 10 * 1024,
            required_mib: 24 * 1024,
            gap_mib: 14 * 1024,
            swap_free_mib: 0,
            max_swap_gap_mib: 8 * 1024,
        },
    };
    let replies = [
        Reply::Status {
            status: status.clone(),
        },
        Reply::Subscribed {
            channel: Channel::Guest {
                instance: InstanceId::from("mac"),
            },
            status,
        },
        Reply::Plan {
            outcome: outcome.clone(),
        },
        Reply::Switch { outcome },
        Reply::Event {
            event: event.clone(),
        },
        Reply::Error {
            message: "no".to_owned(),
        },
    ];
    for reply in replies {
        let line = protocol::encode(&reply).expect("the reply encodes");
        assert_eq!(
            protocol::decode::<Reply>(&line).expect("the reply decodes"),
            reply
        );
    }
}

#[test]
fn a_malformed_line_is_an_error_not_a_panic() {
    assert!(protocol::decode::<Request>("").is_err());
    assert!(protocol::decode::<Request>("not json").is_err());
    assert!(protocol::decode::<Request>(r#"{"request":"unknown"}"#).is_err());
}

#[test]
fn a_granted_plan_is_tagged_granted_on_the_wire() {
    let plan = cdct::SwitchPlan {
        target: KernelOwner::Host,
        memory: None,
        displaced: Vec::new(),
        steps: Vec::new(),
    };
    let reply = Reply::Granted { plan };
    let line = protocol::encode(&reply).expect("the reply encodes");
    assert!(line.starts_with(r#"{"reply":"granted","plan":{"#), "{line}");
    assert_eq!(
        protocol::decode::<Reply>(&line).expect("the reply decodes"),
        reply
    );
}

#[test]
fn record_displaced_and_recorded_use_stable_tags() {
    let request = Request::RecordDisplaced {
        displaced: vec![cdct::DisplacedTenant {
            tenant: "a".to_owned(),
            ladder: GpuLadder::Desktop,
            replicas: 2,
        }],
    };
    assert_eq!(
        protocol::encode(&request).expect("the request encodes"),
        r#"{"request":"record_displaced","displaced":[{"tenant":"a","ladder":"gpu-desktop","replicas":2}]}"#
    );
    let reply = Reply::Recorded {
        holder: "card-mac".to_owned(),
    };
    assert_eq!(
        protocol::encode(&reply).expect("the reply encodes"),
        r#"{"reply":"recorded","holder":"card-mac"}"#
    );
}
