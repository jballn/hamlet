//! Pure scene graph representation emitted by the WASM view-model (SPEC §12.3; ADR-0044).

use serde::{Deserialize, Serialize};

/// High-level HUD statistics rendered over the village view.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct HudData {
    pub daemon_instance_id: String,
    pub seq: u64,
    pub active_agents: usize,
    pub active_commissions: usize,
    pub pending_decrees: usize,
    pub unread_mail: usize,
    pub total_cost_micro_usd: u64,
    #[serde(default)]
    pub ticker: Vec<String>,
    #[serde(default)]
    pub frame_budget_degraded: bool,
}

/// Renderable landmark building element (SPEC §2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenderLandmark {
    pub id: String,
    pub name: String,
    pub tree_path: String,
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
    pub color: String,
    pub item_count: usize,
    pub status_label: String,
}

/// Renderable parcel workshop structure in the census zone (SPEC §2 keep/census/parcel<uid>/).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenderWorkshop {
    pub uid: u32,
    pub parcel_slug: String,
    pub tree_path: String,
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
    pub agent_name: String,
    pub state: String,
    pub status_label: String,
}

/// Renderable agent or mayor sprite with state-driven animation parameters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RenderSprite {
    pub uid: u32,
    pub name: String,
    pub x: f32,
    pub y: f32,
    pub state: String,
    pub char_col: u32,
    pub char_row: u32,
    pub is_mayor: bool,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub cost_micro_usd: u64,
    #[serde(default)]
    pub anim_state: String,
    #[serde(default)]
    pub anim_frame: u32,
    #[serde(default)]
    pub tool_swing: bool,
    #[serde(default)]
    pub loiter: bool,
    #[serde(default)]
    pub zzz: bool,
    #[serde(default)]
    pub fallen: bool,
    #[serde(default)]
    pub attention_mark: bool,
    #[serde(default)]
    pub sparkline: Vec<u64>,
}

/// Renderable over-the-head chat bubble.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RenderBubble {
    pub target_uid: Option<u32>,
    pub target_id: String,
    pub x: f32,
    pub y: f32,
    pub text: String,
    pub kind: String,
    pub remaining_ms: u32,
    #[serde(default)]
    pub is_mail: bool,
}

/// Complete scene graph frame passed to the JS 2D canvas renderer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SceneGraph {
    pub map_width: u32,
    pub map_height: u32,
    pub tile_size: u32,
    pub landmarks: Vec<RenderLandmark>,
    #[serde(default)]
    pub workshops: Vec<RenderWorkshop>,
    pub sprites: Vec<RenderSprite>,
    pub bubbles: Vec<RenderBubble>,
    pub hud: HudData,
}

impl SceneGraph {
    /// Creates an empty initial scene graph with map geometry.
    #[must_use]
    pub fn empty(map_width: u32, map_height: u32, tile_size: u32) -> Self {
        Self {
            map_width,
            map_height,
            tile_size,
            landmarks: Vec::new(),
            workshops: Vec::new(),
            sprites: Vec::new(),
            bubbles: Vec::new(),
            hud: HudData::default(),
        }
    }
}
