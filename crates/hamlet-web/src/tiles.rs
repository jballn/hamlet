//! Village tile map definitions and topology (SPEC §2; ADR-0044).
//!
//! # SPEC §2 Normative Landmark ↔ Tree Topology Mapping Table
//!
//! Every landmark building and workshop zone rendered in the 8-bit village canvas
//! maps 1:1 onto the normative `.hamlet/` directory topology defined in SPEC §2:
//!
//! | Landmark ID | Landmark Name      | SPEC §2 Tree Path         | Architectural Purpose & Village Role           |
//! |-------------|--------------------|---------------------------|------------------------------------------------|
//! | `guild`     | Guild Hall         | `guild/`                  | Commissions ledger, register, blueprints       |
//! | `courier`   | Courier Post       | `courier/`                | Mail spools (/var/mail/), freight, dispatch    |
//! | `keep`      | Keep & Vault       | `keep/`                   | Promoted deliverables (/vault), archives       |
//! | `tavern`    | Tavern & Stasis    | `tavern/`                 | Hibernated patrons (/patrons), user gossip     |
//! | `market`    | Market Stalls      | `market/`                 | Skills repository (/skills), package cache     |
//! | `manor`     | Mayor's Manor      | `keep/census/manor/`      | Mayor orchestrator home (/home/1000/), decrees |
//! | `parcel*`   | Census Workshops   | `keep/census/parcel<uid>/`| Active agent workspaces, deeds, titles         |
//!
//! Note: Frontend layers render exclusively from daemon-pushed state frames;
//! they never inspect or mutate `.hamlet/` on disk directly (CONVENTIONS §2).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// 2D tile grid bounding box.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bounds {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl Bounds {
    /// Checks whether tile coordinates (tx, ty) fall inside this bounding box.
    #[must_use]
    pub fn contains(&self, tx: u32, ty: u32) -> bool {
        tx >= self.x && tx < self.x + self.w && ty >= self.y && ty < self.y + self.h
    }
}

/// 2D tile point.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Point {
    pub x: u32,
    pub y: u32,
}

/// Landmark building definition on the village map (SPEC §2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LandmarkDef {
    pub id: String,
    pub name: String,
    pub tree_path: String,
    pub bounds: Bounds,
    pub interact_point: Point,
    pub color: String,
    pub description: String,
}

/// Assigned tile coordinates for an agent parcel slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParcelSlotDef {
    pub uid: u32,
    pub x: u32,
    pub y: u32,
}

impl ParcelSlotDef {
    /// Returns the normative SPEC §2 tree path for this parcel.
    #[must_use]
    pub fn tree_path(&self) -> String {
        format!("keep/census/parcel{}/", self.uid)
    }

    /// Returns a bounding box representing the workshop parcel structure on the tile map.
    #[must_use]
    pub fn bounds(&self) -> Bounds {
        Bounds {
            x: self.x.saturating_sub(1),
            y: self.y.saturating_sub(1),
            w: 3,
            h: 3,
        }
    }
}

/// Master village map definition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VillageMapData {
    pub version: u32,
    pub tile_size: u32,
    pub map_width: u32,
    pub map_height: u32,
    pub landmarks: Vec<LandmarkDef>,
    pub parcel_slots: Vec<ParcelSlotDef>,
}

impl VillageMapData {
    /// Default canonical village layout embedded at compile-time.
    pub const DEFAULT_MAP_JSON: &'static str = include_str!("../assets/village_map.json");

    /// Loads the embedded canonical village map.
    #[must_use]
    pub fn load_default() -> Self {
        serde_json::from_str(Self::DEFAULT_MAP_JSON).expect("valid embedded village map json")
    }

    /// Finds landmark by string identifier (e.g. "guild", "courier", "keep", "tavern", "market", "manor").
    #[must_use]
    pub fn find_landmark(&self, id: &str) -> Option<&LandmarkDef> {
        self.landmarks.iter().find(|l| l.id == id)
    }

    /// Finds which landmark occupies tile coordinate (tx, ty).
    #[must_use]
    pub fn landmark_at(&self, tx: u32, ty: u32) -> Option<&LandmarkDef> {
        self.landmarks.iter().find(|l| l.bounds.contains(tx, ty))
    }

    /// Finds assigned slot coordinates for an agent UID.
    #[must_use]
    pub fn slot_for_uid(&self, uid: u32) -> Option<Point> {
        self.parcel_slots
            .iter()
            .find(|s| s.uid == uid)
            .map(|s| Point { x: s.x, y: s.y })
    }

    /// Finds assigned parcel slot definition for an agent UID.
    #[must_use]
    pub fn parcel_slot_for_uid(&self, uid: u32) -> Option<&ParcelSlotDef> {
        self.parcel_slots.iter().find(|s| s.uid == uid)
    }

    /// Slot map index keyed by UID.
    #[must_use]
    pub fn slot_map(&self) -> BTreeMap<u32, Point> {
        let mut map = BTreeMap::new();
        for slot in &self.parcel_slots {
            map.insert(
                slot.uid,
                Point {
                    x: slot.x,
                    y: slot.y,
                },
            );
        }
        map
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_village_map_loads_and_maps_spec_landmarks() {
        let map = VillageMapData::load_default();
        assert_eq!(map.version, 1);
        assert_eq!(map.tile_size, 16);
        assert_eq!(map.map_width, 32);
        assert_eq!(map.map_height, 24);

        // 6 normative landmarks from SPEC §2
        let expected_landmarks = ["guild", "courier", "keep", "tavern", "market", "manor"];
        for id in expected_landmarks {
            let lm = map
                .find_landmark(id)
                .unwrap_or_else(|| panic!("missing landmark: {id}"));
            assert!(!lm.name.is_empty());
            assert!(!lm.tree_path.is_empty());
            assert!(lm.bounds.w > 0 && lm.bounds.h > 0);
        }

        // Test coordinate hit testing
        let guild = map.landmark_at(3, 3).expect("guild at (3, 3)");
        assert_eq!(guild.id, "guild");

        // Test parcel slot lookup
        let slot_1001 = map.slot_for_uid(1001).expect("slot 1001");
        assert_eq!(slot_1001, Point { x: 10, y: 10 });
    }
}
