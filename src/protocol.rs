//! The JSON-lines protocol between the daemon, the CLI and the tray.
//!
//! One message per line: a client sends a [`Request`], the daemon answers a
//! single question with one [`Reply`], or, after [`Request::Subscribe`], with a
//! [`Reply::Subscribed`] snapshot followed by pushed replies. Nothing here
//! polls: a subscriber receives every state change as it happens.
//!
//! The transport is abstract: the same lines run over the host's local Unix
//! socket and over a guest's virtio-serial channel. The channel a request
//! arrived on identifies the guest, so no credential travels with a request.

use cdrv::{Event, InstanceClass, InstanceId, KernelOwner};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::gpu::{DisplacedTenant, LadderDecision};
use crate::holders::HolderState;
use crate::ladder::{PressureObservation, RestoreDecision};
use crate::memory::MemoryRefusal;
use crate::plan::{SwitchOutcome, SwitchPlan, SwitchRefusal};

/// A request from a client.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "request", rename_all = "snake_case")]
pub enum Request {
    /// Show who holds the card and what each configured target would do now.
    Status,
    /// Decide a switch without changing any state.
    PlanSwitch {
        /// The kernel to plan for.
        target: KernelOwner,
    },
    /// Decide a switch, record its grant, and return the plan for the runtime.
    Switch {
        /// The kernel to switch to.
        target: KernelOwner,
    },
    /// Receive the current snapshot and every later state change.
    ///
    /// The first reply is [`Reply::Subscribed`]; afterwards the daemon pushes
    /// one [`Reply::Granted`] per granted switch, one [`Reply::Event`] per change and a fresh [`Reply::Status`] after the
    /// event that changed the state.
    Subscribe,
    /// Store the live replica counts of the displaced tenants with the card's
    /// holder. Only the local socket may send it: the executor reads the real
    /// counts before it stops the tenants. The daemon is the only writer of the
    /// holder table, so the executor never opens it.
    RecordDisplaced {
        /// The displaced tenants, with their real replica counts.
        displaced: Vec<DisplacedTenant>,
    },
    /// The executor ran a granted plan to its end. Only the local socket may
    /// send it. A switch back to the host releases the guest's holder (and the
    /// displaced list it carries) only now: until then the guest still owns
    /// the card, so a failed return can be asked for again.
    Completed {
        /// The kernel the completed plan gave the card to.
        target: KernelOwner,
    },
    /// The executor observed the card at an event (a GPU pod, the node or the
    /// model broker changed). The daemon runs the live ladder on it and, for a
    /// shed or a restore, pushes the plan as [`Reply::Granted`]. Only the
    /// local socket may send it: a guest never reports the host's card.
    Pressure {
        /// What the executor observed.
        observation: PressureObservation,
    },
}

/// Where a connection came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "channel", rename_all = "snake_case")]
pub enum Channel {
    /// The host's local control socket.
    Local,
    /// A guest's virtio-serial channel. The channel itself identifies the
    /// instance; the client needs no credential.
    Guest {
        /// The instance whose channel this is.
        instance: InstanceId,
    },
}

/// What a configured target would do if it were asked for now.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "availability", rename_all = "snake_case")]
pub enum Availability {
    /// The switch would be planned immediately.
    Ready,
    /// The target already owns the card.
    AlreadyHeld,
    /// The RAM would make it wait; nothing is displaced.
    Wait {
        /// Why it waits.
        reason: MemoryRefusal,
    },
    /// The switch cannot proceed as asked.
    Decline {
        /// Why it is declined.
        reason: SwitchRefusal,
    },
}

impl Availability {
    /// Reduce a switch outcome to the availability of its target.
    #[must_use]
    pub fn from_outcome(outcome: SwitchOutcome) -> Self {
        match outcome {
            SwitchOutcome::Plan(_) => Self::Ready,
            SwitchOutcome::AlreadyHeld { .. } => Self::AlreadyHeld,
            SwitchOutcome::Wait { reason } => Self::Wait { reason },
            SwitchOutcome::Decline { reason } => Self::Decline { reason },
        }
    }
}

