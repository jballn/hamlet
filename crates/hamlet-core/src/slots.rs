//! UID slot allocation (SPEC §4; CONVENTIONS §3 rule 5; ADR-0004).
//!
//! UID 1000 is statically reserved for the Mayor (manor). Subagent slots
//! 1001–2000 are allocated **sequentially and permanently** per agent
//! identity: a slot is never reused for a different identity while any
//! record references it (hibernated profiles, patron stasis, parcels).
//! Exhaustion beyond 2000 is a typed hard error surfaced to the operator,
//! never a wrap or silent reuse.
//!
//! Permanence is enforced structurally: this module offers **no removal
//! API**. Records transition status (`active` → `hibernated`/`crashed` →
//! `active`) but the uid → identity mapping persists for the life of the
//! charter (SPEC §10: hibernation frees compute, not the slot).
//!
//! Pure allocation logic over `BTreeMap` charter state; no I/O, no
//! wall-clock reads.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::sanitize::SafeName;

/// UID statically reserved for the Mayor / manor identity (SPEC §4).
pub const MAYOR_UID: u32 = 1000;

/// First subagent UID (SPEC §4).
pub const FIRST_AGENT_UID: u32 = 1001;

/// Last subagent UID; exhaustion beyond this is a hard error (SPEC §4).
pub const LAST_AGENT_UID: u32 = 2000;

/// Number of allocatable subagent slots (`FIRST_AGENT_UID..=LAST_AGENT_UID`).
pub const AGENT_SLOT_CAPACITY: usize = (LAST_AGENT_UID - FIRST_AGENT_UID + 1) as usize;

/// Lifecycle status of a slot's identity (v1 shape; sprint 07 extends
/// hibernation/census semantics on top — status transitions only, the
/// mapping itself is permanent).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SlotStatus {
    /// Running or startable.
    Active,
    /// Compute freed, profile parked (SPEC §10); mapping persists.
    Hibernated,
    /// Process died unexpectedly; daemon marks on restart when a live
    /// process cannot be re-attached (CONVENTIONS §3 rule 6).
    Crashed,
}

/// The identity record bound to a UID slot, permanently.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SlotRecord {
    /// The sanitized identity name this slot belongs to — forever.
    pub identity: SafeName,
    /// Current lifecycle status.
    pub status: SlotStatus,
    /// Commission this slot is working (optional; `None` when idle).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commission: Option<u64>,
}

impl SlotRecord {
    /// A fresh active record for `identity`.
    #[must_use]
    pub fn new(identity: SafeName, commission: Option<u64>) -> Self {
        Self {
            identity,
            status: SlotStatus::Active,
            commission,
        }
    }
}

/// Typed hard errors from slot operations. Exhaustion is *never* a wrap
/// (CONVENTIONS §3 rule 5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlotError {
    /// All slots 1001–2000 are allocated; the operator must act.
    Exhausted {
        /// Identity that could not be allocated.
        identity: SafeName,
    },
    /// UID 1000 already belongs to a different identity.
    MayorConflict {
        /// Current holder of the mayor slot.
        holder: SafeName,
        /// Identity the caller tried to reserve it for.
        attempted: SafeName,
    },
    /// No record exists for `uid`.
    NoSuchSlot {
        /// The unknown uid.
        uid: u32,
    },
}

impl fmt::Display for SlotError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Exhausted { identity } => write!(
                f,
                "UID slots {FIRST_AGENT_UID}-{LAST_AGENT_UID} exhausted; cannot allocate '{identity}' (hard error, never wrap)"
            ),
            Self::MayorConflict { holder, attempted } => write!(
                f,
                "UID {MAYOR_UID} is reserved for '{holder}', cannot re-reserve for '{attempted}'"
            ),
            Self::NoSuchSlot { uid } => write!(f, "no slot record for UID {uid}"),
        }
    }
}

impl std::error::Error for SlotError {}

/// Slot table type used in the charter (`uid → identity record`).
pub type SlotTable = BTreeMap<u32, SlotRecord>;

