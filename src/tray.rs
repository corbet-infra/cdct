//! The tray's menu model, as pure data.
//!
//! The GUI binary (`cdct-tray`) only renders this. Every rule lives here, so
//! the menu can be tested without a display: the entry for the kernel that
//! already holds the card is omitted, every other configured kernel is offered
//! with its label, and an entry the RAM would make wait or decline is disabled
//! and carries the plain reason it is greyed out.

use crate::describe;
use crate::protocol::{Availability, Status};

/// One menu entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuEntry {
    /// The entry's text ("Boot macOS").
    pub label: String,
    /// The kernel the entry switches to.
    pub kernel: cdrv::KernelOwner,
    /// Whether the entry can be chosen now.
    pub enabled: bool,
    /// Why the entry is greyed out, when it is.
    pub reason: Option<String>,
}

/// The whole menu: who holds the card and the kernels on offer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuModel {
    /// The title/tooltip text ("GPU: macOS").
    pub title: String,
    /// The entries, in configuration order.
    pub entries: Vec<MenuEntry>,
}

/// Build the menu for a status snapshot.
///
/// The kernel that holds the card is shown in the title, never offered as an
/// entry. A target that waits or is declined stays visible but disabled with
/// its reason.
#[must_use]
pub fn menu(status: &Status) -> MenuModel {
    let entries = status
        .targets
        .iter()
        .filter(|target| target.kernel != status.owner)
        .filter_map(|target| match &target.availability {
            Availability::AlreadyHeld => None,
            Availability::Ready => Some(MenuEntry {
                label: target.label.clone(),
                kernel: target.kernel.clone(),
                enabled: true,
                reason: None,
            }),
            Availability::Wait { reason } => Some(MenuEntry {
                label: target.label.clone(),
                kernel: target.kernel.clone(),
                enabled: false,
                reason: Some(describe::memory_refusal(reason)),
            }),
            Availability::Decline { reason } => Some(MenuEntry {
                label: target.label.clone(),
                kernel: target.kernel.clone(),
                enabled: false,
                reason: Some(describe::switch_refusal(reason)),
            }),
        })
        .collect();
    MenuModel {
        title: format!("GPU: {}", status.owner_label),
        entries,
    }
}
