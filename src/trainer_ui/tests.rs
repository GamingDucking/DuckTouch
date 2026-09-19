/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

use super::*;
use crate::gles::{GLenum, GLfloat, GLsizei, GLvoid};

#[derive(Debug, PartialEq)]
struct Draw {
    texture: Option<GLuint>,
    color: (f32, f32, f32, f32),
}

/// Models the state left by present_frame, without needing a window or GPU.
struct RecordingGles {
    textured: bool,
    texture: GLuint,
    tex_env_mode: GLint,
    color: (f32, f32, f32, f32),
    draws: Vec<Draw>,
}

#[allow(non_snake_case)]
impl GLES for RecordingGles {
    unsafe fn GetIntegerv(&mut self, pname: GLenum, params: *mut GLint) {
        *params = match pname {
            gles11::ACTIVE_TEXTURE => gles11::TEXTURE0 as GLint,
            gles11::TEXTURE_BINDING_2D => self.texture as GLint,
            _ => panic!("unexpected state query: {pname:#x}"),
        };
    }

    unsafe fn GetTexEnviv(&mut self, target: GLenum, pname: GLenum, params: *mut GLint) {
        assert_eq!(target, gles11::TEXTURE_ENV);
        assert_eq!(pname, gles11::TEXTURE_ENV_MODE);
        *params = self.tex_env_mode;
    }

    unsafe fn TexEnviv(&mut self, target: GLenum, pname: GLenum, params: *const GLint) {
        assert_eq!(target, gles11::TEXTURE_ENV);
        assert_eq!(pname, gles11::TEXTURE_ENV_MODE);
        self.tex_env_mode = *params;
    }

    unsafe fn ActiveTexture(&mut self, texture: GLenum) {
        assert_eq!(texture, gles11::TEXTURE0);
    }

    unsafe fn BindTexture(&mut self, target: GLenum, texture: GLuint) {
        assert_eq!(target, gles11::TEXTURE_2D);
        self.texture = texture;
    }

    unsafe fn Enable(&mut self, cap: GLenum) {
        match cap {
            gles11::TEXTURE_2D => self.textured = true,
            gles11::BLEND => (),
            _ => panic!("unexpected capability: {cap:#x}"),
        }
    }

    unsafe fn Disable(&mut self, cap: GLenum) {
        match cap {
            gles11::TEXTURE_2D => self.textured = false,
            gles11::BLEND => (),
            _ => panic!("unexpected capability: {cap:#x}"),
        }
    }

    unsafe fn Color4f(&mut self, r: GLfloat, g: GLfloat, b: GLfloat, a: GLfloat) {
        self.color = (r, g, b, a);
    }

    unsafe fn DrawArrays(&mut self, mode: GLenum, first: GLint, count: GLsizei) {
        assert_eq!((mode, first, count), (gles11::TRIANGLE_STRIP, 0, 4));
        if self.textured {
            assert_eq!(self.tex_env_mode, gles11::MODULATE as GLint);
        }
        self.draws.push(Draw {
            texture: self.textured.then_some(self.texture),
            color: self.color,
        });
    }

    unsafe fn MatrixMode(&mut self, _mode: GLenum) {}
    unsafe fn PushMatrix(&mut self) {}
    unsafe fn PopMatrix(&mut self) {}
    unsafe fn LoadIdentity(&mut self) {}
    unsafe fn Orthof(
        &mut self,
        _left: GLfloat,
        _right: GLfloat,
        _bottom: GLfloat,
        _top: GLfloat,
        _near: GLfloat,
        _far: GLfloat,
    ) {
    }
    unsafe fn EnableClientState(&mut self, _array: GLenum) {}
    unsafe fn DisableClientState(&mut self, _array: GLenum) {}
    unsafe fn BlendFunc(&mut self, _sfactor: GLenum, _dfactor: GLenum) {}
    unsafe fn VertexPointer(
        &mut self,
        _size: GLint,
        _type: GLenum,
        _stride: GLsizei,
        _pointer: *const GLvoid,
    ) {
    }
    unsafe fn TexCoordPointer(
        &mut self,
        _size: GLint,
        _type: GLenum,
        _stride: GLsizei,
        _pointer: *const GLvoid,
    ) {
    }
}

