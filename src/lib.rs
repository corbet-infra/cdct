//! The conductor (`cdct`): one decision engine for RAM and the exclusive GPU.
//!
//! The conductor decides; it never executes. It opens no device, starts no
//! guest, binds no driver and runs no host tool. Its inputs are facts (parsed
//! text and observed state), its output is a decision or a typed plan, and its
//! only persistence is the holder table behind an exclusive file lock.
//!
//! The pieces:
//! - [`facts`] parses `/proc/meminfo`, ZFS `arcstats` and memory PSI from text;
//! - [`reader`] is the thin module that reads those files;
//! - [`memory`] is the ported guard and RAM admission;
//! - [`gpu`] is the one-kernel model, the platform ladder and the four
//!   starvation signals;
//! - [`plan`] turns a switch into an ordered list of steps;
//! - [`holders`] persists who holds what under one writer's lock;
//! - [`reconcile`] rebuilds the table from observed facts after a crash;
//! - [`policy`] holds every threshold as data with a sensible default;
//! - [`protocol`] is the JSON-lines protocol the daemon and its clients speak;
//! - [`daemon`] is `cdct serve`: the one writer and its listeners;
//! - [`client`] connects the CLI and the tray, on the host or inside a guest;
//! - [`config`] names the instances and the kernels the CLI and tray offer;
//! - [`tray`] is the tray's menu model as pure data;
//! - [`describe`] turns decisions into the lines an operator reads;
//! - [`engine`] is the [`Conductor`] that fronts all of the above.
//!
//! ```
//! use cdct::{Conductor, Policy};
//!
//! let conductor = Conductor::new(Policy::default());
//! assert_eq!(conductor.policy().memory.gpu_vm_claim_mib, 24 * 1024);
//! ```
#![forbid(unsafe_code)]

pub mod client;
pub mod config;
#[cfg(unix)]
pub mod daemon;
pub mod describe;
pub mod engine;
pub mod facts;
pub mod gpu;
pub mod holders;
pub mod ladder;
pub mod memory;
pub mod plan;
pub mod policy;
pub mod protocol;
pub mod reader;
pub mod reconcile;
pub mod tray;

pub use client::{Client, ClientError, Endpoint, Stream};
pub use config::{Config, ConfigError, InstanceConfig, TargetConfig, TenantConfig};
pub use describe::{availability, event, memory_refusal, outcome, status, switch_refusal};
pub use engine::{Conductor, FactsError, FactsFile};
pub use facts::{
    ArcStats, MemInfo, MemorySnapshot, ParseError, Pressure, PressureShare, RawFacts,
    parse_arcstats, parse_meminfo, parse_pressure,
};
pub use gpu::{
    DisplacedTenant, GpuLadder, GpuState, GpuTenant, InstanceObservation, KernelOwner,
    LadderDecision, LadderHold, LadderInput, StarvationSignal, TenantState, VramFacts,
};
pub use holders::{Holder, HolderState, HolderStore, HolderTable, StoreError};
pub use ladder::{
    PressureObservation, RestoreDecision, RestoreHold, ShedTenant, decide_guard, decide_restore,
    measure_freed,
};
pub use memory::{
    MemoryAdmission, MemoryDecision, MemoryHeadroom, MemoryRefusal, decide_memory,
    decide_memory_with_credit, pressure_allows_swap,
};
pub use plan::{
    PlanStep, SwitchOutcome, SwitchPlan, SwitchRefusal, guard_plan, plan_switch, restore_plan,
    shed_plan,
};
pub use policy::{GpuPolicy, MemoryPolicy, Policy, PolicyError, SwapPolicy};
pub use protocol::{
    Availability, Channel, HolderSummary, Reply, Request, Status as DaemonStatus, TargetStatus,
};
pub use reader::ProcReader;
pub use reconcile::{
    ObservedDeviceDriver, ObservedHost, ObservedInstance, ReconcileReason, ReconcileReport, Repair,
    reconcile,
};
pub use tray::{MenuEntry, MenuModel};
