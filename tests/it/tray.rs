//! The tray's menu model, without a display.

use cdct::protocol::{Availability, Status, TargetStatus};
use cdct::tray;
use cdct::{KernelOwner, MemoryRefusal, SwitchRefusal};
use cdrv::InstanceId;

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

fn status(owner: KernelOwner, owner_label: &str, targets: Vec<TargetStatus>) -> Status {
    Status {
        owner,
        owner_label: owner_label.to_owned(),
        seq: 3,
        holders: Vec::new(),
        targets,
    }
}

fn short_of_ram() -> Availability {
    Availability::Wait {
        reason: MemoryRefusal::NotEnoughFree {
            available_mib: 10 * 1024,
            required_mib: 24 * 1024,
            gap_mib: 14 * 1024,
            swap_free_mib: 0,
            max_swap_gap_mib: 8 * 1024,
        },
    }
}

#[test]
fn the_menu_offers_the_other_kernels_and_greys_out_what_waits() {
    let status = status(
        KernelOwner::Host,
        "Linux",
        vec![
            target(
                "linux",
                "Back to Linux",
                KernelOwner::Host,
                Availability::AlreadyHeld,
            ),
            target("macos", "Boot macOS", guest("mac"), Availability::Ready),
            target(
                "windows",
                "Boot Windows",
                guest("windows-desktop"),
                short_of_ram(),
            ),
        ],
    );
    let model = tray::menu(&status);
    assert_eq!(model.title, "GPU: Linux");
    assert_eq!(
        model
            .entries
            .iter()
            .map(|entry| entry.label.as_str())
            .collect::<Vec<&str>>(),
        vec!["Boot macOS", "Boot Windows"],
        "the kernel that holds the card is not an entry"
    );
    assert!(model.entries[0].enabled);
    assert_eq!(model.entries[0].reason, None);
    assert!(!model.entries[1].enabled, "a wait greys the entry out");
    let reason = model.entries[1]
        .reason
        .as_deref()
        .expect("a greyed entry carries its reason");
    assert!(reason.contains("not enough free memory"), "{reason}");
    assert!(reason.contains("nothing is displaced"), "{reason}");
}

#[test]
fn inside_a_guest_the_menu_offers_the_way_back_and_the_other_guest() {
    let status = status(
        guest("mac"),
        "macOS",
        vec![
            target(
                "linux",
                "Back to Linux",
                KernelOwner::Host,
                Availability::Ready,
            ),
            target(
                "macos",
                "Boot macOS",
                guest("mac"),
                Availability::AlreadyHeld,
            ),
            target(
                "windows",
                "Boot Windows",
                guest("windows-desktop"),
                Availability::Ready,
            ),
        ],
    );
    let model = tray::menu(&status);
    assert_eq!(model.title, "GPU: macOS");
    assert_eq!(
        model
            .entries
            .iter()
            .map(|entry| (entry.label.as_str(), entry.kernel.clone()))
            .collect::<Vec<(&str, KernelOwner)>>(),
        vec![
            ("Back to Linux", KernelOwner::Host),
            ("Boot Windows", guest("windows-desktop")),
        ]
    );
    assert!(model.entries.iter().all(|entry| entry.enabled));
}

#[test]
fn a_declined_target_is_greyed_out_with_its_reason() {
    let status = status(
        KernelOwner::Host,
        "Linux",
        vec![target(
            "windows",
            "Boot Windows",
            guest("windows-desktop"),
            Availability::Decline {
                reason: SwitchRefusal::NotADesktopInstance {
                    instance: InstanceId::from("windows-desktop"),
                    class: cdrv::InstanceClass::Dev,
                },
            },
        )],
    );
    let model = tray::menu(&status);
    assert_eq!(model.entries.len(), 1);
    assert!(!model.entries[0].enabled);
    let reason = model.entries[0].reason.as_deref().expect("a reason");
    assert!(reason.contains("not a desktop one"), "{reason}");
}

#[test]
fn a_host_without_targets_offers_no_entries() {
    let model = tray::menu(&status(KernelOwner::Host, "Linux", Vec::new()));
    assert_eq!(model.title, "GPU: Linux");
    assert!(model.entries.is_empty());
}
