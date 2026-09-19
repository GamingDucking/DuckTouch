/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! GameGuardian-style on-screen trainer overlay: a small floating button in
//! the top-right corner of the viewport opens a touch panel for searching and
//! editing guest memory while a game is running.
//!
//! The overlay is drawn host-side on top of the presented frame (see
//! `crate::gles::present::present_frame`) using the GLES 1.x fixed-function
//! API, and receives raw window-space touch coordinates from
//! `crate::window` before they are forwarded to the guest. Commands produced
//! by the panel are executed by the trainer engine (`crate::trainer`) on the
//! main loop thread, where `&mut Mem` is available.

use crate::font::{Font, TextAlignment};
use crate::gles::gles11_raw as gles11;
use crate::gles::{GLES, GLint, GLuint};
use crate::trainer::{SearchResult, VType};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

// ---------------------------------------------------------------------------
// Commands sent from the overlay to the trainer engine.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub enum TrainerCmd {
    Search { vtype: VType, text: String },
    Refine { vtype: VType, text: String },
    Reset,
    Set { vtype: VType, text: String },
    Freeze { vtype: VType, text: String },
    UnfreezeAll,
    Dump,
    SaveHack { vtype: VType },
    ApplyHack { addr: u32, vtype: VType, bits: u64 },
}

static COMMANDS: Mutex<Vec<TrainerCmd>> = Mutex::new(Vec::new());

// ---------------------------------------------------------------------------
// Overlay state, shared between the input path (window thread), the draw
// path (present callback) and the engine (main loop tick).
// ---------------------------------------------------------------------------

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum Focus {
    Search,
    Set,
}

struct TrainerUi {
    enabled: bool,
    app_id: Option<String>,
    open: bool,
    focus: Focus,
    search_text: String,
    set_text: String,
    vtype: VType,
    results: Vec<SearchResult>,
    total_results: usize,
    selected: Option<u32>,
    scroll: usize,
    status: String,
    frozen_count: usize,
    /// Widget id pressed but not yet released (pending activation).
    pending: Option<u16>,
}

impl TrainerUi {
    const fn new() -> TrainerUi {
        TrainerUi {
            enabled: true,
            app_id: None,
            open: false,
            focus: Focus::Search,
            search_text: String::new(),
            set_text: String::new(),
            vtype: VType::I32,
            results: Vec::new(),
            total_results: 0,
            selected: None,
            scroll: 0,
            status: String::new(),
            frozen_count: 0,
            pending: None,
        }
    }
}

static UI: Mutex<TrainerUi> = Mutex::new(TrainerUi::new());
static HARDWARE_ENABLED: AtomicBool = AtomicBool::new(true);

/// Master switch, driven by the `--no-trainer` option.
pub fn set_hardware_enabled(enabled: bool) {
    HARDWARE_ENABLED.store(enabled, Ordering::SeqCst);
}

/// Called by the trainer engine whenever the running app changes.
pub fn reset_for_app(app_id: Option<&str>) {
    let mut ui = UI.lock().unwrap();
    ui.app_id = app_id.map(String::from);
    ui.open = false;
    ui.focus = Focus::Search;
    ui.search_text.clear();
    ui.set_text.clear();
    ui.results.clear();
    ui.total_results = 0;
    ui.selected = None;
    ui.scroll = 0;
    ui.status.clear();
    ui.frozen_count = 0;
    ui.pending = None;
}

pub fn take_commands() -> Vec<TrainerCmd> {
    std::mem::take(&mut COMMANDS.lock().unwrap())
}

pub fn publish_results(results: &[SearchResult], total: usize) {
    let mut ui = UI.lock().unwrap();
    ui.results = results.iter().copied().take(200).collect();
    ui.total_results = total;
    ui.scroll = 0;
}

pub fn publish_status(status: String) {
    UI.lock().unwrap().status = status;
}

pub fn publish_frozen(count: usize) {
    UI.lock().unwrap().frozen_count = count;
}

pub fn selected_address() -> Option<u32> {
    UI.lock().unwrap().selected
}

// ---------------------------------------------------------------------------
// Layout & hit testing.
// ---------------------------------------------------------------------------

