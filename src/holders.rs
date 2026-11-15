//! The holder table: who holds what, persisted by one writer per host.
//!
//! The table is JSON on disk behind an exclusive file lock. Every change emits
//! an [`cdrv::Event`] through the caller's log, so the tiles and clients learn
//! of it without polling. The GPU owner is part of the table: a holder that has
//! an instance and an exclusive device is the card's custodian, and releasing
//! the last such holder returns the card to the host kernel.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use cdrv::{
    Event, EventLog, HolderId, InstanceClass, InstanceId, LeaseId, Severity, Subject, Timestamp,
};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::json;
use thiserror::Error;

use crate::gpu::{DisplacedTenant, KernelOwner};

/// What a holder is allowed to do with what it was granted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HolderState {
    /// Admitted but not observed running yet.
    Granted,
    /// Observed running.
    Running,
    /// Leaving; the card or memory is not reusable until it is gone.
    Releasing,
}

/// One row of the holder table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Holder {
    /// The holder's identifier.
    pub id: HolderId,
    /// The lease this holder belongs to.
    pub lease: LeaseId,
    /// The instance, for a guest holder.
    pub instance: Option<InstanceId>,
    /// The tenant name, for a host-side GPU tenant holder.
    pub tenant: Option<String>,
    /// The instance class.
    pub class: InstanceClass,
    /// The admitted priority.
    pub priority: u32,
    /// The admitted memory, in MiB.
    pub memory_mib: u64,
    /// The admitted CPU quota, in cores (a limit, never an admission gate).
    pub cpu_cores: f32,
    /// The exclusive devices granted, by hardware-layer name.
    pub devices: Vec<String>,
    /// The host tenants the switch that granted this holder displaced. A later
    /// switch back to the host restores exactly this set.
    #[serde(default)]
    pub displaced: Vec<DisplacedTenant>,
    /// Its observed state.
    pub state: HolderState,
    /// When it was admitted.
    pub since: Timestamp,
    /// The sequence number of the event that last changed it.
    pub last_event_seq: u64,
}

impl Holder {
    /// Whether this holder is the card's custodian: it has an instance and at
    /// least one exclusive device (the GPU is the only one today).
    #[must_use]
    pub fn is_card_custodian(&self) -> bool {
        self.instance.is_some() && !self.devices.is_empty()
    }
}

/// The persisted holder table of one host.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HolderTable {
    /// The last event sequence number written for this host.
    pub seq: u64,
    /// The kernel that owns the card; derived from the custodian holder.
    pub gpu_owner: KernelOwner,
    /// Every holder.
    pub holders: Vec<Holder>,
}

impl Default for HolderTable {
    fn default() -> Self {
        Self {
            seq: 0,
            gpu_owner: KernelOwner::Host,
            holders: Vec::new(),
        }
    }
}

impl HolderTable {
    /// An empty table with no holders and the card at the host kernel.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The holder that currently owns the card, when a guest does.
    #[must_use]
    pub fn card_custodian(&self) -> Option<&Holder> {
        self.holders
            .iter()
            .find(|holder| holder.is_card_custodian())
    }

    /// The host tenants the card's holder displaced, empty when the host holds
    /// the card or no switch recorded any.
    #[must_use]
    pub fn displaced_by_card_custodian(&self) -> &[DisplacedTenant] {
        self.card_custodian()
            .map_or(&[], |holder| holder.displaced.as_slice())
    }

    /// Admit one holder. A holder that takes an exclusive device and has an
    /// instance becomes the card's custodian and moves ownership to that guest.
    ///
    /// Emits `admission.granted`.
    pub fn admit(&mut self, mut holder: Holder, log: &mut EventLog, at: Timestamp) -> Event {
        holder.since = at;
        let event = log.emit(
            at,
            "admission.granted",
            Subject::Holder(holder.id.clone()),
            Severity::Info,
            json!({
                "instance": holder.instance,
                "tenant": holder.tenant,
                "class": holder.class,
                "priority": holder.priority,
                "memory_mib": holder.memory_mib,
                "devices": holder.devices,
                "displaced": holder.displaced,
            }),
        );
        holder.last_event_seq = event.seq;
        if let Some(instance) = &holder.instance
            && holder.is_card_custodian()
        {
            self.gpu_owner = KernelOwner::Guest(instance.clone());
        }
        self.holders.push(holder);
        self.seq = event.seq;
        event
    }

