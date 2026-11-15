//! The conductor: one decision engine, pure logic, no execution.
//!
//! Every decision the system makes comes through [`Conductor`]: RAM admission,
//! the GPU ladder and switch planning, and the post-crash reconcile. It holds
//! policy and nothing else; it opens no device, starts no guest and calls no
//! host tool.

use std::path::Path;

use cdrv::{EventLog, Timestamp};
use serde::{Deserialize, Serialize};

use crate::facts::{MemorySnapshot, ParseError, RawFacts};
use crate::gpu::{GpuState, LadderDecision, LadderInput, decide_ladder};
use crate::holders::HolderTable;
use crate::memory::{MemoryDecision, decide_memory};
use crate::plan::{SwitchOutcome, plan_switch};
use crate::policy::Policy;
use crate::reconcile::{ObservedHost, ReconcileReport, reconcile};

/// The one decision engine.
#[derive(Debug, Clone, PartialEq)]
pub struct Conductor {
    policy: Policy,
}

impl Conductor {
    /// A conductor with the given policy.
    #[must_use]
    pub fn new(policy: Policy) -> Self {
        Self { policy }
    }

    /// A conductor with the default policy.
    #[must_use]
    pub fn with_defaults() -> Self {
        Self::new(Policy::default())
    }

    /// The policy this conductor decides with.
    #[must_use]
    pub fn policy(&self) -> &Policy {
        &self.policy
    }

    /// Decide RAM for a fixed claim.
    #[must_use]
    pub fn memory(&self, snapshot: &MemorySnapshot, claim_mib: u64) -> MemoryDecision {
        decide_memory(snapshot, claim_mib, &self.policy.memory)
    }

    /// Decide the GPU ladder action for one tick.
    #[must_use]
    pub fn ladder(&self, input: &LadderInput) -> LadderDecision {
        decide_ladder(input, &self.policy.gpu)
    }

    /// Decide and plan a switch of the card to `target`.
    ///
    /// The table supplies what an earlier switch displaced, so a switch back to
    /// the host restores exactly that set.
    #[must_use]
    pub fn switch(
        &self,
        target: &crate::gpu::KernelOwner,
        gpu: &GpuState,
        memory: &MemorySnapshot,
        table: &HolderTable,
    ) -> SwitchOutcome {
        plan_switch(
            target,
            gpu,
            memory,
            table.displaced_by_card_custodian(),
            &self.policy,
        )
    }

    /// Rebuild the holder table from observed facts after a crash.
    #[must_use]
    pub fn reconcile(
        &self,
        table: HolderTable,
        observed: &ObservedHost,
        log: &mut EventLog,
        at: Timestamp,
    ) -> ReconcileReport {
        reconcile(table, observed, log, at)
    }
}

/// The JSON document `cdct plan switch --facts` reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FactsFile {
    /// The contents of `/proc/meminfo`.
    pub meminfo: String,
    /// The contents of the ZFS `arcstats` file, when the host has ZFS.
    pub arcstats: Option<String>,
    /// The contents of `/proc/pressure/memory`.
    pub pressure_memory: String,
    /// The observed GPU and instance state.
    #[serde(default)]
    pub gpu: GpuState,
}

/// Why a facts document could not be used.
#[derive(Debug, thiserror::Error)]
pub enum FactsError {
    /// The document could not be read.
    #[error("cannot read facts {path}: {source}")]
    Read {
        /// The file that failed.
        path: String,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// The document is not valid JSON.
    #[error("facts {path} is invalid JSON: {source}")]
    Json {
        /// The file that failed.
        path: String,
        /// The underlying JSON error.
        #[source]
        source: serde_json::Error,
    },
    /// A raw fact inside the document could not be parsed.
    #[error("facts {path}: {source}")]
    Parse {
        /// The file that failed.
        path: String,
        /// The parse error.
        #[source]
        source: ParseError,
    },
}

impl FactsFile {
    /// Read a facts document.
    pub fn read(path: impl AsRef<Path>) -> Result<Self, FactsError> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path).map_err(|source| FactsError::Read {
            path: path.display().to_string(),
            source,
        })?;
        serde_json::from_str(&text).map_err(|source| FactsError::Json {
            path: path.display().to_string(),
            source,
        })
    }

    /// Parse the raw texts into the memory snapshot the conductor decides on.
    pub fn memory_snapshot(&self, path: &Path) -> Result<MemorySnapshot, FactsError> {
        MemorySnapshot::parse(&RawFacts {
            meminfo: self.meminfo.clone(),
            arcstats: self.arcstats.clone(),
            pressure_memory: self.pressure_memory.clone(),
        })
        .map_err(|source| FactsError::Parse {
            path: path.display().to_string(),
            source,
        })
    }
}
