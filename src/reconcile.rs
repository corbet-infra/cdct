//! Crash recovery: rebuild the holder table from observed facts.
//!
//! A reconcile pass runs only after a crash, never as steady-state polling. It
//! repairs what it can prove is stale, reports what it cannot, and never
//! silently displaces a live holder that has no recorded lease.

use cdrv::{Event, EventLog, HolderId, InstanceId, InstanceState, Severity, Subject, Timestamp};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::gpu::KernelOwner;
use crate::holders::HolderTable;

/// One instance as the driver observed it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservedInstance {
    /// The instance.
    pub id: InstanceId,
    /// Its observed state.
    pub state: InstanceState,
}

/// What driver the card is bound to right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservedDeviceDriver {
    /// The normal host driver.
    Host,
    /// The passthrough driver.
    Passthrough,
}

/// Everything a reconcile pass observes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservedHost {
    /// Every instance the driver owns.
    pub instances: Vec<ObservedInstance>,
    /// The card's hardware-layer name.
    pub device: String,
    /// The driver the card is bound to.
    pub device_driver: ObservedDeviceDriver,
}

/// One repair the conductor asks the runtime or driver to perform.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Repair {
    /// The holder's instance is gone: release the holder.
    ReleaseHolder {
        /// The stale holder.
        holder: HolderId,
        /// The instance it recorded.
        instance: InstanceId,
        /// Why it is released.
        reason: ReconcileReason,
    },
    /// A live instance has no holder. Report it; do not invent a lease and do
    /// not displace it.
    ReportOrphan {
        /// The unrecorded instance.
        instance: InstanceId,
        /// Why it is reported.
        reason: ReconcileReason,
    },
    /// The card is bound to the passthrough driver with no guest holder:
    /// return it to the host kernel.
    ReturnCardToHost {
        /// The card.
        device: String,
        /// Why it is returned.
        reason: ReconcileReason,
    },
}

/// Why a repair was needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReconcileReason {
    /// The recorded instance is no longer observed.
    InstanceGone,
    /// A live instance has no holder row.
    NoHolder,
    /// The passthrough driver is bound while no holder owns the card.
    BoundWithoutHolder,
}

/// The rebuilt table, the repairs, and the events the pass emitted.
#[derive(Debug, Clone, PartialEq)]
pub struct ReconcileReport {
    /// The repaired table.
    pub table: HolderTable,
    /// What the runtime or driver must do.
    pub repairs: Vec<Repair>,
    /// One event per repair or report.
    pub events: Vec<Event>,
}

/// Rebuild the holder table against observed reality.
///
/// Rules:
/// 1. a holder whose instance is no longer live is released (`instance_gone`);
/// 2. a live instance with no holder is reported, never adopted or displaced;
/// 3. the passthrough driver bound while no guest owns the card is returned to
///    the host (`bound_without_holder`).
#[must_use]
pub fn reconcile(
    mut table: HolderTable,
    observed: &ObservedHost,
    log: &mut EventLog,
    at: Timestamp,
) -> ReconcileReport {
    let mut repairs = Vec::new();
    let mut events = Vec::new();
    let live: Vec<&InstanceId> = observed
        .instances
        .iter()
        .filter(|instance| instance.state.is_live())
        .map(|instance| &instance.id)
        .collect();

    let stale: Vec<(HolderId, InstanceId)> = table
        .holders
        .iter()
        .filter_map(|holder| {
            let instance = holder.instance.as_ref()?;
            if live.contains(&instance) {
                None
            } else {
                Some((holder.id.clone(), instance.clone()))
            }
        })
        .collect();
    for (holder, instance) in stale {
        if let Some(event) = table.release_reconciled(&holder, log, at, "instance_gone") {
            events.push(event);
        }
        repairs.push(Repair::ReleaseHolder {
            holder,
            instance,
            reason: ReconcileReason::InstanceGone,
        });
    }

    for instance in &live {
        let recorded = table
            .holders
            .iter()
            .any(|holder| holder.instance.as_ref() == Some(instance));
        if !recorded {
            let event = log.emit(
                at,
                "holder.reconciled",
                Subject::Instance((*instance).clone()),
                Severity::Warn,
                json!({"reason": "no_holder"}),
            );
            table.seq = event.seq;
            events.push(event);
            repairs.push(Repair::ReportOrphan {
                instance: (*instance).clone(),
                reason: ReconcileReason::NoHolder,
            });
        }
    }

    if observed.device_driver == ObservedDeviceDriver::Passthrough
        && table.gpu_owner == KernelOwner::Host
    {
        let event = log.emit(
            at,
            "holder.reconciled",
            Subject::Device(observed.device.clone()),
            Severity::Warn,
            json!({"reason": "bound_to_vfio_without_holder"}),
        );
        table.seq = event.seq;
        events.push(event);
        repairs.push(Repair::ReturnCardToHost {
            device: observed.device.clone(),
            reason: ReconcileReason::BoundWithoutHolder,
        });
    }

    ReconcileReport {
        table,
        repairs,
        events,
    }
}
