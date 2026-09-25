//! Debug test for the trainer overlay text atlas: reproduces the exact
//! glyph-capture + blit logic from src/trainer_ui.rs and prints ASCII art so
//! mirroring/orientation bugs are visible in the test output.

use touchHLE::font::{Font, TextAlignment};

const FONT_PX: f32 = 14.0;

#[test]
fn atlas_orientation_ascii_art() {
    let bytes = std::fs::read("touchHLE_fonts/LiberationMono-Regular.ttf").unwrap();
    let font = Font::from_vec(bytes).unwrap();
    let units_per_em = font.units_per_em() as f32;

    let sample = "SEARCH";
    let mut raw: Vec<(char, f32, (f32, f32), (i32, i32), Vec<f32>)> = Vec::new();
    let mut min_y = f32::MAX;
    let mut max_y = f32::MIN;
    for ch in sample.chars() {
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
    assert!(min_y <= max_y, "no glyphs captured at all");

    let cell_h = max_y - min_y;
    let mut atlas_w = 0.0f32;
    let mut u_ranges_px: Vec<(f32, f32)> = Vec::new();
    let mut cells: Vec<Option<(f32, f32, f32, f32, f32, f32, f32, f32)>> = Vec::new(); // u0,v0,u1,v1,dx,dy,dw,dh
    for (_ch, advance, _origin, dims, _pixels) in &raw {
        let gw = if dims.0 > 0 { dims.0 as f32 } else { 0.0 };
        let u0 = atlas_w + 1.0;
        let u1 = u0 + gw;
        u_ranges_px.push((u0, u1));
        cells.push(Some((0.0, 0.0, 0.0, 0.0, 0.0, 0.0, gw, dims.1 as f32)));
        atlas_w = u1 + 1.0;
    }
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
        let atlas_h_f = atlas_h as f32;
        let atlas_w_f = atlas_w as f32;
        cell.0 = u0_px / atlas_w_f; // u0
        cell.2 = u1_px / atlas_w_f; // u1
        cell.1 = 1.0 - by as f32 / atlas_h_f; // v0 (top)
        cell.3 = 1.0 - (by as f32 + dims.1 as f32) / atlas_h_f; // v1 (bottom)
        cell.5 = origin.1 - min_y; // dy
    }

    // --- Dump the atlas bitmap itself as ASCII art ---
    println!("=== ATLAS BITMAP ({atlas_w}x{atlas_h}) ===");
    for y in 0..atlas_h {
        let mut row = String::new();
        for x in 0..atlas_w {
            let a = bitmap[(y * atlas_w + x) * 4 + 3];
            row.push(if a > 127 { '#' } else { '.' });
        }
        println!("{row}");
    }

    // --- Software-rasterize the render_es2 path for "SEARCH" ---
    println!("=== RENDERED \"SEARCH\" (ES2 math, 2x scale) ===");
    let scale_px = FONT_PX * 2.0;
    let mut canvas = vec![0u8; 40 * 220];
    let mut cursor = 2.0f32;
    for (idx, ch) in sample.chars().enumerate() {
        let Some(Some(cell)) = cells.get(idx) else { continue };
        let sc = scale_px / FONT_PX;
        let gx = cursor + cell.4 * sc;
        let gy = 2.0 + cell.5 * sc;
        let gw = cell.6 * sc;
        let gh = cell.7 * sc;
        if gw > 0.0 && gh > 0.0 {
            // Sample the quad like the shader would: for each screen pixel in
            // the quad, map to uv (bilinear over the quad corners) and fetch
            // the atlas texel.
            let x0 = gx.round() as usize;
            let y0 = gy.round() as usize;
            for py in 0..(gh.round() as usize) {
                for px in 0..(gw.round() as usize) {
                    // v interpolated top->bottom: v0 at top, v1 at bottom
                    let ty = py as f32 / gh;
                    let tx = px as f32 / gw;
                    let u = cell.0 + (cell.2 - cell.0) * tx;
                    let v = cell.1 + (cell.3 - cell.1) * ty;
                    // texture v -> bitmap row (first uploaded row = v 0)
                    let bmp_y = (v * atlas_h as f32) as isize;
                    let bmp_x = (u * atlas_w as f32) as isize;
                    if bmp_x >= 0 && bmp_y >= 0 {
                        let bx_u = bmp_x as usize;
                        let by_u = bmp_y as usize;
                        if bx_u < atlas_w && by_u < atlas_h {
                            let a = bitmap[(by_u * atlas_w + bx_u) * 4 + 3];
                            let cx = x0 + px;
                            let cy = y0 + py;
                            if cx < 220 && cy < 40 {
                                canvas[cy * 220 + cx] = if a > 127 { b'#' } else { b'.' };
                            }
                        }
                    }
                }
            }
        }
        cursor += cell.6 * sc + 2.0; // advance approx: scaled width + gap
    }
    for y in 0..40 {
        let row: String = canvas[y * 220..(y + 1) * 220]
            .iter()
            .map(|&c| c as char)
            .collect();
        if row.contains('#') {
            println!("{row}");
        }
    }

    // --- Hard orientation assertions on the raw glyph bitmaps ---
    // 'L': vertical stem on the LEFT -> ink at bottom-left, empty top-right.
    for (ch, _adv, _origin, dims, pixels) in &raw {
        if *ch == 'L' && dims.0 > 4 && dims.1 > 4 {
            let bl = pixels[(dims.1 as usize - 1) * dims.0 as usize];
            let tr = pixels[dims.0 as usize - 1];
            assert!(
                bl > 0.5,
                "'L' bottom-left should have ink (stem); got {bl}"
            );
            assert!(
                tr < 0.5,
                "'L' top-right should be empty; got {tr} — glyph bitmaps are mirrored!"
            );
        }
    }
    println!("orientation checks passed");
}
