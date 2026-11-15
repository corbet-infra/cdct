//! The configuration: defaults, names and the holder labels.

use cdct::config::{Config, TargetConfig};
use cdrv::{InstanceId, KernelOwner};

use super::{TempTree, guest};

fn configured() -> Config {
    Config::from_json(
        r#"{
            "socket": "/run/example/control.sock",
            "targets": [
                {"name": "linux", "label": "Back to Linux", "holder": "Linux", "kernel": "host"},
                {"name": "macos", "label": "Boot macOS", "holder": "macOS", "kernel": {"guest": "mac"}},
                {"name": "windows", "label": "Boot Windows", "kernel": {"guest": "windows-desktop"}}
            ]
        }"#,
    )
    .expect("the synthetic configuration parses")
}

#[test]
fn an_empty_document_is_the_default_configuration() {
    let config = Config::from_json("{}").expect("the empty document parses");
    assert_eq!(config, Config::default());
    assert_eq!(
        config.socket,
        std::path::PathBuf::from("/run/cdct/control.sock")
    );
    assert_eq!(
        config.state,
        std::path::PathBuf::from("/var/lib/cdct/holders.json")
    );
    assert_eq!(config.gpu_device, "gpu");
    assert!(config.instances.is_empty());
    assert!(config.targets.is_empty());
}

#[test]
fn a_partial_document_keeps_every_unnamed_default() {
    let config = Config::from_json(r#"{"socket": "/run/example/control.sock"}"#)
        .expect("the partial document parses");
    assert_eq!(
        config.socket,
        std::path::PathBuf::from("/run/example/control.sock")
    );
    assert_eq!(config.host, "localhost");
    assert_eq!(config.socket_mode, 0o660);
    assert!(config.policy.memory.swap.enabled);
}

#[test]
fn configured_names_resolve_to_their_kernels() {
    let config = configured();
    assert_eq!(config.resolve("linux"), Some(KernelOwner::Host));
    assert_eq!(config.resolve("macos"), Some(guest("mac")));
    assert_eq!(
        config.resolve("windows"),
        Some(KernelOwner::Guest(InstanceId::from("windows-desktop")))
    );
    assert_eq!(config.resolve("host"), Some(KernelOwner::Host));
    assert_eq!(config.resolve("guest:other"), Some(guest("other")));
    assert_eq!(config.resolve("guest:"), None);
    assert_eq!(config.resolve("amiga"), None);
    assert_eq!(config.target_names(), vec!["linux", "macos", "windows"]);
}

#[test]
fn the_owner_label_prefers_the_configured_holder_name() {
    let config = configured();
    assert_eq!(config.owner_label(&KernelOwner::Host), "Linux");
    assert_eq!(config.owner_label(&guest("mac")), "macOS");
    // No holder label configured: the name is the label.
    assert_eq!(config.owner_label(&guest("windows-desktop")), "windows");
    // No target at all: the kernel's own spelling.
    assert_eq!(
        config.owner_label(&guest("unknown")),
        "guest:unknown".to_owned()
    );
}

#[test]
fn a_target_without_a_holder_label_falls_back_to_its_name() {
    let target = TargetConfig {
        name: "macos".to_owned(),
        label: "Boot macOS".to_owned(),
        holder: String::new(),
        kernel: guest("mac"),
    };
    assert_eq!(target.holder_label(), "macos");
}

#[test]
fn a_missing_configuration_file_is_an_error() {
    let tree = TempTree::new("config-missing");
    assert!(Config::load(tree.path().join("absent.json")).is_err());
}