#[test]
fn gles1_overlay_does_not_sample_the_presented_game_frame() {
    const GAME_TEXTURE: GLuint = 7;
    const ATLAS_TEXTURE: GLuint = 8;
    let mut quads = Vec::new();
    push_rect(
        &mut quads,
        Rect {
            x: 10.0,
            y: 10.0,
            w: 32.0,
            h: 32.0,
        },
        COL_ACCENT,
    );
    let button = quads[0];
    let glyph = Quad {
        tex: ATLAS_TEXTURE,
        col: COL_TEXT,
        ..button
    };
    // Initial solids, repeated glyphs, then a panel and another glyph.
    quads.extend([button, glyph, glyph, button, glyph]);

    // No cursor/FPS leaves texturing on; those overlays can leave it off.
    for textured in [true, false] {
        for viewport in [(0, 100, 320, 480), (100, 0, 480, 320)] {
            let mut gles = RecordingGles {
                textured,
                texture: GAME_TEXTURE,
                tex_env_mode: gles11::REPLACE as GLint,
                color: (1.0, 1.0, 1.0, 1.0),
                draws: Vec::new(),
            };
            unsafe { render_gles1(&mut gles, viewport, &quads) };
            let expected: Vec<_> = quads
                .iter()
                .map(|q| Draw {
                    texture: (q.tex != 0).then_some(q.tex),
                    color: q.col,
                })
                .collect();
            assert_eq!(gles.draws, expected);
            assert_eq!(gles.texture, GAME_TEXTURE);
            assert_eq!(gles.tex_env_mode, gles11::REPLACE as GLint);
            assert!(!gles.textured);
        }
    }
}

#[test]
fn panel_has_unique_actions_and_results_header_below_keypad() {
    let mut ui = TrainerUi::new();
    ui.open = true;
    for viewport in [(0, 100, 320, 480), (100, 0, 480, 320)] {
        let panel = compute_layout(&ui, viewport).panel.unwrap();
        let ids: std::collections::HashSet<_> = panel.widgets.iter().map(|&(id, _)| id).collect();
        assert_eq!(ids.len(), panel.widgets.len(), "duplicate widget IDs");
        assert_eq!(panel.widgets.iter().filter(|&&(id, _)| id == W_SET_ALL).count(), 1);
        assert_eq!(panel.widgets.iter().filter(|&&(id, _)| id == W_DUMP).count(), 1);
        let scroll = panel.widgets.iter().find(|&&(id, _)| id == W_SCROLL_UP).unwrap().1;
        assert_eq!(panel.results_header_y, scroll.y);
        for &(id, rect) in &panel.widgets {
            if (W_KEY_BASE..W_KEY_BASE + 16).contains(&id) {
                assert!(rect.y + rect.h <= panel.results_header_y);
            }
        }
    }
}

#[test]
fn dump_and_safe_all_dispatch_distinct_commands() {
    let mut ui = TrainerUi::new();
    ui.set_text = "999".to_string();
    take_commands();
    activate_widget(&mut ui, W_DUMP);
    let commands = take_commands();
    assert!(matches!(commands.as_slice(), [TrainerCmd::CancelBulk, TrainerCmd::Dump]));
    activate_widget(&mut ui, W_SET_ALL);
    let commands = take_commands();
    assert!(matches!(commands.as_slice(), [TrainerCmd::SetAll { confirm: false, .. }]));
    ui.bulk_preview = true;
    activate_widget(&mut ui, W_SET_ALL);
    let commands = take_commands();
    assert!(matches!(commands.as_slice(), [TrainerCmd::SetAll { confirm: true, .. }]));
    // A second click before a new preview is drawn cannot confirm again.
    activate_widget(&mut ui, W_SET_ALL);
    let commands = take_commands();
    assert!(matches!(commands.as_slice(), [TrainerCmd::SetAll { confirm: false, .. }]));
    ui.bulk_preview = true;
    activate_widget(&mut ui, W_TYPE);
    assert!(!ui.bulk_preview);
    assert!(matches!(take_commands().as_slice(), [TrainerCmd::CancelBulk]));
}