/// Reserve UID 1000 for the Mayor/manor identity. Idempotent: reserving
/// the same identity again is a no-op; a *different* identity is a typed
/// [`SlotError::MayorConflict`].
///
/// # Errors
///
/// [`SlotError::MayorConflict`] if 1000 is already held by another identity.
pub fn reserve_mayor(slots: &mut SlotTable, identity: SafeName) -> Result<u32, SlotError> {
    match slots.get(&MAYOR_UID) {
        Some(record) if record.identity == identity => Ok(MAYOR_UID),
        Some(record) => Err(SlotError::MayorConflict {
            holder: record.identity.clone(),
            attempted: identity,
        }),
        None => {
            slots.insert(MAYOR_UID, SlotRecord::new(identity, None));
            Ok(MAYOR_UID)
        }
    }
}

/// Allocate the next sequential permanent slot for `identity`.
///
/// - If `identity` already holds a slot (any status, including
///   `Hibernated`/`Crashed`), that uid is returned unchanged — identities
///   never migrate between slots and slots are never reused.
/// - Otherwise the lowest free uid in 1001–2000 is bound to `identity`.
///
/// # Errors
///
/// [`SlotError::Exhausted`] when every uid in 1001–2000 is occupied.
pub fn allocate(
    slots: &mut SlotTable,
    identity: SafeName,
    commission: Option<u64>,
) -> Result<u32, SlotError> {
    if let Some((&uid, _)) = slots.iter().find(|(_, record)| record.identity == identity) {
        return Ok(uid); // permanence: same identity → same slot, forever
    }
    let Some(uid) = next_free_uid(slots) else {
        return Err(SlotError::Exhausted { identity });
    };
    slots.insert(uid, SlotRecord::new(identity, commission));
    Ok(uid)
}

/// Lowest unallocated uid in 1001–2000, or `None` when the range is full.
/// Pure scan over the sorted table; never returns an occupied uid, so a
/// slot referenced by any surviving record can never be handed out again.
#[must_use]
pub fn next_free_uid(slots: &SlotTable) -> Option<u32> {
    let mut candidate = FIRST_AGENT_UID;
    for uid in slots
        .range(FIRST_AGENT_UID..=LAST_AGENT_UID)
        .map(|(&k, _)| k)
    {
        if uid > candidate {
            return Some(candidate);
        }
        candidate = uid + 1;
        if candidate > LAST_AGENT_UID {
            return None;
        }
    }
    (candidate <= LAST_AGENT_UID).then_some(candidate)
}

/// Transition `uid` to [`SlotStatus::Hibernated`]. The mapping persists
/// (SPEC §10); only compute is freed.
///
/// # Errors
///
/// [`SlotError::NoSuchSlot`] if `uid` has no record.
pub fn hibernate(slots: &mut SlotTable, uid: u32) -> Result<(), SlotError> {
    transition(slots, uid, SlotStatus::Hibernated)
}

/// Mark `uid` [`SlotStatus::Crashed`] (daemon restart found no live process
/// to re-attach). The mapping persists.
///
/// # Errors
///
/// [`SlotError::NoSuchSlot`] if `uid` has no record.
pub fn mark_crashed(slots: &mut SlotTable, uid: u32) -> Result<(), SlotError> {
    transition(slots, uid, SlotStatus::Crashed)
}

/// Return `uid` to [`SlotStatus::Active`] (awakening / restart recovery).
///
/// # Errors
///
/// [`SlotError::NoSuchSlot`] if `uid` has no record.
pub fn reactivate(slots: &mut SlotTable, uid: u32) -> Result<(), SlotError> {
    transition(slots, uid, SlotStatus::Active)
}

