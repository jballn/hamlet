//! C-ABI WebAssembly exports for JS glue layer (SPEC §12.3; ADR-0044).

use crate::view_model::VillageViewModel;
use hamlet_core::api::WsServerFrame;
use std::sync::Mutex;

static VILLAGE: Mutex<Option<VillageViewModel>> = Mutex::new(None);
static OUT_BUFFER: Mutex<Vec<u8>> = Mutex::new(Vec::new());
static PANEL_BUFFER: Mutex<Vec<u8>> = Mutex::new(Vec::new());

/// Protocol ABI version (matches ADR-0044).
pub const WASM_ABI_VERSION: u32 = 1;

/// Initializes the global village view-model.
#[no_mangle]
pub extern "C" fn hamlet_web_init() -> u32 {
    let mut lock = VILLAGE.lock().unwrap();
    *lock = Some(VillageViewModel::new());
    WASM_ABI_VERSION
}

/// Returns the WASM ABI version number.
#[no_mangle]
pub extern "C" fn hamlet_web_version() -> u32 {
    WASM_ABI_VERSION
}

/// Allocates a byte buffer of `len` bytes in WASM linear memory.
#[no_mangle]
pub extern "C" fn hamlet_web_alloc(len: usize) -> *mut u8 {
    let mut buf = Vec::with_capacity(len);
    let ptr = buf.as_mut_ptr();
    std::mem::forget(buf);
    ptr
}

/// Frees a previously allocated byte buffer.
///
/// # Safety
/// `ptr` must have been returned by `hamlet_web_alloc` with capacity `len`.
#[no_mangle]
pub unsafe extern "C" fn hamlet_web_dealloc(ptr: *mut u8, len: usize) {
    if !ptr.is_null() {
        drop(Vec::from_raw_parts(ptr, 0, len));
    }
}

/// Ingests a UTF-8 JSON server frame string. Returns 1 if state changed, 0 otherwise.
///
/// # Safety
/// `ptr` must be a valid pointer to `len` bytes of UTF-8 encoded text.
#[no_mangle]
pub unsafe extern "C" fn hamlet_web_process_frame(ptr: *const u8, len: usize) -> u32 {
    if ptr.is_null() || len == 0 {
        return 0;
    }
    let slice = std::slice::from_raw_parts(ptr, len);
    let frame: WsServerFrame = match serde_json::from_slice(slice) {
        Ok(f) => f,
        Err(_) => return 0,
    };

    let mut lock = VILLAGE.lock().unwrap();
    let vm = lock.get_or_insert_with(VillageViewModel::new);
    if vm.apply_server_frame(frame) {
        1
    } else {
        0
    }
}

/// Advances village simulation/animation timers by `dt_ms` milliseconds.
#[no_mangle]
pub extern "C" fn hamlet_web_tick(dt_ms: u32) {
    let mut lock = VILLAGE.lock().unwrap();
    if let Some(vm) = lock.as_mut() {
        vm.tick(dt_ms);
    }
}

/// Configures the frame-budget guard degraded mode.
/// 0 = normal (60fps target), 1 = degraded (accelerates bubble decay and clamps bubble count).
#[no_mangle]
pub extern "C" fn hamlet_web_set_degraded(degraded: u32) {
    let mut lock = VILLAGE.lock().unwrap();
    if let Some(vm) = lock.as_mut() {
        vm.set_degraded_mode(degraded != 0);
    }
}

/// Returns 1 if view-model flagged a sequence gap or resync requirement, 0 otherwise.
#[no_mangle]
pub extern "C" fn hamlet_web_resync_needed() -> u32 {
    let lock = VILLAGE.lock().unwrap();
    if let Some(vm) = lock.as_ref() {
        if vm.resync_needed() {
            1
        } else {
            0
        }
    } else {
        0
    }
}

/// Clears the resync needed flag in the view-model.
#[no_mangle]
pub extern "C" fn hamlet_web_clear_resync_needed() {
    let mut lock = VILLAGE.lock().unwrap();
    if let Some(vm) = lock.as_mut() {
        vm.clear_resync_needed();
    }
}

/// Serializes the current `SceneGraph` to JSON and writes pointer/length to out params.
///
/// # Safety
/// `out_len` must be a valid pointer to a `usize`.
#[no_mangle]
pub unsafe extern "C" fn hamlet_web_render_scene_json(out_len: *mut usize) -> *const u8 {
    let mut lock = VILLAGE.lock().unwrap();
    let vm = lock.get_or_insert_with(VillageViewModel::new);
    let scene = vm.generate_scene();
    let json_bytes = serde_json::to_vec(&scene).unwrap_or_default();

    let mut out = OUT_BUFFER.lock().unwrap();
    *out = json_bytes;
    if !out_len.is_null() {
        *out_len = out.len();
    }
    out.as_ptr()
}

