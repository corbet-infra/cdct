//! The daemon's configuration: paths, instance facts and the named kernels.
//!
//! One JSON file serves the daemon and its clients. A client resolves the
//! familiar names (`macos`, `windows`, `linux`) through [`Config::resolve`];
//! the daemon uses the same targets to build the tray's entries and the same
//! instances to observe what a switch could start.
//!
//! The file carries deployment data and is therefore never committed. The
//! defaults are safe for a host without a configuration file: no instance, no
//! target and the standard paths.

use std::path::{Path, PathBuf};

use cdrv::{GpuLadder, GuestOs, InstanceClass, InstanceId, KernelOwner};
use serde::{Deserialize, Serialize};

use crate::policy::Policy;

/// Default location of the local control socket.
pub const DEFAULT_SOCKET: &str = "/run/cdct/control.sock";
/// Default location of the holder table.
pub const DEFAULT_STATE: &str = "/var/lib/cdct/holders.json";

/// One configured instance the daemon may observe or start.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InstanceConfig {
    /// The instance identifier shared with `cwld` and the drivers.
    pub id: InstanceId,
    /// Its class.
    pub class: InstanceClass,
    /// Its guest OS.
    pub guest_os: GuestOs,
    /// The install (disk) identity; two profiles on one install never run
    /// together.
    #[serde(default)]
    pub install: Option<String>,
    /// The fixed memory claim, when the class has one.
    #[serde(default)]
    pub memory_mib: Option<u64>,
    /// The memory it holds while stopped or idle, when known.
    #[serde(default)]
    pub resident_mib: Option<u64>,
    /// The host path of this guest's virtio-serial channel. The daemon listens
    /// here and the channel itself identifies the instance.
    #[serde(default)]
    pub channel: Option<PathBuf>,
}

/// One kernel the CLI names and the tray offers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TargetConfig {
    /// The CLI name (`macos`, `windows`, `linux`, ...).
    pub name: String,
    /// The tray entry ("Boot macOS").
    pub label: String,
    /// The label shown when this kernel holds the card ("macOS"). Empty means
    /// the name.
    #[serde(default)]
    pub holder: String,
    /// The kernel the entry names.
    pub kernel: KernelOwner,
}

/// One host tenant of the card, declared when no facts document reports them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TenantConfig {
    /// The tenant's name in the platform (`llm/llama-broker`).
    pub id: String,
    /// Its ladder rung.
    pub ladder: GpuLadder,
}

impl TargetConfig {
    /// The label to show when this target holds the card.
    #[must_use]
    pub fn holder_label(&self) -> &str {
        if self.holder.is_empty() {
            &self.name
        } else {
            &self.holder
        }
    }
}

/// Everything one deployment configures.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// The persisted holder table.
    pub state: PathBuf,
    /// The local control socket the daemon binds (unless socket-activated).
    pub socket: PathBuf,
    /// The host name carried in events.
    pub host: String,
    /// The card's name in the hardware inventory, never an address.
    pub gpu_device: String,
    /// The proc root the memory guard reads (`/proc` on a live host).
    pub proc_root: PathBuf,
    /// An optional facts document (observed tenants and instances), in the
    /// format `cdct plan switch --facts` reads.
    pub facts: Option<PathBuf>,
    /// The mode of every socket the daemon binds itself (0660 = group access).
    pub socket_mode: u32,
    /// The group that owns those sockets, when set.
    pub socket_gid: Option<u32>,
    /// The decision policy.
    pub policy: Policy,
    /// The instances the daemon knows.
    pub instances: Vec<InstanceConfig>,
    /// The kernels the CLI names and the tray offers.
    pub targets: Vec<TargetConfig>,
    /// The host tenants of the card, used when no facts document reports them.
    pub tenants: Vec<TenantConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            state: PathBuf::from(DEFAULT_STATE),
            socket: PathBuf::from(DEFAULT_SOCKET),
            host: "localhost".to_owned(),
            gpu_device: "gpu".to_owned(),
            proc_root: PathBuf::from("/proc"),
            facts: None,
            socket_mode: 0o660,
            socket_gid: None,
            policy: Policy::default(),
            instances: Vec::new(),
            targets: Vec::new(),
            tenants: Vec::new(),
        }
    }
}

impl Config {
    /// Parse a configuration from JSON text. Missing fields keep their
    /// defaults.
    pub fn from_json(text: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(text)
    }

    /// Read a configuration file.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.display().to_string(),
            source,
        })?;
        Self::from_json(&text).map_err(|source| ConfigError::Json {
            path: path.display().to_string(),
            source,
        })
    }

    /// The configured file named by `$CDCT_CONFIG`, when set.
    #[must_use]
    pub fn default_path() -> Option<PathBuf> {
        std::env::var_os("CDCT_CONFIG").map(PathBuf::from)
    }

    /// The instance with this identifier, when configured.
    #[must_use]
    pub fn instance(&self, id: &InstanceId) -> Option<&InstanceConfig> {
        self.instances.iter().find(|instance| &instance.id == id)
    }

    /// Resolve a CLI name to a kernel.
    ///
    /// A configured target name wins; `host` and `guest:<instance>` are always
    /// accepted so an operator can bypass the names.
    #[must_use]
    pub fn resolve(&self, value: &str) -> Option<KernelOwner> {
        if let Some(target) = self.targets.iter().find(|target| target.name == value) {
            return Some(target.kernel.clone());
        }
        if value == "host" {
            return Some(KernelOwner::Host);
        }
        value
            .strip_prefix("guest:")
            .filter(|instance| !instance.is_empty())
            .map(|instance| KernelOwner::Guest(InstanceId::from(instance)))
    }

    /// The configured target names, in configuration order.
    #[must_use]
    pub fn target_names(&self) -> Vec<&str> {
        self.targets
            .iter()
            .map(|target| target.name.as_str())
            .collect()
    }

    /// A human label for the kernel that holds the card.
    #[must_use]
    pub fn owner_label(&self, owner: &KernelOwner) -> String {
        self.targets
            .iter()
            .find(|target| &target.kernel == owner)
            .map_or_else(
                || owner.to_string(),
                |target| target.holder_label().to_owned(),
            )
    }
}

/// A configuration load failure.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The file could not be read.
    #[error("cannot read configuration {path}: {source}")]
    Read {
        /// The file that failed.
        path: String,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// The file is not valid JSON for these types.
    #[error("configuration {path} is invalid: {source}")]
    Json {
        /// The file that failed.
        path: String,
        /// The underlying JSON error.
        #[source]
        source: serde_json::Error,
    },
}