/// One admitted holder, as status reports it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HolderSummary {
    /// The holder's identifier.
    pub id: String,
    /// The instance, for a guest holder.
    pub instance: Option<InstanceId>,
    /// The tenant, for a host-side tenant holder.
    pub tenant: Option<String>,
    /// The instance class.
    pub class: InstanceClass,
    /// The admitted priority.
    pub priority: u32,
    /// The admitted memory, in MiB.
    pub memory_mib: u64,
    /// The exclusive devices granted, by hardware-layer name.
    pub devices: Vec<String>,
    /// Its observed state.
    pub state: HolderState,
}

/// One configured kernel and what a switch to it would do now.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TargetStatus {
    /// The configured name (`macos`, `windows`, `linux`, ...).
    pub name: String,
    /// The menu entry ("Boot macOS").
    pub label: String,
    /// The kernel the entry names.
    pub kernel: KernelOwner,
    /// What a switch would do now.
    pub availability: Availability,
}

/// Who holds the card and what every configured target would do.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Status {
    /// The kernel that owns the card.
    pub owner: KernelOwner,
    /// A human label for the owner ("macOS", "Linux", ...).
    pub owner_label: String,
    /// The holder table's last event sequence number.
    pub seq: u64,
    /// Every admitted holder.
    pub holders: Vec<HolderSummary>,
    /// Every configured target, in configuration order.
    pub targets: Vec<TargetStatus>,
}

/// The daemon's answer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case")]
pub enum Reply {
    /// A status snapshot, sent for a single [`Request::Status`] and pushed
    /// after every state change.
    Status {
        /// The snapshot.
        status: Status,
    },
    /// The first answer to [`Request::Subscribe`]: the snapshot plus the
    /// channel the daemon identified.
    Subscribed {
        /// The channel this connection is.
        channel: Channel,
        /// The snapshot at subscription time.
        status: Status,
    },
    /// The decision for [`Request::PlanSwitch`]; nothing changed.
    Plan {
        /// The decision.
        outcome: SwitchOutcome,
    },
    /// The decision for [`Request::Switch`]; a plan means the grant was
    /// recorded and the table changed.
    Switch {
        /// The decision.
        outcome: SwitchOutcome,
    },
    /// A granted plan, pushed to every subscriber before the events and the
    /// status of the change. An executor runs exactly the plans it receives
    /// this way; the tray ignores it.
    Granted {
        /// The plan the switcher received.
        plan: SwitchPlan,
    },
    /// The answer to [`Request::RecordDisplaced`].
    Recorded {
        /// The identifier of the holder that now carries the list.
        holder: String,
    },
    /// The answer to [`Request::Completed`].
    Completed {
        /// Who owns the card now.
        owner: KernelOwner,
    },
    /// The answer to [`Request::Pressure`]: what the ladder decided.
    Pressure {
        /// The shed decision.
        ladder: LadderDecision,
        /// The restore decision, taken when nothing is shed.
        restore: Option<RestoreDecision>,
        /// Whether the device plugin is restarted (a lost registration).
        #[serde(default)]
        restart_plugin: bool,
        /// When the conductor wants the next observation without an event.
        #[serde(default)]
        wake_at: Option<cdrv::Timestamp>,
    },
    /// One pushed state change.
    Event {
        /// The event, in the contract's envelope.
        event: Event,
    },
    /// The request could not be served.
    Error {
        /// A plain explanation.
        message: String,
    },
}

/// Encode one message as a single JSON line, without the newline.
pub fn encode<T: Serialize>(message: &T) -> Result<String, serde_json::Error> {
    serde_json::to_string(message)
}

/// Decode one JSON line.
pub fn decode<T: DeserializeOwned>(line: &str) -> Result<T, serde_json::Error> {
    serde_json::from_str(line)
}
