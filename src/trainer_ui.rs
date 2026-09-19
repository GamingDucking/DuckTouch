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
    // Coordinates are VIEWPORT-RELATIVE: the GL viewport already offsets
    // drawing by (vx, vy), and the touch handlers subtract it before
    // hit-testing. Adding the origin here would double it (panel shifted
    // by the letterbox offset, tap targets misaligned with drawn keys).
    let (_vx, _vy, vw, vh) = viewport;
    let (vx, vy, vw, vh) = (0.0_f32, 0.0_f32, vw as f32, vh as f32);
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
    let (vx, vy, _, _) = viewport;
    let (x, y) = (abs.0 - vx as f32, abs.1 - vy as f32);
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
    let (vx, vy, _, _) = viewport;
    let (x, y) = (abs.0 - vx as f32, abs.1 - vy as f32);
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
    // Pixel-space U range of each glyph inside the atlas row. Converted to
    // 0..1 UVs once the final atlas width is known (dividing by the running
    // width here AND by the final width below would corrupt the coordinates).
    let mut u_ranges_px: Vec<(f32, f32)> = Vec::with_capacity(ATLAS_CHARS.len());
    let mut cells: Vec<Option<GlyphCell>> = Vec::with_capacity(ATLAS_CHARS.len());
    for (_ch, advance, _origin, dims, _pixels) in &raw {
        let gw = if dims.0 > 0 { dims.0 as f32 } else { 0.0 };
        let u0 = atlas_w + 1.0; // 1px padding to avoid bleeding
        let u1 = u0 + gw;
        u_ranges_px.push((u0, u1));
        cells.push(Some(GlyphCell {
            u0: 0.0,
            v0: 0.0,
            u1: 0.0,
            v1: 0.0,
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
    for (i, (cell, (_ch, _adv, origin, dims, pixels))) in
        cells.iter_mut().zip(raw.iter()).enumerate()
    {
        let Some(cell) = cell else { continue };
        if dims.0 <= 0 || dims.1 <= 0 {
            continue;
        }
        let (u0_px, u1_px) = u_ranges_px[i];
        let bx = u0_px.round() as usize;
        // Vertical placement of this glyph's bitmap inside the atlas.
        let by = ((origin.1 - min_y).round() as usize).min(atlas_h.saturating_sub(1));
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
        // Texture coordinates. GL textures have their origin at the BOTTOM
        // left (the first byte of the uploaded data is v=0), while our bitmap
        // has row 0 at the top — so v grows downwards through the bitmap as
        // the coordinate DECREASES from 1.
        let atlas_h_f = atlas_h as f32;
        let atlas_w_f = atlas_w as f32;
        cell.u0 = u0_px / atlas_w_f;
        cell.u1 = u1_px / atlas_w_f;
        // v0 = top of the glyph band, v1 = bottom (matches the quad corners
        // in push_text, where v0 is used at the top edge).
        // The first row uploaded (bitmap row 0, the glyph band's top) is
        // sampled at v = 0, so v simply grows downward through the bitmap:
        // no `1 -` inversion here — that would flip every glyph upside down.
        cell.v0 = by as f32 / atlas_h_f;
        cell.v1 = (by as f32 + dims.1 as f32) / atlas_h_f;
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
/// One drawable rectangle, positioned in viewport pixel space. `tex == 0`
/// means a solid quad (no texture); otherwise the atlas texture is sampled.
#[derive(Clone, Copy)]
struct Quad {
    tex: GLuint,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    u0: f32,
    v0: f32,
    u1: f32,
    v1: f32,
    col: (f32, f32, f32, f32),
}

fn quad_vertices(q: &Quad) -> [f32; 16] {
    let (x, y, w, h) = (q.x, q.y, q.w, q.h);
    let (u0, v0, u1, v1) = (q.u0, q.v0, q.u1, q.v1);
    // Interleaved pos(2) + uv(2), triangle strip: TL, BL, TR, BR.
    [
        x, y, u0, v0,
        x, y + h, u0, v1,
        x + w, y, u1, v0,
        x + w, y + h, u1, v1,
    ]
}

/// Push a solid rectangle onto the scene.
fn push_rect(quads: &mut Vec<Quad>, rect: Rect, color: (f32, f32, f32, f32)) {
    quads.push(Quad {
        tex: 0,
        x: rect.x,
        y: rect.y,
        w: rect.w,
        h: rect.h,
        u0: 0.0,
        v0: 0.0,
        u1: 1.0,
        v1: 1.0,
        col: color,
    });
}

/// Draw text with the cached atlas, top-left at (x, y), pixel height
/// `px_size`. Returns the width of the drawn text.
/// Push text (as atlas glyph quads) onto the scene, top-left at (x, y),
/// pixel height `px_size`. Returns the width of the text.
fn push_text(
    quads: &mut Vec<Quad>,
    atlas: &Atlas,
    text: &str,
    x: f32,
    y: f32,
    px_size: f32,
    color: (f32, f32, f32, f32),
) -> f32 {
    let scale = px_size / FONT_PX;
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
            quads.push(Quad {
                tex: atlas.tex,
                x: gx,
                y: gy,
                w: gw,
                h: gh,
                u0: cell.u0,
                v0: cell.v0,
                u1: cell.u1,
                v1: cell.v1,
                col: color,
            });
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

/// Entry point called from present_frame (CA composition and native ES 1.1
/// present paths), in viewport pixel space. Renders with GLES 1.x
/// fixed-function calls.
pub unsafe fn draw(gles: &mut dyn GLES, viewport: (u32, u32, u32, u32)) {
    let Some(quads) = build_scene(gles, viewport) else {
        return;
    };
    render_gles1(gles, viewport, &quads);
}

/// Entry point for native OpenGL ES 2.0 present paths (see
/// present_renderbuffer_es2), which lack the fixed-function pipeline.
pub unsafe fn draw_es2(gles: &mut dyn GLES, viewport: (u32, u32, u32, u32)) {
    let Some(quads) = build_scene(gles, viewport) else {
        return;
    };
    render_es2(gles, viewport, &quads);
}

/// Build the overlay scene (shared by both renderers). Returns None when the
/// overlay is disabled or the glyph atlas is unavailable.
unsafe fn build_scene(
    gles: &mut dyn GLES,
    viewport: (u32, u32, u32, u32),
) -> Option<Vec<Quad>> {
    if !HARDWARE_ENABLED.load(Ordering::SeqCst) {
        return None;
    }
    let mut ui_state = UI.lock().unwrap();
    if !ui_state.enabled || ui_state.app_id.is_none() {
        return None;
    }
    if ensure_atlas(gles).is_none() {
        return None;
    }
    let atlas_guard = ATLAS.get().unwrap().lock().unwrap();
    let Some(atlas) = atlas_guard.as_ref() else { return None; };

    let mut quads: Vec<Quad> = Vec::new();

    let layout = compute_layout(&ui_state, viewport);

    // Floating button.
    let lit = ui_state.pending == Some(W_BUTTON) || ui_state.open;
    push_rect(&mut quads, layout.button, if lit { COL_WIDGET_LIT } else { COL_ACCENT });
    let bs = layout.scale;
    let label = if ui_state.open { "X" } else { "GG" };
    let tw = text_width(atlas, label, 12.0 * bs);
    push_text(&mut quads,
        atlas,
        label,
        layout.button.x + (layout.button.w - tw) / 2.0,
        layout.button.y + (layout.button.h - atlas.height * (12.0 * bs / FONT_PX)) / 2.0,
        12.0 * bs,
        COL_TEXT,
    );

    if let Some(panel) = &layout.panel {
        push_rect(&mut quads, panel.rect, COL_PANEL);

        // Header.
        let title = "GameGuardian";
        push_text(&mut quads,
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
                    push_rect(&mut quads, *rect, COL_WIDGET);
                    let tw = text_width(atlas, "X", 12.0 * bs);
                    push_text(&mut quads,
                        atlas,
                        "X",
                        rect.x + (rect.w - tw) / 2.0,
                        rect.y + (rect.h - atlas.height * (12.0 * bs / FONT_PX)) / 2.0,
                        12.0 * bs,
                        COL_TEXT,
                    );
                }
                W_TYPE => {
                    push_rect(&mut quads, *rect, COL_WIDGET);
                    let label = format!("TYPE: {}", ui_state.vtype.name());
                    push_text(&mut quads, atlas, &label, rect.x + 6.0 * bs, rect.y + 5.0 * bs, 12.0 * bs, COL_TEXT);
                }
                W_FIELD_SEARCH | W_FIELD_SET => {
                    let focus_here = (*id == W_FIELD_SEARCH && ui_state.focus == Focus::Search)
                        || (*id == W_FIELD_SET && ui_state.focus == Focus::Set);
                    push_rect(&mut quads, *rect, if focus_here { COL_SELECTED } else { COL_FIELD });
                    let (label, content) = if *id == W_FIELD_SEARCH {
                        ("VAL", &ui_state.search_text)
                    } else {
                        ("SET", &ui_state.set_text)
                    };
                    let text = format!("{}: {}_", label, content);
                    push_text(&mut quads, atlas, &text, rect.x + 6.0 * bs, rect.y + 5.0 * bs, 12.0 * bs, COL_TEXT);
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
                    push_rect(&mut quads, *rect, COL_WIDGET);
                    if !label.is_empty() {
                        let size = 12.0 * bs;
                        let tw = text_width(atlas, &label, size);
                        push_text(&mut quads,
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
                    push_rect(&mut quads, *rect, COL_WIDGET);
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
                    push_text(&mut quads,
                        atlas,
                        label,
                        rect.x + (rect.w - tw) / 2.0,
                        rect.y + (rect.h - atlas.height * (size / FONT_PX)) / 2.0,
                        size,
                        COL_TEXT,
                    );
                }
                W_SCROLL_UP | W_SCROLL_DOWN => {
                    push_rect(&mut quads, *rect, COL_WIDGET);
                    let label: &str = if *id == W_SCROLL_UP { "^" } else { "v" };
                    let size = 12.0 * bs;
                    let tw = text_width(atlas, label, size);
                    push_text(&mut quads,
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
                        push_rect(&mut quads, *rect, if selected { COL_SELECTED } else { COL_FIELD });
                        let text = format!(
                            "0x{:08X} {}",
                            result.addr,
                            ui_state.vtype.format(result.bits)
                        );
                        push_text(&mut quads, atlas, &text, rect.x + 6.0 * bs, rect.y + 3.0 * bs, 11.0 * bs, COL_TEXT);
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
        push_text(&mut quads,
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
            push_text(&mut quads,
                atlas,
                &ui_state.status,
                panel.rect.x + 8.0 * bs,
                status_y,
                11.0 * bs,
                COL_ACCENT,
            );
        }
    }

    Some(quads)
}

/// Render the scene with GLES 1.x fixed-function calls.
unsafe fn render_gles1(gles: &mut dyn GLES, viewport: (u32, u32, u32, u32), quads: &[Quad]) {
    // Coordinates are VIEWPORT-RELATIVE: the GL viewport already offsets
    // drawing by (vx, vy), and the touch handlers subtract it before
    // hit-testing. Adding the origin here would double it (panel shifted
    // by the letterbox offset, tap targets misaligned with drawn keys).
    let (_vx, _vy, vw, vh) = viewport;
    let (vx, vy, vw, vh) = (0.0_f32, 0.0_f32, vw as f32, vh as f32);

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

    let mut bound_tex: GLuint = 0;
    for q in quads {
        let (r, g, b, a) = q.col;
        gles.Color4f(r, g, b, a);
        if q.tex == 0 {
            if bound_tex != 0 {
                gles.Disable(gles11::TEXTURE_2D);
                bound_tex = 0;
            }
        } else {
            if bound_tex == 0 {
                gles.Enable(gles11::TEXTURE_2D);
            }
            if bound_tex != q.tex {
                gles.BindTexture(gles11::TEXTURE_2D, q.tex);
                bound_tex = q.tex;
            }
        }
        let verts = quad_vertices(q);
        gles.VertexPointer(2, gles11::FLOAT, 16, verts.as_ptr() as *const _);
        gles.TexCoordPointer(
            2,
            gles11::FLOAT,
            16,
            verts.as_ptr().add(2) as *const _,
        );
        gles.DrawArrays(gles11::TRIANGLE_STRIP, 0, 4);
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

// ---------------------------------------------------------------------------
// Native OpenGL ES 2.0 rendering. Native ES 2.0 drivers (Android etc.) have
// no fixed-function pipeline, so present_renderbuffer_es2 calls draw_es2()
// and the scene is drawn with a small dedicated shader program instead.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct OverlayProgram {
    program: GLuint,
    a_pos: GLint,
    a_uv: GLint,
    a_col: GLint,
    u_viewport: GLint,
    u_tex: GLint,
}

static OVERLAY_PROGRAM: Mutex<Option<OverlayProgram>> = Mutex::new(None);
static OVERLAY_VBO: Mutex<Option<GLuint>> = Mutex::new(None);
static OVERLAY_WHITE_TEX: Mutex<Option<GLuint>> = Mutex::new(None);

const OVERLAY_VS_SRC: &[u8] = b"\
    attribute vec2 aPos;\n\
    attribute vec2 aUV;\n\
    attribute vec4 aCol;\n\
    uniform vec2 uViewport;\n\
    varying vec2 vUV;\n\
    varying vec4 vCol;\n\
    void main() {\n\
        vec2 ndc = vec2(aPos.x / uViewport.x * 2.0 - 1.0, 1.0 - aPos.y / uViewport.y * 2.0);\n\
        gl_Position = vec4(ndc, 0.0, 1.0);\n\
        vUV = aUV;\n\
        vCol = aCol;\n\
    }\n\0";

const OVERLAY_FS_SRC: &[u8] = b"\
    precision mediump float;\n\
    varying vec2 vUV;\n\
    varying vec4 vCol;\n\
    uniform sampler2D uTex;\n\
    void main() {\n\
        gl_FragColor = texture2D(uTex, vUV) * vCol;\n\
    }\n\0";

unsafe fn ensure_overlay_program(gles: &mut dyn GLES) -> Option<OverlayProgram> {
    use crate::gles::gles2_raw as gles2;
    {
        let guard = OVERLAY_PROGRAM.lock().unwrap();
        if let Some(p) = *guard {
            return Some(p);
        }
    }
    let mut guard = OVERLAY_PROGRAM.lock().unwrap();
    if let Some(p) = guard.as_ref() {
        return Some(OverlayProgram { ..*p });
    }

    let vs_src = OVERLAY_VS_SRC;
    let fs_src = OVERLAY_FS_SRC;

    let vs = gles.CreateShader(gles2::VERTEX_SHADER);
    let vs_ptr = vs_src.as_ptr() as *const _;
    let vs_len = (vs_src.len() - 1) as GLint;
    gles.ShaderSource(vs, 1, &vs_ptr, &vs_len);
    gles.CompileShader(vs);
    let mut ok: GLint = 0;
    gles.GetShaderiv(vs, gles2::COMPILE_STATUS, &mut ok);
    if ok == 0 {
        log!("Warning: trainer overlay: vertex shader failed to compile.");
        gles.DeleteShader(vs);
        return None;
    }

    let fs = gles.CreateShader(gles2::FRAGMENT_SHADER);
    let fs_ptr = fs_src.as_ptr() as *const _;
    let fs_len = (fs_src.len() - 1) as GLint;
    gles.ShaderSource(fs, 1, &fs_ptr, &fs_len);
    gles.CompileShader(fs);
    let mut ok: GLint = 0;
    gles.GetShaderiv(fs, gles2::COMPILE_STATUS, &mut ok);
    if ok == 0 {
        log!("Warning: trainer overlay: fragment shader failed to compile.");
        gles.DeleteShader(vs);
        gles.DeleteShader(fs);
        return None;
    }

    let program = gles.CreateProgram();
    gles.AttachShader(program, vs);
    gles.AttachShader(program, fs);
    gles.LinkProgram(program);
    gles.DeleteShader(vs);
    gles.DeleteShader(fs);
    let mut ok: GLint = 0;
    gles.GetProgramiv(program, gles2::LINK_STATUS, &mut ok);
    if ok == 0 {
        log!("Warning: trainer overlay: program failed to link.");
        return None;
    }

    let p = OverlayProgram {
        program,
        a_pos: gles.GetAttribLocation(program, b"aPos\0".as_ptr() as *const _),
        a_uv: gles.GetAttribLocation(program, b"aUV\0".as_ptr() as *const _),
        a_col: gles.GetAttribLocation(program, b"aCol\0".as_ptr() as *const _),
        u_viewport: gles.GetUniformLocation(program, b"uViewport\0".as_ptr() as *const _),
        u_tex: gles.GetUniformLocation(program, b"uTex\0".as_ptr() as *const _),
    };
    *guard = Some(p);
    Some(p)
}

unsafe fn ensure_overlay_vbo(gles: &mut dyn GLES) -> GLuint {
    use crate::gles::gles2_raw as gles2;
    let mut guard = OVERLAY_VBO.lock().unwrap();
    if let Some(vbo) = *guard {
        return vbo;
    }
    let mut vbo: GLuint = 0;
    gles.GenBuffers(1, &mut vbo);
    *guard = Some(vbo);
    vbo
}

unsafe fn ensure_overlay_white_tex(gles: &mut dyn GLES) -> GLuint {
    use crate::gles::gles2_raw as gles2;
    {
        let guard = OVERLAY_WHITE_TEX.lock().unwrap();
        if let Some(tex) = *guard {
            return tex;
        }
    }
    let mut guard = OVERLAY_WHITE_TEX.lock().unwrap();
    if let Some(tex) = *guard {
        return tex;
    }
    let mut tex: GLuint = 0;
    gles.GenTextures(1, &mut tex);
    gles.BindTexture(gles2::TEXTURE_2D, tex);
    let white: [u8; 4] = [255, 255, 255, 255];
    gles.TexImage2D(
        gles2::TEXTURE_2D,
        0,
        gles2::RGBA as GLint,
        1,
        1,
        0,
        gles2::RGBA,
        gles2::UNSIGNED_BYTE,
        white.as_ptr() as *const _,
    );
    gles.TexParameteri(gles2::TEXTURE_2D, gles2::TEXTURE_MIN_FILTER, gles2::NEAREST as _);
    gles.TexParameteri(gles2::TEXTURE_2D, gles2::TEXTURE_MAG_FILTER, gles2::NEAREST as _);
    gles.TexParameteri(gles2::TEXTURE_2D, gles2::TEXTURE_WRAP_S, gles2::CLAMP_TO_EDGE as _);
    gles.TexParameteri(gles2::TEXTURE_2D, gles2::TEXTURE_WRAP_T, gles2::CLAMP_TO_EDGE as _);
    *guard = Some(tex);
    tex
}

/// Render the scene with a small ES 2.0 shader program. Saves and restores
/// the state it touches; the caller (present_renderbuffer_es2) restores the
/// rest of the presenter state afterwards.
unsafe fn render_es2(gles: &mut dyn GLES, viewport: (u32, u32, u32, u32), quads: &[Quad]) {
    use crate::gles::gles2_raw as gles2;

    if quads.is_empty() {
        return;
    }
    let Some(program) = ensure_overlay_program(gles) else {
        return;
    };
    if program.a_pos < 0 || program.a_uv < 0 || program.a_col < 0 {
        return;
    }
    let vbo = ensure_overlay_vbo(gles);
    let white = ensure_overlay_white_tex(gles);

    // Save state we touch.
    let mut old_program: GLint = 0;
    gles.GetIntegerv(gles2::CURRENT_PROGRAM, &mut old_program);
    let mut old_array_buffer: GLint = 0;
    gles.GetIntegerv(gles2::ARRAY_BUFFER_BINDING, &mut old_array_buffer);
    let mut old_active_texture: GLint = 0;
    gles.GetIntegerv(gles2::ACTIVE_TEXTURE, &mut old_active_texture);
    gles.ActiveTexture(gles2::TEXTURE0);
    let mut old_tex0: GLint = 0;
    gles.GetIntegerv(gles2::TEXTURE_BINDING_2D, &mut old_tex0);
    let blend_was_on = gles.IsEnabled(gles2::BLEND) != 0;
    let attribs = [program.a_pos as GLuint, program.a_uv as GLuint, program.a_col as GLuint];
    let mut attrib_states = [0u8; 3];
    for (slot, &attrib) in attrib_states.iter_mut().zip(attribs.iter()) {
        let mut v: GLint = 0;
        gles.GetVertexAttribiv(attrib as GLuint, gles2::VERTEX_ATTRIB_ARRAY_ENABLED, &mut v);
        *slot = v as u8;
    }

    gles.UseProgram(program.program);
    gles.Uniform2f(program.u_viewport, viewport.2 as f32, viewport.3 as f32);
    gles.Uniform1i(program.u_tex, 0);
    gles.Enable(gles2::BLEND);
    gles.BlendFunc(gles2::SRC_ALPHA, gles2::ONE_MINUS_SRC_ALPHA);

    gles.BindBuffer(gles2::ARRAY_BUFFER, vbo);
    for &attr in &attribs {
        gles.EnableVertexAttribArray(attr as _);
    }
    let stride = 8 * 4;
    gles.VertexAttribPointer(
        program.a_pos as _,
        2,
        gles2::FLOAT,
        gles2::FALSE,
        stride,
        0usize as *const _,
    );
    gles.VertexAttribPointer(
        program.a_uv as _,
        2,
        gles2::FLOAT,
        gles2::FALSE,
        stride,
        8usize as *const _,
    );
    gles.VertexAttribPointer(
        program.a_col as _,
        4,
        gles2::FLOAT,
        gles2::FALSE,
        stride,
        16usize as *const _,
    );

    for q in quads {
        let (r, g, b, a) = q.col;
        // Interleaved pos(2) uv(2) col(4), triangle strip TL, BL, TR, BR.
        let mut data = [0.0f32; 4 * 8];
        let corners = [
            (q.x, q.y, q.u0, q.v0),
            (q.x, q.y + q.h, q.u0, q.v1),
            (q.x + q.w, q.y, q.u1, q.v0),
            (q.x + q.w, q.y + q.h, q.u1, q.v1),
        ];
        for (i, (px, py, u, v)) in corners.iter().enumerate() {
            data[i * 8] = *px;
            data[i * 8 + 1] = *py;
            data[i * 8 + 2] = *u;
            data[i * 8 + 3] = *v;
            data[i * 8 + 4] = r;
            data[i * 8 + 5] = g;
            data[i * 8 + 6] = b;
            data[i * 8 + 7] = a;
        }
        let tex = if q.tex == 0 { white } else { q.tex };
        gles.BindTexture(gles2::TEXTURE_2D, tex);
        gles.BufferData(
            gles2::ARRAY_BUFFER,
            (data.len() * 4) as _,
            data.as_ptr() as *const _,
            gles2::DYNAMIC_DRAW,
        );
        gles.DrawArrays(gles2::TRIANGLE_STRIP, 0, 4);
    }

    for (&attr, &was) in attribs.iter().zip(attrib_states.iter()) {
        if was != 0 {
            gles.EnableVertexAttribArray(attr as _);
        } else {
            gles.DisableVertexAttribArray(attr as _);
        }
    }
    if !blend_was_on {
        gles.Disable(gles2::BLEND);
    }
    gles.BindTexture(gles2::TEXTURE_2D, old_tex0 as _);
    gles.ActiveTexture(old_active_texture as _);
    gles.BindBuffer(gles2::ARRAY_BUFFER, old_array_buffer as _);
    gles.UseProgram(old_program as _);
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