    /// Release one holder. Releasing the custodian returns the card to the
    /// host kernel.
    ///
    /// Emits `holder.released`; returns `None` when the holder is unknown.
    pub fn release(
        &mut self,
        id: &HolderId,
        log: &mut EventLog,
        at: Timestamp,
        reason: &str,
    ) -> Option<Event> {
        self.remove(id, log, at, reason, "holder.released")
    }

    /// Release a holder because a reconcile pass proved it stale.
    ///
    /// Emits `holder.reconciled`; returns `None` when the holder is unknown.
    pub fn release_reconciled(
        &mut self,
        id: &HolderId,
        log: &mut EventLog,
        at: Timestamp,
        reason: &str,
    ) -> Option<Event> {
        self.remove(id, log, at, reason, "holder.reconciled")
    }

    fn remove(
        &mut self,
        id: &HolderId,
        log: &mut EventLog,
        at: Timestamp,
        reason: &str,
        kind: &str,
    ) -> Option<Event> {
        let position = self.holders.iter().position(|holder| &holder.id == id)?;
        let holder = self.holders.remove(position);
        if holder.is_card_custodian()
            && let Some(instance) = &holder.instance
            && self.gpu_owner == KernelOwner::Guest(instance.clone())
        {
            self.gpu_owner = KernelOwner::Host;
        }
        let event = log.emit(
            at,
            kind,
            Subject::Holder(holder.id),
            Severity::Info,
            json!({
                "instance": holder.instance,
                "tenant": holder.tenant,
                "reason": reason,
            }),
        );
        self.seq = event.seq;
        Some(event)
    }
}

/// A holder-store failure.
#[derive(Debug, Error)]
pub enum StoreError {
    /// The state file could not be opened, locked or written.
    #[error("holder store {path}: {source}")]
    Io {
        /// The file that failed.
        path: String,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// The state file is not a holder table.
    #[error("holder store {path} is invalid JSON: {source}")]
    Json {
        /// The file that failed.
        path: String,
        /// The underlying JSON error.
        #[source]
        source: serde_json::Error,
    },
}

fn io_error(path: &Path, source: std::io::Error) -> StoreError {
    StoreError::Io {
        path: path.display().to_string(),
        source,
    }
}

/// One writer's handle on the holder table file.
///
/// [`HolderStore::open`] takes the exclusive advisory lock and keeps it until
/// the store is dropped, so two conductors on one host cannot interleave
/// writes.
#[derive(Debug)]
pub struct HolderStore {
    file: File,
    path: PathBuf,
}

impl HolderStore {
    /// Open (creating if needed) and exclusively lock the state file.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent).map_err(|error| io_error(&path, error))?;
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|error| io_error(&path, error))?;
        file.lock_exclusive()
            .map_err(|error| io_error(&path, error))?;
        Ok(Self { file, path })
    }

    /// The state file this store owns.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read the stored table; an empty file is an empty table.
    pub fn load(&mut self) -> Result<HolderTable, StoreError> {
        self.file
            .seek(SeekFrom::Start(0))
            .map_err(|error| io_error(&self.path, error))?;
        let mut text = String::new();
        self.file
            .read_to_string(&mut text)
            .map_err(|error| io_error(&self.path, error))?;
        if text.trim().is_empty() {
            return Ok(HolderTable::new());
        }
        serde_json::from_str(&text).map_err(|source| StoreError::Json {
            path: self.path.display().to_string(),
            source,
        })
    }

    /// Replace the stored table in one truncate-and-write pass.
    pub fn save(&mut self, table: &HolderTable) -> Result<(), StoreError> {
        let text = serde_json::to_vec_pretty(table).map_err(|source| StoreError::Json {
            path: self.path.display().to_string(),
            source,
        })?;
        self.file
            .seek(SeekFrom::Start(0))
            .map_err(|error| io_error(&self.path, error))?;
        self.file
            .set_len(0)
            .map_err(|error| io_error(&self.path, error))?;
        self.file
            .write_all(&text)
            .map_err(|error| io_error(&self.path, error))?;
        self.file
            .flush()
            .map_err(|error| io_error(&self.path, error))?;
        Ok(())
    }
}

/// Read a holder table without taking the writer lock, for `cdct status`.
///
/// A missing file is `None` (no conductor has written yet), never an error.
pub fn read_table(path: impl AsRef<Path>) -> Result<Option<HolderTable>, StoreError> {
    let path = path.as_ref();
    match fs::read_to_string(path) {
        Ok(text) if text.trim().is_empty() => Ok(Some(HolderTable::new())),
        Ok(text) => serde_json::from_str(&text)
            .map(Some)
            .map_err(|source| StoreError::Json {
                path: path.display().to_string(),
                source,
            }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(io_error(path, error)),
    }
}