/// Opens a management panel for a given kind and target.
///
/// # Safety
/// `kind_ptr` and `target_ptr` must be valid UTF-8 buffers of length `kind_len` and `target_len`.
#[no_mangle]
pub unsafe extern "C" fn hamlet_web_open_panel(
    kind_ptr: *const u8,
    kind_len: usize,
    target_ptr: *const u8,
    target_len: usize,
) -> u32 {
    if kind_ptr.is_null() || kind_len == 0 {
        return 0;
    }
    let kind_slice = std::slice::from_raw_parts(kind_ptr, kind_len);
    let kind_str = match std::str::from_utf8(kind_slice) {
        Ok(s) => s,
        Err(_) => return 0,
    };

    let target_str = if !target_ptr.is_null() && target_len > 0 {
        let target_slice = std::slice::from_raw_parts(target_ptr, target_len);
        std::str::from_utf8(target_slice)
            .ok()
            .map(|s| s.to_string())
    } else {
        None
    };

    let kind = match kind_str {
        "agent" => crate::panels::PanelKind::Agent,
        "guild" => crate::panels::PanelKind::Guild,
        "vault" => crate::panels::PanelKind::Vault,
        "decree_desk" | "manor" => crate::panels::PanelKind::DecreeDesk,
        "courier" => crate::panels::PanelKind::Courier,
        "tavern" => crate::panels::PanelKind::Tavern,
        _ => return 0,
    };

    let mut lock = VILLAGE.lock().unwrap();
    let vm = lock.get_or_insert_with(VillageViewModel::new);
    vm.open_panel(kind, target_str);
    1
}

/// Closes the currently active management panel.
#[no_mangle]
pub extern "C" fn hamlet_web_close_panel() {
    let mut lock = VILLAGE.lock().unwrap();
    if let Some(vm) = lock.as_mut() {
        vm.close_panel();
    }
}

/// Serializes the currently active panel view model to JSON and returns pointer/length.
///
/// # Safety
/// `out_len` must be a valid pointer to a `usize`.
#[no_mangle]
pub unsafe extern "C" fn hamlet_web_render_panel_json(out_len: *mut usize) -> *const u8 {
    let mut lock = VILLAGE.lock().unwrap();
    let vm = lock.get_or_insert_with(VillageViewModel::new);
    let panel_view = vm.generate_panel_view();
    let json_bytes = serde_json::to_vec(&panel_view).unwrap_or_default();

    let mut out = PANEL_BUFFER.lock().unwrap();
    *out = json_bytes;
    if !out_len.is_null() {
        *out_len = out.len();
    }
    out.as_ptr()
}

/// Records an optimistic mutation command awaiting server ACK.
///
/// # Safety
/// `cid_ptr`, `desc_ptr`, and `tgt_ptr` must be valid UTF-8 buffers.
#[no_mangle]
pub unsafe extern "C" fn hamlet_web_record_mutation(
    cid_ptr: *const u8,
    cid_len: usize,
    desc_ptr: *const u8,
    desc_len: usize,
    tgt_ptr: *const u8,
    tgt_len: usize,
) -> u32 {
    if cid_ptr.is_null() || cid_len == 0 {
        return 0;
    }
    let cid_slice = std::slice::from_raw_parts(cid_ptr, cid_len);
    let cid = match std::str::from_utf8(cid_slice) {
        Ok(s) => s.to_string(),
        Err(_) => return 0,
    };

    let desc = if !desc_ptr.is_null() && desc_len > 0 {
        let desc_slice = std::slice::from_raw_parts(desc_ptr, desc_len);
        std::str::from_utf8(desc_slice).unwrap_or("").to_string()
    } else {
        String::new()
    };

    let tgt = if !tgt_ptr.is_null() && tgt_len > 0 {
        let tgt_slice = std::slice::from_raw_parts(tgt_ptr, tgt_len);
        std::str::from_utf8(tgt_slice).unwrap_or("").to_string()
    } else {
        String::new()
    };

    let mut lock = VILLAGE.lock().unwrap();
    let vm = lock.get_or_insert_with(VillageViewModel::new);
    vm.record_optimistic_command(cid, desc, tgt);
    1
}