const W_BUTTON: u16 = 1;
const W_CLOSE: u16 = 2;
const W_TYPE: u16 = 3;
const W_FIELD_SEARCH: u16 = 4;
const W_FIELD_SET: u16 = 5;
const W_SEARCH: u16 = 6;
const W_REFINE: u16 = 7;
const W_RESET: u16 = 8;
const W_SET: u16 = 9;
const W_FREEZE: u16 = 10;
const W_UNFREEZE: u16 = 11;
const W_DUMP: u16 = 12;
const W_SAVE: u16 = 13;
const W_SCROLL_UP: u16 = 14;
const W_SCROLL_DOWN: u16 = 15;
const W_RESULT_BASE: u16 = 32; // + row index
const W_KEY_BASE: u16 = 64; // + key index

#[derive(Copy, Clone, Debug)]
struct Rect {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

impl Rect {
    fn contains(&self, px: f32, py: f32) -> bool {
        px >= self.x && px < self.x + self.w && py >= self.y && py < self.y + self.h
    }
}

/// Rows of the on-screen keypad: digits, sign and editing keys.
const KEYPAD: [[Option<char>; 4]; 4] = [
    [Some('1'), Some('2'), Some('3'), Some('\u{8}')],
    [Some('4'), Some('5'), Some('6'), Some('\u{4}')],
    [Some('7'), Some('8'), Some('9'), Some('.')],
    [Some('-'), Some('0'), None, None],
];

struct Layout {
    scale: f32,
    button: Rect,
    panel: Option<PanelLayout>,
}

struct PanelLayout {
    rect: Rect,
    widgets: Vec<(u16, Rect)>,
}

const RESULT_ROWS: usize = 5;

fn compute_layout(ui: &TrainerUi, viewport: (u32, u32, u32, u32)) -> Layout {
    let (vx, vy, vw, vh) = viewport;
    let (vx, vy, vw, vh) = (vx as f32, vy as f32, vw as f32, vh as f32);
    // UI scale: reference height is 480 px (iPhone portrait).
    let s = (vh / 480.0).clamp(0.75, 4.0);
    let btn = 30.0 * s;
    let button = Rect {
        x: vx + vw - btn - 6.0 * s,
        y: vy + 6.0 * s,
        w: btn,
        h: btn,
    };
    let panel: Option<PanelLayout> = if ui.open {
        let pw = (vw - 12.0 * s).min(300.0 * s);
        let px = vx + vw - pw - 6.0 * s;
        // Leave room for the button row above the panel.
        let py = button.y + button.h + 4.0 * s;
        let row_h = 24.0 * s;
        let small_h = 20.0 * s;
        let key_h = 26.0 * s;
        let mut widgets = Vec::new();
        let mut y = py + 4.0 * s;
        let mut push_widget = |id: u16, wx: f32, wy: f32, ww: f32, wh: f32, widgets: &mut Vec<(u16, Rect)>| {
            widgets.push((id, Rect { x: wx, y: wy, w: ww, h: wh }));
        };
        // Header row (title + close button).
        push_widget(W_CLOSE, px + pw - small_h - 4.0 * s, y, small_h, small_h, &mut widgets);
        y += small_h + 4.0 * s;
        // Type row.
        push_widget(W_TYPE, px + 6.0 * s, y, 110.0 * s, row_h, &mut widgets);
        y += row_h + 4.0 * s;
        // Search value field.
        push_widget(W_FIELD_SEARCH, px + 6.0 * s, y, pw - 12.0 * s, row_h, &mut widgets);
        y += row_h + 4.0 * s;
        // Keypad: 4 columns x 4 rows.
        let key_w = (pw - 12.0 * s) / 4.0;
        for (row_idx, row) in KEYPAD.iter().enumerate() {
            for (col_idx, _) in row.iter().enumerate() {
                if row[col_idx].is_none() {
                    continue;
                }
                let kx = px + 6.0 * s + key_w * col_idx as f32;
                let ky = y + key_h * row_idx as f32;
                push_widget(
                    W_KEY_BASE + (row_idx * 4 + col_idx) as u16,
                    kx,
                    ky,
                    key_w,
                    key_h,
                    &mut widgets,
                );
            }
        }
        y += key_h * 4.0 + 4.0 * s;
        // Actions row.
        let third = (pw - 12.0 * s - 8.0 * s) / 3.0;
        for (i, id) in [W_SEARCH, W_REFINE, W_RESET].iter().enumerate() {
            push_widget(*id, px + 6.0 * s + (third + 4.0 * s) * i as f32, y, third, row_h, &mut widgets);
        }
        y += row_h + 4.0 * s;
        // Results header + scroll buttons.
        let res_h = 18.0 * s;
        push_widget(W_SCROLL_UP, px + pw - 2.0 * (res_h + 3.0 * s), y, res_h, res_h, &mut widgets);
        push_widget(W_SCROLL_DOWN, px + pw - res_h - 3.0 * s, y, res_h, res_h, &mut widgets);
        y += res_h + 2.0 * s;
        // Result rows.
        for i in 0..RESULT_ROWS {
            push_widget(W_RESULT_BASE + i as u16, px + 6.0 * s, y, pw - 12.0 * s, res_h, &mut widgets);
            y += res_h;
        }
        y += 3.0 * s;
        // Set value field.
        push_widget(W_FIELD_SET, px + 6.0 * s, y, pw - 12.0 * s, row_h, &mut widgets);
        y += row_h + 4.0 * s;
        // Set / freeze row.
        let third = (pw - 12.0 * s - 8.0 * s) / 3.0;
        for (i, id) in [W_SET, W_FREEZE, W_UNFREEZE].iter().enumerate() {
            push_widget(*id, px + 6.0 * s + (third + 4.0 * s) * i as f32, y, third, row_h, &mut widgets);
        }
        y += row_h + 4.0 * s;
        // Dump / save row.
        let half = (pw - 12.0 * s - 4.0 * s) / 2.0;
        for (i, id) in [W_DUMP, W_SAVE].iter().enumerate() {
            push_widget(*id, px + 6.0 * s + (half + 4.0 * s) * i as f32, y, half, row_h, &mut widgets);
        }
        y += row_h + 4.0 * s;
        // Status line (not interactive).
        let ph = y + small_h + 4.0 * s - py;
        Some(PanelLayout {
            rect: Rect { x: px, y: py, w: pw, h: ph },
            widgets,
        })
    } else {
        None
    };
    Layout { scale: s, button, panel }
}

// ---------------------------------------------------------------------------
// Touch input, called from crate::window with raw window-space coordinates.
// Returns true if the touch was consumed by the overlay.
// ---------------------------------------------------------------------------

fn overlay_active() -> bool {
    HARDWARE_ENABLED.load(Ordering::SeqCst) && {
        let ui = UI.lock().unwrap();
        ui.enabled && ui.app_id.is_some()
    }
}

pub fn touch_down(abs: (f32, f32), viewport: (u32, u32, u32, u32)) -> bool {
    if !overlay_active() {
        return false;
    }
    let mut ui = UI.lock().unwrap();
    let layout = compute_layout(&ui, viewport);
    let (x, y) = abs;
    if layout.button.contains(x, y) {
        ui.pending = Some(W_BUTTON);
        return true;
    }
    if let Some(panel) = &layout.panel {
        if !panel.rect.contains(x, y) {
            return false; // outside the panel: let the game have the touch
        }
        for (id, rect) in &panel.widgets {
            if rect.contains(x, y) {
                ui.pending = Some(*id);
                return true;
            }
        }
        // Inside the panel but not on a widget: swallow the touch.
        return true;
    }
    false
}

pub fn touch_motion(abs: (f32, f32), _viewport: (u32, u32, u32, u32)) -> bool {
    if !overlay_active() {
        return false;
    }
    let ui = UI.lock().unwrap();
    // Swallow motion while a press is pending so the game doesn't see drags
    // that started on the overlay.
    ui.pending.is_some()
}

pub fn touch_up(abs: (f32, f32), viewport: (u32, u32, u32, u32)) -> bool {
    if !overlay_active() {
        return false;
    }
    let mut ui = UI.lock().unwrap();
    let layout = compute_layout(&ui, viewport);
    let (x, y) = abs;
    let Some(pending) = ui.pending.take() else {
        // Finger wasn't consumed on the way down.
        if layout.button.contains(x, y) || layout.panel.as_ref().map_or(false, |p| p.rect.contains(x, y)) {
            return true;
        }
        return false;
    };
    // Activate if the finger is released over the same widget.
    let hit = pending == W_BUTTON && layout.button.contains(x, y)
        || layout
            .panel
            .as_ref()
            .map_or(false, |p| p.widgets.iter().any(|(id, r)| *id == pending && r.contains(x, y)));
    if hit {
        activate_widget(&mut ui, pending);
    }
    true
}

fn activate_widget(ui: &mut TrainerUi, id: u16) {
    match id {
        W_BUTTON => ui.open = !ui.open,
        W_CLOSE => ui.open = false,
        W_TYPE => ui.vtype = ui.vtype.next(),
        W_FIELD_SEARCH => ui.focus = Focus::Search,
        W_FIELD_SET => ui.focus = Focus::Set,
        W_SEARCH => {
            let text = ui.search_text.trim().to_string();
            COMMANDS
                .lock()
                .unwrap()
                .push(TrainerCmd::Search { vtype: ui.vtype, text });
        }
        W_REFINE => {
            let text = ui.search_text.trim().to_string();
            COMMANDS
                .lock()
                .unwrap()
                .push(TrainerCmd::Refine { vtype: ui.vtype, text });
        }
        W_RESET => {
            COMMANDS.lock().unwrap().push(TrainerCmd::Reset);
        }
        W_SET => {
            let text = ui.set_text.trim().to_string();
            COMMANDS.lock().unwrap().push(TrainerCmd::Set { vtype: ui.vtype, text });
        }
        W_FREEZE => {
            let text = ui.set_text.trim().to_string();
            COMMANDS
                .lock()
                .unwrap()
                .push(TrainerCmd::Freeze { vtype: ui.vtype, text });
        }
        W_UNFREEZE => {
            COMMANDS.lock().unwrap().push(TrainerCmd::UnfreezeAll);
        }
        W_DUMP => {
            COMMANDS.lock().unwrap().push(TrainerCmd::Dump);
        }
        W_SAVE => {
            COMMANDS.lock().unwrap().push(TrainerCmd::SaveHack { vtype: ui.vtype });
        }
        W_SCROLL_UP => ui.scroll = ui.scroll.saturating_sub(RESULT_ROWS),
        W_SCROLL_DOWN => {
            let last = ui.total_results.saturating_sub(RESULT_ROWS);
            if ui.scroll + RESULT_ROWS < last + RESULT_ROWS && ui.scroll < last {
                ui.scroll = (ui.scroll + RESULT_ROWS).min(last);
            }
        }
        id if (W_RESULT_BASE..W_RESULT_BASE + RESULT_ROWS as u16).contains(&id) => {
            let idx = ui.scroll + (id - W_RESULT_BASE) as usize;
            if idx < ui.results.len() {
                ui.selected = Some(ui.results[idx].addr);
                ui.status = format!("SELECTED 0x{:X}", ui.results[idx].addr);
            }
        }
        id if (W_KEY_BASE..W_KEY_BASE + 16).contains(&id) => {
            let key_idx = (id - W_KEY_BASE) as usize;
            let row = key_idx / 4;
            let col = key_idx % 4;
            if let Some(Some(ch)) = KEYPAD.get(row).and_then(|r| r.get(col)) {
                let field = match ui.focus {
                    Focus::Search => &mut ui.search_text,
                    Focus::Set => &mut ui.set_text,
                };
                match ch {
                    '\u{8}' => {
                        field.pop();
                    }
                    '\u{4}' => field.clear(),
                    _ => {
                        if field.len() < 15 {
                            field.push(*ch);
                        }
                    }
                }
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Drawing. Only the GLES 1.x fixed-function API is used, matching
// present_frame's own overlay, so this works on every backend.
// ---------------------------------------------------------------------------

const FONT_PX: f32 = 14.0;
const ATLAS_CHARS: &str = " !\"#$%&'()*+,-./0123456789:;<=>?@ABCDEFGHIJKLMNOPQRSTUVWXYZ[\\]^_`abcdefghijklmnopqrstuvwxyz{|}~";

struct GlyphCell {
    /// Atlas-space position and size of the glyph bitmap.
    u0: f32,
    v0: f32,
    u1: f32,
    v1: f32,
    /// Per-character screen metrics (at font size units).
    advance: f32,
    draw_dx: f32,
    draw_dy: f32,
    draw_w: f32,
    draw_h: f32,
}

struct Atlas {
    tex: GLuint,
    height: f32,
    glyphs: Vec<Option<GlyphCell>>,
}

static ATLAS: OnceLock<Mutex<Option<Atlas>>> = OnceLock::new();

fn char_index(ch: char) -> Option<usize> {
    ATLAS_CHARS.chars().position(|c| c == ch)
}

unsafe fn build_atlas(gles: &mut dyn GLES) -> Option<Atlas> {
    let font = Font::mono_regular();
    let units_per_em = font.units_per_em() as f32;
    let mut raw: Vec<(char, f32, (f32, f32), (i32, i32), Vec<f32>)> = Vec::new();
    let mut min_y = f32::MAX;
    let mut max_y = f32::MIN;
    for ch in ATLAS_CHARS.chars() {
        let glyph_id = font.glyph_id_for_char(ch as u16);
        let advance = font.glyph_advance(glyph_id) as f32 * FONT_PX / units_per_em;
        let mut captured: Option<((f32, f32), (i32, i32), Vec<f32>)> = None;
        font.draw(
            FONT_PX,
            &ch.to_string(),
            (0.0, 0.0),
            None,
            TextAlignment::Left,
            |glyph| {
                let (origin, dims) = (glyph.origin(), glyph.dimensions());
                if dims.0 <= 0 || dims.1 <= 0 {
                    return;
                }
                let mut pixels = Vec::with_capacity((dims.0 * dims.1) as usize);
                for y in 0..dims.1 {
                    for x in 0..dims.0 {
                        pixels.push(glyph.pixel_at((x, y)));
                    }
                }
                captured = Some((origin, dims, pixels));
            },
        );
        if let Some((origin, dims, pixels)) = captured {
            min_y = min_y.min(origin.1);
            max_y = max_y.max(origin.1 + dims.1 as f32);
            raw.push((ch, advance, origin, dims, pixels));
        } else {
            raw.push((ch, advance, (0.0, 0.0), (0, 0), Vec::new()));
        }
    }
    if min_y > max_y {
        return None;
    }
    let cell_h = max_y - min_y;
    let mut atlas_w = 0.0f32;
    let mut cells: Vec<Option<GlyphCell>> = Vec::with_capacity(ATLAS_CHARS.len());
    for (_ch, advance, _origin, dims, _pixels) in &raw {
        let gw = if dims.0 > 0 { dims.0 as f32 } else { 0.0 };
        let u0 = atlas_w + 1.0; // 1px padding to avoid bleeding
        let u1 = u0 + gw;
        cells.push(Some(GlyphCell {
            u0: u0 / atlas_w.max(1.0),
            v0: 0.0,
            u1: u1 / atlas_w.max(1.0),
            v1: cell_h / cell_h.max(1.0),
            advance: *advance,
            draw_dx: 0.0,
            draw_dy: 0.0,
            draw_w: gw,
            draw_h: dims.1 as f32,
        }));
        atlas_w = u1 + 1.0;
    }
    // Second pass to fix UVs now that atlas_w is known, and to blit pixels.
    let atlas_w = atlas_w.ceil() as usize;
    let atlas_h = cell_h.ceil() as usize;
    let mut bitmap = vec![0u8; atlas_w * atlas_h * 4];
    for (cell, (_ch, _adv, origin, dims, pixels)) in cells.iter_mut().zip(raw.iter()) {
        let Some(cell) = cell else { continue };
        if dims.0 <= 0 || dims.1 <= 0 {
            continue;
        }
        let bx = (cell.u0 * atlas_w as f32).round() as usize;
        let by = ((origin.1 - min_y).round() as usize).min(atlas_h - 1);
        for y in 0..dims.1 as usize {
            for x in 0..dims.0 as usize {
                let coverage = pixels[y * dims.0 as usize + x];
                let idx = ((by + y) * atlas_w + bx + x) * 4;
                if idx + 3 < bitmap.len() {
                    bitmap[idx] = 255;
                    bitmap[idx + 1] = 255;
                    bitmap[idx + 2] = 255;
                    bitmap[idx + 3] = (coverage * 255.0).clamp(0.0, 255.0) as u8;
                }
            }
        }
        cell.u0 /= atlas_w as f32;
        cell.u1 /= atlas_w as f32;
        cell.draw_dx = 0.0;
        cell.draw_dy = origin.1 - min_y;
        cell.draw_w = dims.0 as f32;
        cell.draw_h = dims.1 as f32;
    }
    let mut tex: GLuint = 0;
    gles.GenTextures(1, &mut tex);
    gles.BindTexture(gles11::TEXTURE_2D, tex);
    gles.TexImage2D(
        gles11::TEXTURE_2D,
        0,
        gles11::RGBA as _,
        atlas_w as _,
        atlas_h as _,
        0,
        gles11::RGBA,
        gles11::UNSIGNED_BYTE,
        bitmap.as_ptr() as *const _,
    );
    gles.TexParameteri(gles11::TEXTURE_2D, gles11::TEXTURE_MIN_FILTER, gles11::LINEAR as _);
    gles.TexParameteri(gles11::TEXTURE_2D, gles11::TEXTURE_MAG_FILTER, gles11::LINEAR as _);
    gles.TexParameteri(gles11::TEXTURE_2D, gles11::TEXTURE_WRAP_S, gles11::CLAMP_TO_EDGE as _);
    gles.TexParameteri(gles11::TEXTURE_2D, gles11::TEXTURE_WRAP_T, gles11::CLAMP_TO_EDGE as _);
    Some(Atlas { tex, height: cell_h, glyphs: cells })
}

unsafe fn ensure_atlas(gles: &mut dyn GLES) -> Option<()> {
    let lock = ATLAS.get_or_init(|| Mutex::new(None)).lock().unwrap();
    if lock.is_some() {
        return Some(());
    }
    drop(lock);
    let mut guard = ATLAS.get_or_init(|| Mutex::new(None)).lock().unwrap();
    if guard.is_none() {
        *guard = build_atlas(gles);
    }
    guard.is_some().then_some(())
}

/// Draw a solid rectangle.
unsafe fn draw_rect(
    gles: &mut dyn GLES,
    rect: Rect,
    color: (f32, f32, f32, f32),
) {
    let (r, g, b, a) = color;
    gles.Color4f(r, g, b, a);
    let verts: [f32; 8] = [
        rect.x,
        rect.y,
        rect.x,
        rect.y + rect.h,
        rect.x + rect.w,
        rect.y,
        rect.x + rect.w,
        rect.y + rect.h,
    ];
    gles.VertexPointer(2, gles11::FLOAT, 0, verts.as_ptr() as *const _);
    gles.Disable(gles11::TEXTURE_2D);
    gles.DrawArrays(gles11::TRIANGLE_STRIP, 0, 4);
}

/// Draw text with the cached atlas, top-left at (x, y), pixel height
/// `px_size`. Returns the width of the drawn text.
unsafe fn draw_text(
    gles: &mut dyn GLES,
    atlas: &Atlas,
    text: &str,
    x: f32,
    y: f32,
    px_size: f32,
    color: (f32, f32, f32, f32),
) -> f32 {
    let scale = px_size / FONT_PX;
    let (r, g, b, a) = color;
    gles.Color4f(r, g, b, a);
    gles.Enable(gles11::TEXTURE_2D);
    gles.BindTexture(gles11::TEXTURE_2D, atlas.tex);
    let mut cursor = x;
    for ch in text.chars() {
        let Some(idx) = char_index(ch) else {
            cursor += px_size * 0.3;
            continue;
        };
        let Some(cell) = &atlas.glyphs[idx] else { continue; };
        let gx = cursor + cell.draw_dx * scale;
        let gy = y + cell.draw_dy * scale;
        let gw = cell.draw_w * scale;
        let gh = cell.draw_h * scale;
        if gw > 0.0 && gh > 0.0 {
            let verts: [f32; 8] = [gx, gy, gx, gy + gh, gx + gw, gy, gx + gw, gy + gh];
            let uv: [f32; 8] = [
                cell.u0, cell.v0, cell.u0, cell.v1, cell.u1, cell.v0, cell.u1, cell.v1,
            ];
            gles.VertexPointer(2, gles11::FLOAT, 0, verts.as_ptr() as *const _);
            gles.TexCoordPointer(2, gles11::FLOAT, 0, uv.as_ptr() as *const _);
            gles.DrawArrays(gles11::TRIANGLE_STRIP, 0, 4);
        }
        cursor += cell.advance * scale;
    }
    cursor - x
}

const COL_PANEL: (f32, f32, f32, f32) = (0.05, 0.05, 0.06, 0.88);
const COL_WIDGET: (f32, f32, f32, f32) = (0.22, 0.22, 0.24, 0.95);
const COL_WIDGET_LIT: (f32, f32, f32, f32) = (0.0, 0.5, 0.25, 0.95);
const COL_FIELD: (f32, f32, f32, f32) = (0.12, 0.12, 0.14, 0.95);
const COL_TEXT: (f32, f32, f32, f32) = (0.95, 0.95, 0.95, 1.0);
const COL_TEXT_DIM: (f32, f32, f32, f32) = (0.6, 0.6, 0.62, 1.0);
const COL_ACCENT: (f32, f32, f32, f32) = (0.0, 0.75, 0.35, 0.92);
const COL_SELECTED: (f32, f32, f32, f32) = (0.15, 0.3, 0.6, 0.95);

/// Entry point called from present_frame, in viewport pixel space.
pub unsafe fn draw(gles: &mut dyn GLES, viewport: (u32, u32, u32, u32)) {
    if !HARDWARE_ENABLED.load(Ordering::SeqCst) {
        return;
    }
    let mut ui_state = UI.lock().unwrap();
    if !ui_state.enabled || ui_state.app_id.is_none() {
        return;
    }
    if ensure_atlas(gles).is_none() {
        return;
    }
    let atlas_guard = ATLAS.get().unwrap().lock().unwrap();
    let Some(atlas) = atlas_guard.as_ref() else { return; };

    let (vx, vy, vw, vh) = viewport;
    let (vx, vy, vw, vh) = (vx as f32, vy as f32, vw as f32, vh as f32);

    // Save state (mirrors draw_onscreen_text).
    let mut old_active_texture: GLint = 0;
    gles.GetIntegerv(gles11::ACTIVE_TEXTURE, &mut old_active_texture);
    let mut old_texture: GLint = 0;
    gles.GetIntegerv(gles11::TEXTURE_BINDING_2D, &mut old_texture);

    gles.MatrixMode(gles11::PROJECTION);
    gles.PushMatrix();
    gles.LoadIdentity();
    gles.Orthof(0.0, vw, vh, 0.0, -1.0, 1.0);
    gles.MatrixMode(gles11::MODELVIEW);
    gles.PushMatrix();
    gles.LoadIdentity();

    gles.EnableClientState(gles11::VERTEX_ARRAY);
    gles.EnableClientState(gles11::TEXTURE_COORD_ARRAY);
    gles.Enable(gles11::BLEND);
    gles.BlendFunc(gles11::SRC_ALPHA, gles11::ONE_MINUS_SRC_ALPHA);

    let layout = compute_layout(&ui_state, viewport);

    // Floating button.
    let lit = ui_state.pending == Some(W_BUTTON) || ui_state.open;
    draw_rect(gles, layout.button, if lit { COL_WIDGET_LIT } else { COL_ACCENT });
    let bs = layout.scale;
    let label = if ui_state.open { "X" } else { "GG" };
    let tw = text_width(atlas, label, 12.0 * bs);
    draw_text(
        gles,
        atlas,
        label,
        layout.button.x + (layout.button.w - tw) / 2.0,
        layout.button.y + (layout.button.h - atlas.height * (12.0 * bs / FONT_PX)) / 2.0,
        12.0 * bs,
        COL_TEXT,
    );

    if let Some(panel) = &layout.panel {
        draw_rect(gles, panel.rect, COL_PANEL);

        // Header.
        let title = "GameGuardian";
        draw_text(
            gles,
            atlas,
            title,
            panel.rect.x + 8.0 * bs,
            panel.rect.y + 7.0 * bs,
            12.0 * bs,
            COL_ACCENT,
        );

        // Widget pass: draw every widget by id.
        for (id, rect) in &panel.widgets {
            match *id {
                W_CLOSE => {
                    draw_rect(gles, *rect, COL_WIDGET);
                    let tw = text_width(atlas, "X", 12.0 * bs);
                    draw_text(
                        gles,
                        atlas,
                        "X",
                        rect.x + (rect.w - tw) / 2.0,
                        rect.y + (rect.h - atlas.height * (12.0 * bs / FONT_PX)) / 2.0,
                        12.0 * bs,
                        COL_TEXT,
                    );
                }
                W_TYPE => {
                    draw_rect(gles, *rect, COL_WIDGET);
                    let label = format!("TYPE: {}", ui_state.vtype.name());
                    draw_text(gles, atlas, &label, rect.x + 6.0 * bs, rect.y + 5.0 * bs, 12.0 * bs, COL_TEXT);
                }
                W_FIELD_SEARCH | W_FIELD_SET => {
                    let focus_here = (*id == W_FIELD_SEARCH && ui_state.focus == Focus::Search)
                        || (*id == W_FIELD_SET && ui_state.focus == Focus::Set);
                    draw_rect(gles, *rect, if focus_here { COL_SELECTED } else { COL_FIELD });
                    let (label, content) = if *id == W_FIELD_SEARCH {
                        ("VAL", &ui_state.search_text)
                    } else {
                        ("SET", &ui_state.set_text)
                    };
                    let text = format!("{}: {}_", label, content);
                    draw_text(gles, atlas, &text, rect.x + 6.0 * bs, rect.y + 5.0 * bs, 12.0 * bs, COL_TEXT);
                }
                id if (W_KEY_BASE..W_KEY_BASE + 16).contains(&id) => {
                    let key_idx = (id - W_KEY_BASE) as usize;
                    let row = key_idx / 4;
                    let col = key_idx % 4;
                    let ch = KEYPAD[row][col];
                    let label = match ch {
                        Some('\u{8}') => "DEL".to_string(),
                        Some('\u{4}') => "CLR".to_string(),
                        Some(c) => c.to_string(),
                        None => String::new(),
                    };
                    draw_rect(gles, *rect, COL_WIDGET);
                    if !label.is_empty() {
                        let size = 12.0 * bs;
                        let tw = text_width(atlas, &label, size);
                        draw_text(
                            gles,
                            atlas,
                            &label,
                            rect.x + (rect.w - tw) / 2.0,
                            rect.y + (rect.h - atlas.height * (size / FONT_PX)) / 2.0,
                            size,
                            COL_TEXT,
                        );
                    }
                }
                W_SEARCH | W_REFINE | W_RESET | W_SET | W_FREEZE | W_UNFREEZE | W_DUMP | W_SAVE => {
                    draw_rect(gles, *rect, COL_WIDGET);
                    let label: &str = match *id {
                        W_SEARCH => "SEARCH",
                        W_REFINE => "REFINE",
                        W_RESET => "RESET",
                        W_SET => "SET",
                        W_FREEZE => "FREEZE",
                        W_UNFREEZE => "UNFRZ",
                        W_DUMP => "DUMP",
                        W_SAVE => "SAVE HACK",
                        _ => "",
                    };
                    let size = 12.0 * bs;
                    let tw = text_width(atlas, label, size);
                    draw_text(
                        gles,
                        atlas,
                        label,
                        rect.x + (rect.w - tw) / 2.0,
                        rect.y + (rect.h - atlas.height * (size / FONT_PX)) / 2.0,
                        size,
                        COL_TEXT,
                    );
                }
                W_SCROLL_UP | W_SCROLL_DOWN => {
                    draw_rect(gles, *rect, COL_WIDGET);
                    let label: &str = if *id == W_SCROLL_UP { "^" } else { "v" };
                    let size = 12.0 * bs;
                    let tw = text_width(atlas, label, size);
                    draw_text(
                        gles,
                        atlas,
                        label,
                        rect.x + (rect.w - tw) / 2.0,
                        rect.y + (rect.h - atlas.height * (size / FONT_PX)) / 2.0,
                        size,
                        COL_TEXT,
                    );
                }
                id if (W_RESULT_BASE..W_RESULT_BASE + RESULT_ROWS as u16).contains(&id) => {
                    let row_idx = (id - W_RESULT_BASE) as usize;
                    let abs_idx = ui_state.scroll + row_idx;
                    if let Some(result) = ui_state.results.get(abs_idx) {
                        let selected = ui_state.selected == Some(result.addr);
                        draw_rect(gles, *rect, if selected { COL_SELECTED } else { COL_FIELD });
                        let text = format!(
                            "0x{:08X} {}",
                            result.addr,
                            ui_state.vtype.format(result.bits)
                        );
                        draw_text(gles, atlas, &text, rect.x + 6.0 * bs, rect.y + 3.0 * bs, 11.0 * bs, COL_TEXT);
                    }
                }
                _ => {}
            }
        }

        // Results header + counts, drawn between the header and the result rows.
        let res_label = format!(
            "RESULTS {} FROZEN {}",
            ui_state.total_results, ui_state.frozen_count
        );
        draw_text(
            gles,
            atlas,
            &res_label,
            panel.rect.x + 8.0 * bs,
            panel.rect.y + 4.0 * bs + 130.0 * bs,
            11.0 * bs,
            COL_TEXT_DIM,
        );

        // Status line at the bottom of the panel.
        if !ui_state.status.is_empty() {
            let status_y = panel.rect.y + panel.rect.h - 18.0 * bs;
            draw_text(
                gles,
                atlas,
                &ui_state.status,
                panel.rect.x + 8.0 * bs,
                status_y,
                11.0 * bs,
                COL_ACCENT,
            );
        }
    }

    // Restore state.
    gles.BindTexture(gles11::TEXTURE_2D, old_texture as _);
    gles.ActiveTexture(old_active_texture as _);
    gles.Disable(gles11::BLEND);
    gles.Disable(gles11::TEXTURE_2D);
    gles.DisableClientState(gles11::TEXTURE_COORD_ARRAY);
    gles.DisableClientState(gles11::VERTEX_ARRAY);

    gles.MatrixMode(gles11::MODELVIEW);
    gles.PopMatrix();
    gles.MatrixMode(gles11::PROJECTION);
    gles.PopMatrix();
    gles.MatrixMode(gles11::TEXTURE);
    gles.LoadIdentity();
}

fn text_width(atlas: &Atlas, text: &str, px_size: f32) -> f32 {
    let scale = px_size / FONT_PX;
    text.chars()
        .map(|ch| {
            char_index(ch)
                .and_then(|idx| atlas.glyphs[idx].as_ref())
                .map_or(px_size * 0.3, |cell| cell.advance * scale)
        })
        .sum()
}