fn transition(slots: &mut SlotTable, uid: u32, status: SlotStatus) -> Result<(), SlotError> {
    match slots.get_mut(&uid) {
        Some(record) => {
            record.status = status;
            Ok(())
        }
        None => Err(SlotError::NoSuchSlot { uid }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sanitize::sanitize_name;

    fn name(s: &str) -> SafeName {
        sanitize_name(s).unwrap()
    }

    #[test]
    fn mayor_reservation_is_static_idempotent_and_conflict_typed() {
        let mut slots = SlotTable::new();
        assert_eq!(reserve_mayor(&mut slots, name("manor")).unwrap(), MAYOR_UID);
        assert_eq!(reserve_mayor(&mut slots, name("manor")).unwrap(), MAYOR_UID);
        assert_eq!(slots.len(), 1);
        assert_eq!(
            reserve_mayor(&mut slots, name("impostor")),
            Err(SlotError::MayorConflict {
                holder: name("manor"),
                attempted: name("impostor")
            })
        );
    }

    #[test]
    fn allocation_is_sequential_from_1001() {
        let mut slots = SlotTable::new();
        reserve_mayor(&mut slots, name("manor")).unwrap();
        assert_eq!(allocate(&mut slots, name("ada"), None).unwrap(), 1001);
        assert_eq!(allocate(&mut slots, name("grace"), Some(7)).unwrap(), 1002);
        assert_eq!(allocate(&mut slots, name("alan"), None).unwrap(), 1003);
        assert_eq!(slots[&1002].commission, Some(7));
        assert_eq!(slots[&1002].status, SlotStatus::Active);
        // Mayor slot untouched by subagent allocation.
        assert_eq!(slots[&MAYOR_UID].identity, name("manor"));
    }

    #[test]
    fn reallocation_is_permanent_and_idempotent() {
        let mut slots = SlotTable::new();
        let first = allocate(&mut slots, name("ada"), None).unwrap();
        let again = allocate(&mut slots, name("ada"), Some(3)).unwrap();
        assert_eq!(first, again);
        assert_eq!(slots.len(), 1);
        // Original commission ref is NOT silently overwritten by allocate.
        assert_eq!(slots[&first].commission, None);
    }

    #[test]
    fn hibernated_slots_keep_mapping_and_are_never_reused() {
        let mut slots = SlotTable::new();
        let ada = allocate(&mut slots, name("ada"), None).unwrap();
        let _grace = allocate(&mut slots, name("grace"), None).unwrap();
        hibernate(&mut slots, ada).unwrap();
        // Mapping persists (SPEC §10): record still present, identity intact.
        assert_eq!(slots[&ada].identity, name("ada"));
        assert_eq!(slots[&ada].status, SlotStatus::Hibernated);
        // A NEW identity never receives the hibernated slot.
        let alan = allocate(&mut slots, name("alan"), None).unwrap();
        assert_eq!(alan, 1003);
        // The SAME identity re-awakens on its original slot.
        assert_eq!(allocate(&mut slots, name("ada"), None).unwrap(), ada);
        reactivate(&mut slots, ada).unwrap();
        assert_eq!(slots[&ada].status, SlotStatus::Active);
    }

    #[test]
    fn crashed_transition_and_unknown_uid() {
        let mut slots = SlotTable::new();
        let uid = allocate(&mut slots, name("ada"), None).unwrap();
        mark_crashed(&mut slots, uid).unwrap();
        assert_eq!(slots[&uid].status, SlotStatus::Crashed);
        assert_eq!(
            mark_crashed(&mut slots, 1999),
            Err(SlotError::NoSuchSlot { uid: 1999 })
        );
        assert_eq!(
            hibernate(&mut slots, 1999),
            Err(SlotError::NoSuchSlot { uid: 1999 })
        );
    }

    #[test]
    fn exhaustion_beyond_2000_is_typed_hard_error() {
        let mut slots = SlotTable::new();
        for i in 0..AGENT_SLOT_CAPACITY {
            let uid = allocate(&mut slots, name(&format!("agent-{i}")), None).unwrap();
            assert_eq!(uid, FIRST_AGENT_UID + i as u32);
        }
        assert_eq!(next_free_uid(&slots), None);
        assert_eq!(
            allocate(&mut slots, name("one-too-many"), None),
            Err(SlotError::Exhausted {
                identity: name("one-too-many")
            })
        );
        // Table still holds exactly the 1000 permanent records.
        assert_eq!(slots.len(), AGENT_SLOT_CAPACITY);
    }

    #[test]
    fn next_free_uid_scans_lowest_gap() {
        let mut slots = SlotTable::new();
        assert_eq!(next_free_uid(&slots), Some(1001));
        slots.insert(1002, SlotRecord::new(name("grace"), None));
        assert_eq!(next_free_uid(&slots), Some(1001));
        slots.insert(1001, SlotRecord::new(name("ada"), None));
        assert_eq!(next_free_uid(&slots), Some(1003));
        // Mayor slot is outside the scan range.
        slots.insert(MAYOR_UID, SlotRecord::new(name("manor"), None));
        assert_eq!(next_free_uid(&slots), Some(1003));
    }
}
