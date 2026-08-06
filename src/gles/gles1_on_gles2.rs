/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
//! OpenGL ES 1.1 fixed-function emulation on an OpenGL ES 2.0 context.
//!
//! OpenGL ES 2.0 removed the fixed-function pipeline. This backend keeps the
//! GLES 1.1 API state on the CPU and renders it through a small GLSL ES 1.00
//! program. It is intended for Android devices whose native GLES 1.1 path can
//! render a black frame while their GLES 2.0/3.0 path works correctly.

use super::gles2_raw as gl;
use super::gles2_raw::types::*;
use super::gles11_raw as es1;
use super::gles_generic::{GLchar, GLES};
use super::util::{fixed_to_float, float_to_fixed, try_decode_pvrtc};
use super::GLESContext;
use crate::window::{GLContext, GLVersion, Window};
use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::marker::PhantomData;

const ATTR_POSITION: GLuint = 0;
const ATTR_COLOR: GLuint = 1;
const ATTR_NORMAL: GLuint = 2;
const ATTR_TEX0: GLuint = 3;
const MAX_TEXTURE_UNITS: usize = 4;
const MATRIX_IDENTITY: [GLfloat; 16] = [
    1.0, 0.0, 0.0, 0.0,
    0.0, 1.0, 0.0, 0.0,
    0.0, 0.0, 1.0, 0.0,
    0.0, 0.0, 0.0, 1.0,
];

#[derive(Clone, Copy)]
struct ArrayState {
    size: GLint,
    type_: GLenum,
    stride: GLsizei,
    pointer: *const GLvoid,
    buffer_binding: GLuint,
    enabled: bool,
    fixed: bool,
    normalized: bool,
}

impl Default for ArrayState {
    fn default() -> Self {
        Self {
            size: 4,
            type_: gl::FLOAT,
            stride: 0,
            pointer: std::ptr::null(),
            buffer_binding: 0,
            enabled: false,
            fixed: false,
            normalized: false,
        }
    }
}

struct MatrixState {
    current: [GLfloat; 16],
    stack: Vec<[GLfloat; 16]>,
}

impl MatrixState {
    fn new() -> Self {
        Self {
            current: MATRIX_IDENTITY,
            stack: Vec::new(),
        }
    }
}

struct TranslatorState {
    modelview: MatrixState,
    projection: MatrixState,
    texture: [MatrixState; MAX_TEXTURE_UNITS],
    matrix_mode: GLenum,
    active_texture: usize,
    client_active_texture: usize,
    color: [GLfloat; 4],
    normal: [GLfloat; 3],
    texcoords: [[GLfloat; 4]; MAX_TEXTURE_UNITS],
    arrays: [ArrayState; 3],
    texcoord_arrays: [ArrayState; MAX_TEXTURE_UNITS],
    texture_enabled: [bool; MAX_TEXTURE_UNITS],
    texture_env_mode: [GLint; MAX_TEXTURE_UNITS],
    texture_env_color: [[GLfloat; 4]; MAX_TEXTURE_UNITS],
    fixed_buffers: [Vec<GLfloat>; 3],
    array_buffer_binding: GLuint,
    element_array_buffer_binding: GLuint,
    array_buffer_data: HashMap<GLuint, Vec<u8>>,
    element_array_buffer_data: HashMap<GLuint, Vec<u8>>,
    point_size: GLfloat,
    alpha_test_enabled: bool,
    alpha_func: GLenum,
    alpha_ref: GLclampf,
    fog_enabled: bool,
    fog_mode: GLenum,
    fog_density: GLfloat,
    fog_start: GLfloat,
    fog_end: GLfloat,
    fog_color: [GLfloat; 4],
    program: Option<GLuint>,
}

impl TranslatorState {
    fn new() -> Self {
        Self {
            modelview: MatrixState::new(),
            projection: MatrixState::new(),
            texture: std::array::from_fn(|_| MatrixState::new()),
            matrix_mode: es1::MODELVIEW,
            active_texture: 0,
            client_active_texture: 0,
            color: [1.0; 4],
            normal: [0.0, 0.0, 1.0],
            texcoords: [[0.0, 0.0, 0.0, 1.0]; MAX_TEXTURE_UNITS],
            arrays: [ArrayState::default(); 3],
            texcoord_arrays: [ArrayState::default(); MAX_TEXTURE_UNITS],
            texture_enabled: [false; MAX_TEXTURE_UNITS],
            texture_env_mode: [es1::MODULATE as GLint; MAX_TEXTURE_UNITS],
            texture_env_color: [[0.0, 0.0, 0.0, 0.0]; MAX_TEXTURE_UNITS],
            fixed_buffers: std::array::from_fn(|_| Vec::new()),
            array_buffer_binding: 0,
            element_array_buffer_binding: 0,
            array_buffer_data: HashMap::new(),
            element_array_buffer_data: HashMap::new(),
            point_size: 1.0,
            alpha_test_enabled: false,
            alpha_func: es1::ALWAYS,
            alpha_ref: 0.0,
            fog_enabled: false,
            fog_mode: es1::EXP,
            fog_density: 1.0,
            fog_start: 0.0,
            fog_end: 1.0,
            fog_color: [0.0, 0.0, 0.0, 1.0],
            program: None,
        }
    }

    fn matrix_mut(&mut self) -> &mut MatrixState {
        match self.matrix_mode {
            es1::PROJECTION => &mut self.projection,
            es1::TEXTURE => &mut self.texture[self.active_texture],
            _ => &mut self.modelview,
        }
    }

    fn mvp(&self) -> [GLfloat; 16] {
        multiply(&self.projection.current, &self.modelview.current)
    }
}

pub struct GLES1OnGLES2Context {
    gl_ctx: GLContext,
    is_loaded: bool,
    state: TranslatorState,
}

impl GLESContext for GLES1OnGLES2Context {
    fn description() -> &'static str {
        "OpenGL ES 1.1 translated to native OpenGL ES 2.0 shaders"
    }

    fn new(window: &mut Window) -> Result<Self, String> {
        Ok(Self {
            gl_ctx: window.create_gl_context(GLVersion::GLES20)?,
            is_loaded: false,
            state: TranslatorState::new(),
        })
    }

    fn make_current<'gl_ctx, 'win: 'gl_ctx>(
        &'gl_ctx mut self,
        window: &'win mut Window,
    ) -> Box<dyn GLES + 'gl_ctx> {
        if !self.gl_ctx.is_current() || !self.is_loaded {
            unsafe { window.make_gl_context_current(&self.gl_ctx) };
            gl::load_with(|s| window.gl_get_proc_address(s));
            es1::load_with(|s| window.gl_get_proc_address(s));
            self.is_loaded = true;
        }
        Box::new(GLES1OnGLES2 {
            state: &mut self.state,
            _gl_lifetime: PhantomData,
        })
    }

    unsafe fn make_current_unchecked_for_window<'gl_ctx>(
        &'gl_ctx mut self,
        make_current_fn: &mut dyn FnMut(&GLContext),
        loader_fn: &mut dyn FnMut(&'static str) -> *const std::ffi::c_void,
    ) -> Box<dyn GLES + 'gl_ctx> {
        if !self.gl_ctx.is_current() || !self.is_loaded {
            make_current_fn(&self.gl_ctx);
            gl::load_with(&mut *loader_fn);
            es1::load_with(&mut *loader_fn);
            self.is_loaded = true;
        }
        Box::new(GLES1OnGLES2 {
            state: &mut self.state,
            _gl_lifetime: PhantomData,
        })
    }
}

pub struct GLES1OnGLES2<'a> {
    state: &'a mut TranslatorState,
    _gl_lifetime: PhantomData<&'a ()>,
}

fn multiply(a: &[GLfloat; 16], b: &[GLfloat; 16]) -> [GLfloat; 16] {
    let mut out = [0.0; 16];
    for col in 0..4 {
        for row in 0..4 {
            out[col * 4 + row] = (0..4).map(|k| a[k * 4 + row] * b[col * 4 + k]).sum();
        }
    }
    out
}

fn translation(x: GLfloat, y: GLfloat, z: GLfloat) -> [GLfloat; 16] {
    let mut m = MATRIX_IDENTITY;
    m[12] = x;
    m[13] = y;
    m[14] = z;
    m
}

fn scaling(x: GLfloat, y: GLfloat, z: GLfloat) -> [GLfloat; 16] {
    let mut m = MATRIX_IDENTITY;
    m[0] = x;
    m[5] = y;
    m[10] = z;
    m
}

fn rotation(angle: GLfloat, x: GLfloat, y: GLfloat, z: GLfloat) -> [GLfloat; 16] {
    let length = (x * x + y * y + z * z).sqrt();
    if length == 0.0 {
        return MATRIX_IDENTITY;
    }
    let (x, y, z) = (x / length, y / length, z / length);
    let r = angle.to_radians();
    let (s, c) = (r.sin(), r.cos());
    let t = 1.0 - c;
    [
        t * x * x + c, t * x * y + s * z, t * x * z - s * y, 0.0,
        t * x * y - s * z, t * y * y + c, t * y * z + s * x, 0.0,
        t * x * z + s * y, t * y * z - s * x, t * z * z + c, 0.0,
        0.0, 0.0, 0.0, 1.0,
    ]
}

fn ortho(left: GLfloat, right: GLfloat, bottom: GLfloat, top: GLfloat, near: GLfloat, far: GLfloat) -> [GLfloat; 16] {
    [
        2.0 / (right - left), 0.0, 0.0, 0.0,
        0.0, 2.0 / (top - bottom), 0.0, 0.0,
        0.0, 0.0, -2.0 / (far - near), 0.0,
        -(right + left) / (right - left), -(top + bottom) / (top - bottom),
        -(far + near) / (far - near), 1.0,
    ]
}

fn frustum(left: GLfloat, right: GLfloat, bottom: GLfloat, top: GLfloat, near: GLfloat, far: GLfloat) -> [GLfloat; 16] {
    [
        2.0 * near / (right - left), 0.0, 0.0, 0.0,
        0.0, 2.0 * near / (top - bottom), 0.0, 0.0,
        (right + left) / (right - left), (top + bottom) / (top - bottom),
        -(far + near) / (far - near), -1.0,
        0.0, 0.0, -2.0 * far * near / (far - near), 0.0,
    ]
}

fn compile_shader(kind: GLenum, source: &str) -> Result<GLuint, String> {
    unsafe {
        let shader = gl::CreateShader(kind);
        let source = CString::new(source).unwrap();
        let pointer = source.as_ptr();
        gl::ShaderSource(shader, 1, &pointer, std::ptr::null());
        gl::CompileShader(shader);
        let mut ok = 0;
        gl::GetShaderiv(shader, gl::COMPILE_STATUS, &mut ok);
        if ok == 0 {
            let mut log = [0i8; 2048];
            let mut len = 0;
            gl::GetShaderInfoLog(shader, log.len() as GLsizei, &mut len, log.as_mut_ptr() as _);
            return Err(format!(
                "GLES1-on-GLES2 shader compilation failed: {}",
                String::from_utf8_lossy(std::slice::from_raw_parts(log.as_ptr() as *const u8, len.max(0) as usize))
            ));
        }
        Ok(shader)
    }
}

fn create_program() -> Result<GLuint, String> {
    let vertex = compile_shader(gl::VERTEX_SHADER, r#"#version 100
precision mediump float;
attribute vec4 a_position;
attribute vec4 a_color;
attribute vec3 a_normal;
attribute vec4 a_tex0;
uniform mat4 u_mvp;
uniform mat4 u_modelview;
uniform mat4 u_texture_matrix0;
uniform vec4 u_color;
uniform float u_point_size;
varying vec4 v_color;
varying vec2 v_tex0;
varying float v_fog_coord;
void main() {
    vec4 eye_position = u_modelview * a_position;
    gl_Position = u_mvp * a_position;
    gl_PointSize = u_point_size;
    v_color = a_color * u_color;
    v_tex0 = (u_texture_matrix0 * a_tex0).xy;
    v_fog_coord = abs(eye_position.z);
}
"#)?;
    let fragment = compile_shader(gl::FRAGMENT_SHADER, r#"#version 100
precision mediump float;
varying vec4 v_color;
varying vec2 v_tex0;
varying float v_fog_coord;
uniform sampler2D u_tex0;
uniform vec4 u_env_color0;
uniform int u_tex_enabled0;
uniform int u_tex_mode0;
uniform int u_alpha_test_enabled;
uniform int u_alpha_func;
uniform float u_alpha_ref;
uniform int u_fog_enabled;
uniform vec4 u_fog_color;
uniform float u_fog_density;
uniform float u_fog_start;
uniform float u_fog_end;
uniform int u_fog_mode;
float fog_factor() {
    if (u_fog_mode == 2048) return exp(-u_fog_density * v_fog_coord);
    if (u_fog_mode == 2049) {
        float d = u_fog_density * v_fog_coord;
        return exp(-d * d);
    }
    return (u_fog_end - v_fog_coord) / (u_fog_end - u_fog_start);
}
bool alpha_pass(float alpha) {
    if (u_alpha_func == 512) return false;
    if (u_alpha_func == 513) return alpha < u_alpha_ref;
    if (u_alpha_func == 514) return alpha == u_alpha_ref;
    if (u_alpha_func == 515) return alpha <= u_alpha_ref;
    if (u_alpha_func == 516) return alpha > u_alpha_ref;
    if (u_alpha_func == 517) return alpha != u_alpha_ref;
    if (u_alpha_func == 518) return alpha >= u_alpha_ref;
    return true;
}
void main() {
    vec4 color = v_color;
    if (u_tex_enabled0 != 0) {
        vec4 texel = texture2D(u_tex0, v_tex0);
        if (u_tex_mode0 == 1) color = texel;
        else if (u_tex_mode0 == 2) color = color * texel;
        else if (u_tex_mode0 == 3) color = vec4(color.rgb + texel.rgb, color.a * texel.a);
        else if (u_tex_mode0 == 4) color = vec4(mix(color.rgb, texel.rgb, texel.a), color.a);
    }
    if (u_alpha_test_enabled != 0 && !alpha_pass(color.a)) discard;
    if (u_fog_enabled != 0) {
        float factor = clamp(fog_factor(), 0.0, 1.0);
        color = mix(u_fog_color, color, factor);
    }
    gl_FragColor = color;
}
"#)?;
    unsafe {
        let program = gl::CreateProgram();
        gl::AttachShader(program, vertex);
        gl::AttachShader(program, fragment);
        gl::BindAttribLocation(program, ATTR_POSITION, b"a_position\0".as_ptr() as *const GLchar);
        gl::BindAttribLocation(program, ATTR_COLOR, b"a_color\0".as_ptr() as *const GLchar);
        gl::BindAttribLocation(program, ATTR_NORMAL, b"a_normal\0".as_ptr() as *const GLchar);
        gl::BindAttribLocation(program, ATTR_TEX0, b"a_tex0\0".as_ptr() as *const GLchar);
        gl::LinkProgram(program);
        let mut ok = 0;
        gl::GetProgramiv(program, gl::LINK_STATUS, &mut ok);
        if ok == 0 {
            let mut log = [0i8; 2048];
            let mut len = 0;
            gl::GetProgramInfoLog(program, log.len() as GLsizei, &mut len, log.as_mut_ptr() as _);
            gl::DeleteShader(vertex);
            gl::DeleteShader(fragment);
            return Err(format!(
                "GLES1-on-GLES2 program link failed: {}",
                String::from_utf8_lossy(std::slice::from_raw_parts(log.as_ptr() as *const u8, len.max(0) as usize))
            ));
        }
        gl::DeleteShader(vertex);
        gl::DeleteShader(fragment);
        Ok(program)
    }
}

impl GLES for GLES1OnGLES2<'_> {
    fn is_es2(&self) -> bool {
        true
    }

    unsafe fn driver_description(&self) -> String {
        let version = CStr::from_ptr(gl::GetString(gl::VERSION) as *const _);
        let vendor = CStr::from_ptr(gl::GetString(gl::VENDOR) as *const _);
        let renderer = CStr::from_ptr(gl::GetString(gl::RENDERER) as *const _);
        format!("GLES1 translated by GLES2 / {} / {} / {}", version.to_string_lossy(), vendor.to_string_lossy(), renderer.to_string_lossy())
    }

    unsafe fn GetError(&mut self) -> GLenum { gl::GetError() }
    unsafe fn GetString(&mut self, name: GLenum) -> *const GLubyte { gl::GetString(name) }
    unsafe fn GetBooleanv(&mut self, pname: GLenum, params: *mut GLboolean) {
        match pname {
            es1::TEXTURE_2D => *params = if self.state.texture_enabled[self.state.active_texture] { gl::TRUE } else { gl::FALSE },
            es1::ALPHA_TEST => *params = if self.state.alpha_test_enabled { gl::TRUE } else { gl::FALSE },
            es1::FOG => *params = if self.state.fog_enabled { gl::TRUE } else { gl::FALSE },
            es1::LIGHTING => *params = gl::FALSE,
            _ => gl::GetBooleanv(pname, params),
        }
    }
    unsafe fn GetFloatv(&mut self, pname: GLenum, params: *mut GLfloat) {
        match pname {
            es1::MODELVIEW_MATRIX => params.copy_from(self.state.modelview.current.as_ptr(), 16),
            es1::PROJECTION_MATRIX => params.copy_from(self.state.projection.current.as_ptr(), 16),
            es1::TEXTURE_MATRIX => params.copy_from(self.state.texture[self.state.active_texture].current.as_ptr(), 16),
            es1::CURRENT_COLOR => params.copy_from(self.state.color.as_ptr(), 4),
            es1::CURRENT_NORMAL => params.copy_from(self.state.normal.as_ptr(), 3),
            es1::FOG_COLOR => params.copy_from(self.state.fog_color.as_ptr(), 4),
            es1::FOG_DENSITY => *params = self.state.fog_density,
            es1::FOG_START => *params = self.state.fog_start,
            es1::FOG_END => *params = self.state.fog_end,
            es1::POINT_SIZE => *params = self.state.point_size,
            _ => gl::GetFloatv(pname, params),
        }
    }
    unsafe fn GetTexEnviv(&mut self, target: GLenum, pname: GLenum, params: *mut GLint) {
        assert_eq!(target, es1::TEXTURE_ENV);
        if pname == es1::TEXTURE_ENV_MODE {
            *params = self.state.texture_env_mode[self.state.active_texture];
        } else if pname == es1::TEXTURE_ENV_COLOR {
            for (index, value) in self.state.texture_env_color[self.state.active_texture].iter().enumerate() {
                *params.add(index) = *value as GLint;
            }
        }
    }
    unsafe fn GetTexEnvfv(&mut self, target: GLenum, pname: GLenum, params: *mut GLfloat) {
        assert_eq!(target, es1::TEXTURE_ENV);
        if pname == es1::TEXTURE_ENV_MODE {
            *params = self.state.texture_env_mode[self.state.active_texture] as GLfloat;
        } else if pname == es1::TEXTURE_ENV_COLOR {
            params.copy_from(self.state.texture_env_color[self.state.active_texture].as_ptr(), 4);
        }
    }
    unsafe fn GetTexEnvxv(&mut self, target: GLenum, pname: GLenum, params: *mut GLfixed) {
        let mut values = [0.0; 4];
        self.GetTexEnvfv(target, pname, values.as_mut_ptr());
        for (index, value) in values.iter().enumerate() {
            *params.add(index) = float_to_fixed(*value);
        }
    }
    unsafe fn GetTexParameteriv(&mut self, target: GLenum, pname: GLenum, params: *mut GLint) { gl::GetTexParameteriv(target, pname, params); }
    unsafe fn GetTexParameterfv(&mut self, target: GLenum, pname: GLenum, params: *mut GLfloat) { gl::GetTexParameterfv(target, pname, params); }
    unsafe fn GetTexParameterxv(&mut self, target: GLenum, pname: GLenum, params: *mut GLfixed) {
        let mut value = 0.0;
        gl::GetTexParameterfv(target, pname, &mut value);
        *params = float_to_fixed(value);
    }
    unsafe fn Enable(&mut self, cap: GLenum) {
        if cap == es1::TEXTURE_2D {
            self.state.texture_enabled[self.state.active_texture] = true;
        } else if cap == es1::ALPHA_TEST {
            self.state.alpha_test_enabled = true;
        } else if cap == es1::FOG {
            self.state.fog_enabled = true;
        } else if cap != es1::LIGHTING {
            gl::Enable(cap);
        }
    }
    unsafe fn Disable(&mut self, cap: GLenum) {
        if cap == es1::TEXTURE_2D {
            self.state.texture_enabled[self.state.active_texture] = false;
        } else if cap == es1::ALPHA_TEST {
            self.state.alpha_test_enabled = false;
        } else if cap == es1::FOG {
            self.state.fog_enabled = false;
        } else if cap != es1::LIGHTING {
            gl::Disable(cap);
        }
    }
    unsafe fn IsEnabled(&mut self, cap: GLenum) -> GLboolean {
        if cap == es1::TEXTURE_2D {
            return if self.state.texture_enabled[self.state.active_texture] { gl::TRUE } else { gl::FALSE };
        }
        if cap == es1::ALPHA_TEST {
            return if self.state.alpha_test_enabled { gl::TRUE } else { gl::FALSE };
        }
        if cap == es1::FOG {
            return if self.state.fog_enabled { gl::TRUE } else { gl::FALSE };
        }
        if cap == es1::LIGHTING {
            return gl::FALSE;
        }
        gl::IsEnabled(cap)
    }
    unsafe fn ClientActiveTexture(&mut self, texture: GLenum) {
        self.state.client_active_texture = (texture - es1::TEXTURE0).min((MAX_TEXTURE_UNITS - 1) as GLenum) as usize;
    }
    unsafe fn ActiveTexture(&mut self, texture: GLenum) {
        self.state.active_texture = texture.saturating_sub(es1::TEXTURE0).min((MAX_TEXTURE_UNITS - 1) as GLenum) as usize;
        gl::ActiveTexture(es1::TEXTURE0 + self.state.active_texture as GLenum);
    }
    unsafe fn EnableClientState(&mut self, array: GLenum) {
        match array {
            es1::VERTEX_ARRAY => self.state.arrays[0].enabled = true,
            es1::COLOR_ARRAY => self.state.arrays[1].enabled = true,
            es1::NORMAL_ARRAY => self.state.arrays[2].enabled = true,
            es1::TEXTURE_COORD_ARRAY => self.state.texcoord_arrays[self.state.client_active_texture].enabled = true,
            _ => {}
        }
    }
    unsafe fn DisableClientState(&mut self, array: GLenum) {
        match array {
            es1::VERTEX_ARRAY => self.state.arrays[0].enabled = false,
            es1::COLOR_ARRAY => self.state.arrays[1].enabled = false,
            es1::NORMAL_ARRAY => self.state.arrays[2].enabled = false,
            es1::TEXTURE_COORD_ARRAY => self.state.texcoord_arrays[self.state.client_active_texture].enabled = false,
            _ => {}
        }
    }
    unsafe fn AlphaFunc(&mut self, func: GLenum, ref_: GLclampf) {
        self.state.alpha_func = func;
        self.state.alpha_ref = ref_;
    }
    unsafe fn AlphaFuncx(&mut self, func: GLenum, ref_: GLclampx) {
        self.AlphaFunc(func, fixed_to_float(ref_));
    }
    unsafe fn DepthRangef(&mut self, near: GLclampf, far: GLclampf) {
        gl::DepthRangef(near, far);
    }
    unsafe fn DepthRangex(&mut self, near: GLclampx, far: GLclampx) {
        self.DepthRangef(fixed_to_float(near), fixed_to_float(far));
    }
    unsafe fn PolygonOffset(&mut self, factor: GLfloat, units: GLfloat) {
        gl::PolygonOffset(factor, units);
    }
    unsafe fn PolygonOffsetx(&mut self, factor: GLfixed, units: GLfixed) {
        self.PolygonOffset(fixed_to_float(factor), fixed_to_float(units));
    }
    unsafe fn SampleCoverage(&mut self, value: GLclampf, invert: GLboolean) {
        gl::SampleCoverage(value, invert);
    }
    unsafe fn SampleCoveragex(&mut self, value: GLclampx, invert: GLboolean) {
        self.SampleCoverage(fixed_to_float(value), invert);
    }
    unsafe fn Color4f(&mut self, r: GLfloat, g: GLfloat, b: GLfloat, a: GLfloat) { self.state.color = [r, g, b, a]; }
    unsafe fn Color4x(&mut self, r: GLfixed, g: GLfixed, b: GLfixed, a: GLfixed) { self.Color4f(fixed_to_float(r), fixed_to_float(g), fixed_to_float(b), fixed_to_float(a)); }
    unsafe fn Color4ub(&mut self, r: GLubyte, g: GLubyte, b: GLubyte, a: GLubyte) { self.state.color = [r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, a as f32 / 255.0]; }
    unsafe fn Normal3f(&mut self, x: GLfloat, y: GLfloat, z: GLfloat) { self.state.normal = [x, y, z]; }
    unsafe fn Normal3x(&mut self, x: GLfixed, y: GLfixed, z: GLfixed) { self.Normal3f(fixed_to_float(x), fixed_to_float(y), fixed_to_float(z)); }
    unsafe fn MultiTexCoord4f(&mut self, texture: GLenum, s: GLfloat, t: GLfloat, r: GLfloat, q: GLfloat) { let i = (texture - es1::TEXTURE0).min((MAX_TEXTURE_UNITS - 1) as GLenum) as usize; self.state.texcoords[i] = [s, t, r, q]; }
    unsafe fn MultiTexCoord4x(&mut self, texture: GLenum, s: GLfixed, t: GLfixed, r: GLfixed, q: GLfixed) { self.MultiTexCoord4f(texture, fixed_to_float(s), fixed_to_float(t), fixed_to_float(r), fixed_to_float(q)); }
    unsafe fn TexCoordPointer(&mut self, size: GLint, type_: GLenum, stride: GLsizei, pointer: *const GLvoid) {
        let enabled = self.state.texcoord_arrays[self.state.client_active_texture].enabled;
        let buffer_binding = self.state.array_buffer_binding;
        self.state.texcoord_arrays[self.state.client_active_texture] = ArrayState { size, type_, stride, pointer, buffer_binding, enabled, fixed: type_ == es1::FIXED, normalized: false };
    }
    unsafe fn ColorPointer(&mut self, size: GLint, type_: GLenum, stride: GLsizei, pointer: *const GLvoid) {
        let enabled = self.state.arrays[1].enabled;
        let buffer_binding = self.state.array_buffer_binding;
        self.state.arrays[1] = ArrayState { size, type_, stride, pointer, buffer_binding, enabled, fixed: type_ == es1::FIXED, normalized: true };
    }
    unsafe fn NormalPointer(&mut self, type_: GLenum, stride: GLsizei, pointer: *const GLvoid) {
        let enabled = self.state.arrays[2].enabled;
        let buffer_binding = self.state.array_buffer_binding;
        self.state.arrays[2] = ArrayState { size: 3, type_, stride, pointer, buffer_binding, enabled, fixed: type_ == es1::FIXED, normalized: false };
    }
    unsafe fn VertexPointer(&mut self, size: GLint, type_: GLenum, stride: GLsizei, pointer: *const GLvoid) {
        let enabled = self.state.arrays[0].enabled;
        let buffer_binding = self.state.array_buffer_binding;
        self.state.arrays[0] = ArrayState { size, type_, stride, pointer, buffer_binding, enabled, fixed: type_ == es1::FIXED, normalized: false };
    }
    unsafe fn BindBuffer(&mut self, target: GLenum, buffer: GLuint) {
        match target {
            gl::ARRAY_BUFFER => self.state.array_buffer_binding = buffer,
            gl::ELEMENT_ARRAY_BUFFER => self.state.element_array_buffer_binding = buffer,
            _ => {}
        }
        gl::BindBuffer(target, buffer);
    }
    unsafe fn GenBuffers(&mut self, n: GLsizei, buffers: *mut GLuint) { gl::GenBuffers(n, buffers); }
    unsafe fn DeleteBuffers(&mut self, n: GLsizei, buffers: *const GLuint) {
        if !buffers.is_null() {
            for i in 0..n.max(0) as usize {
                let buffer = buffers.add(i).read();
                self.state.array_buffer_data.remove(&buffer);
                self.state.element_array_buffer_data.remove(&buffer);
            }
        }
        gl::DeleteBuffers(n, buffers);
    }
    unsafe fn BufferData(&mut self, target: GLenum, size: GLsizeiptr, data: *const GLvoid, usage: GLenum) {
        let binding = match target {
            gl::ARRAY_BUFFER => self.state.array_buffer_binding,
            gl::ELEMENT_ARRAY_BUFFER => self.state.element_array_buffer_binding,
            _ => 0,
        };
        if binding != 0 && size >= 0 {
            let store = if target == gl::ARRAY_BUFFER { &mut self.state.array_buffer_data } else { &mut self.state.element_array_buffer_data };
            let bytes = store.entry(binding).or_default();
            bytes.resize(size as usize, 0);
            if !data.is_null() { std::ptr::copy_nonoverlapping(data.cast::<u8>(), bytes.as_mut_ptr(), size as usize); }
        }
        gl::BufferData(target, size, data, usage);
    }
    unsafe fn BufferSubData(&mut self, target: GLenum, offset: GLintptr, size: GLsizeiptr, data: *const GLvoid) {
        let binding = match target {
            gl::ARRAY_BUFFER => self.state.array_buffer_binding,
            gl::ELEMENT_ARRAY_BUFFER => self.state.element_array_buffer_binding,
            _ => 0,
        };
        if binding != 0 && offset >= 0 && size >= 0 && !data.is_null() {
            let store = if target == gl::ARRAY_BUFFER { &mut self.state.array_buffer_data } else { &mut self.state.element_array_buffer_data };
            let bytes = store.entry(binding).or_default();
            let end = offset as usize + size as usize;
            if end > bytes.len() { bytes.resize(end, 0); }
            std::ptr::copy_nonoverlapping(data.cast::<u8>(), bytes.as_mut_ptr().add(offset as usize), size as usize);
        }
        gl::BufferSubData(target, offset, size, data);
    }
    unsafe fn BindTexture(&mut self, target: GLenum, texture: GLuint) { gl::BindTexture(target, texture); }
    unsafe fn GenTextures(&mut self, n: GLsizei, textures: *mut GLuint) { gl::GenTextures(n, textures); }
    unsafe fn DeleteTextures(&mut self, n: GLsizei, textures: *const GLuint) { gl::DeleteTextures(n, textures); }
    unsafe fn TexParameteri(&mut self, target: GLenum, pname: GLenum, param: GLint) { gl::TexParameteri(target, pname, param); }
    unsafe fn TexParameterf(&mut self, target: GLenum, pname: GLenum, param: GLfloat) { gl::TexParameterf(target, pname, param); }
    unsafe fn TexParameterx(&mut self, target: GLenum, pname: GLenum, param: GLfixed) { gl::TexParameterf(target, pname, fixed_to_float(param)); }
    unsafe fn TexParameteriv(&mut self, target: GLenum, pname: GLenum, params: *const GLint) { gl::TexParameteriv(target, pname, params); }
    unsafe fn TexParameterfv(&mut self, target: GLenum, pname: GLenum, params: *const GLfloat) { gl::TexParameterfv(target, pname, params); }
    unsafe fn TexParameterxv(&mut self, target: GLenum, pname: GLenum, params: *const GLfixed) { let v = fixed_to_float(*params); gl::TexParameterf(target, pname, v); }
    unsafe fn TexImage2D(&mut self, target: GLenum, level: GLint, internalformat: GLint, width: GLsizei, height: GLsizei, border: GLint, format: GLenum, type_: GLenum, pixels: *const GLvoid) { gl::TexImage2D(target, level, internalformat, width, height, border, format, type_, pixels); }
    unsafe fn TexSubImage2D(&mut self, target: GLenum, level: GLint, x: GLint, y: GLint, width: GLsizei, height: GLsizei, format: GLenum, type_: GLenum, pixels: *const GLvoid) { gl::TexSubImage2D(target, level, x, y, width, height, format, type_, pixels); }
    unsafe fn CompressedTexImage2D(&mut self, target: GLenum, level: GLint, internalformat: GLenum, width: GLsizei, height: GLsizei, border: GLint, image_size: GLsizei, data: *const GLvoid) {
        if !data.is_null() && image_size > 0 && try_decode_pvrtc(self, target, level, internalformat, width, height, border, std::slice::from_raw_parts(data.cast::<u8>(), image_size as usize)) {
            return;
        }
        gl::CompressedTexImage2D(target, level, internalformat, width, height, border, image_size, data);
    }
    unsafe fn TexEnvi(&mut self, _target: GLenum, pname: GLenum, param: GLint) {
        let unit = self.state.active_texture;
        match pname {
            es1::TEXTURE_ENV_MODE => self.state.texture_env_mode[unit] = param,
            es1::TEXTURE_ENV_COLOR => self.state.texture_env_color[unit] = [param as GLfloat; 4],
            _ => {}
        }
    }
    unsafe fn TexEnvf(&mut self, target: GLenum, pname: GLenum, param: GLfloat) { self.TexEnvi(target, pname, param as GLint); }
    unsafe fn TexEnvx(&mut self, target: GLenum, pname: GLenum, param: GLfixed) { self.TexEnvi(target, pname, param); }
    unsafe fn TexEnviv(&mut self, target: GLenum, pname: GLenum, params: *const GLint) {
        if pname == es1::TEXTURE_ENV_COLOR { self.state.texture_env_color[self.state.active_texture] = std::slice::from_raw_parts(params, 4).iter().map(|v| *v as GLfloat).collect::<Vec<_>>().try_into().unwrap(); } else { self.TexEnvi(target, pname, *params); }
    }
    unsafe fn TexEnvfv(&mut self, target: GLenum, pname: GLenum, params: *const GLfloat) {
        if pname == es1::TEXTURE_ENV_COLOR { self.state.texture_env_color[self.state.active_texture] = std::slice::from_raw_parts(params, 4).try_into().unwrap(); } else { self.TexEnvi(target, pname, *params as GLint); }
    }
    unsafe fn TexEnvxv(&mut self, target: GLenum, pname: GLenum, params: *const GLfixed) {
        if pname == es1::TEXTURE_ENV_COLOR { self.state.texture_env_color[self.state.active_texture] = std::slice::from_raw_parts(params, 4).iter().map(|v| fixed_to_float(*v)).collect::<Vec<_>>().try_into().unwrap(); } else { self.TexEnvi(target, pname, *params); }
    }
    unsafe fn MatrixMode(&mut self, mode: GLenum) { self.state.matrix_mode = mode; }
    unsafe fn LoadIdentity(&mut self) { self.state.matrix_mut().current = MATRIX_IDENTITY; }
    unsafe fn LoadMatrixf(&mut self, m: *const GLfloat) { self.state.matrix_mut().current.copy_from_slice(std::slice::from_raw_parts(m, 16)); }
    unsafe fn LoadMatrixx(&mut self, m: *const GLfixed) { let mut out = [0.0; 16]; for (d, s) in out.iter_mut().zip(std::slice::from_raw_parts(m, 16)) { *d = fixed_to_float(*s); } self.state.matrix_mut().current = out; }
    unsafe fn MultMatrixf(&mut self, m: *const GLfloat) { let a = self.state.matrix_mut().current; let b = std::slice::from_raw_parts(m, 16).try_into().unwrap(); self.state.matrix_mut().current = multiply(&a, &b); }
    unsafe fn MultMatrixx(&mut self, m: *const GLfixed) { let mut b = [0.0; 16]; for (d, s) in b.iter_mut().zip(std::slice::from_raw_parts(m, 16)) { *d = fixed_to_float(*s); } let a = self.state.matrix_mut().current; self.state.matrix_mut().current = multiply(&a, &b); }
    unsafe fn PushMatrix(&mut self) { let current = self.state.matrix_mut().current; self.state.matrix_mut().stack.push(current); }
    unsafe fn PopMatrix(&mut self) { if let Some(m) = self.state.matrix_mut().stack.pop() { self.state.matrix_mut().current = m; } }
    unsafe fn Orthof(&mut self, l: GLfloat, r: GLfloat, b: GLfloat, t: GLfloat, n: GLfloat, f: GLfloat) { let a = self.state.matrix_mut().current; self.state.matrix_mut().current = multiply(&a, &ortho(l, r, b, t, n, f)); }
    unsafe fn Orthox(&mut self, l: GLfixed, r: GLfixed, b: GLfixed, t: GLfixed, n: GLfixed, f: GLfixed) { self.Orthof(fixed_to_float(l), fixed_to_float(r), fixed_to_float(b), fixed_to_float(t), fixed_to_float(n), fixed_to_float(f)); }
    unsafe fn Frustumf(&mut self, l: GLfloat, r: GLfloat, b: GLfloat, t: GLfloat, n: GLfloat, f: GLfloat) { let a = self.state.matrix_mut().current; self.state.matrix_mut().current = multiply(&a, &frustum(l, r, b, t, n, f)); }
    unsafe fn Frustumx(&mut self, l: GLfixed, r: GLfixed, b: GLfixed, t: GLfixed, n: GLfixed, f: GLfixed) { self.Frustumf(fixed_to_float(l), fixed_to_float(r), fixed_to_float(b), fixed_to_float(t), fixed_to_float(n), fixed_to_float(f)); }
    unsafe fn Translatef(&mut self, x: GLfloat, y: GLfloat, z: GLfloat) { let a = self.state.matrix_mut().current; self.state.matrix_mut().current = multiply(&a, &translation(x, y, z)); }
    unsafe fn Translatex(&mut self, x: GLfixed, y: GLfixed, z: GLfixed) { self.Translatef(fixed_to_float(x), fixed_to_float(y), fixed_to_float(z)); }
    unsafe fn Scalef(&mut self, x: GLfloat, y: GLfloat, z: GLfloat) { let a = self.state.matrix_mut().current; self.state.matrix_mut().current = multiply(&a, &scaling(x, y, z)); }
    unsafe fn Scalex(&mut self, x: GLfixed, y: GLfixed, z: GLfixed) { self.Scalef(fixed_to_float(x), fixed_to_float(y), fixed_to_float(z)); }
    unsafe fn Rotatef(&mut self, a: GLfloat, x: GLfloat, y: GLfloat, z: GLfloat) { let m = self.state.matrix_mut().current; self.state.matrix_mut().current = multiply(&m, &rotation(a, x, y, z)); }
    unsafe fn Rotatex(&mut self, a: GLfixed, x: GLfixed, y: GLfixed, z: GLfixed) { self.Rotatef(fixed_to_float(a), fixed_to_float(x), fixed_to_float(y), fixed_to_float(z)); }
    unsafe fn Viewport(&mut self, x: GLint, y: GLint, w: GLsizei, h: GLsizei) { gl::Viewport(x, y, w, h); }
    unsafe fn Scissor(&mut self, x: GLint, y: GLint, w: GLsizei, h: GLsizei) { gl::Scissor(x, y, w, h); }
    unsafe fn Clear(&mut self, mask: GLbitfield) { gl::Clear(mask); }
    unsafe fn ClearColor(&mut self, r: GLclampf, g: GLclampf, b: GLclampf, a: GLclampf) { gl::ClearColor(r, g, b, a); }
    unsafe fn ClearColorx(&mut self, r: GLclampx, g: GLclampx, b: GLclampx, a: GLclampx) { self.ClearColor(fixed_to_float(r), fixed_to_float(g), fixed_to_float(b), fixed_to_float(a)); }
    unsafe fn ClearDepthf(&mut self, d: GLclampf) { gl::ClearDepthf(d); }
    unsafe fn ClearStencil(&mut self, s: GLint) { gl::ClearStencil(s); }
    unsafe fn Fogf(&mut self, pname: GLenum, param: GLfloat) {
        match pname { es1::FOG_MODE => self.state.fog_mode = param as GLenum, es1::FOG_DENSITY => self.state.fog_density = param, es1::FOG_START => self.state.fog_start = param, es1::FOG_END => self.state.fog_end = param, _ => {} }
    }
    unsafe fn Fogx(&mut self, pname: GLenum, param: GLfixed) { self.Fogf(pname, fixed_to_float(param)); }
    unsafe fn Fogfv(&mut self, pname: GLenum, params: *const GLfloat) {
        if pname == es1::FOG_COLOR { self.state.fog_color = std::slice::from_raw_parts(params, 4).try_into().unwrap(); } else { self.Fogf(pname, *params); }
    }
    unsafe fn Fogxv(&mut self, pname: GLenum, params: *const GLfixed) {
        if pname == es1::FOG_COLOR { self.state.fog_color = std::slice::from_raw_parts(params, 4).iter().map(|v| fixed_to_float(*v)).collect::<Vec<_>>().try_into().unwrap(); } else { self.Fogx(pname, *params); }
    }
    unsafe fn GetIntegerv(&mut self, pname: GLenum, params: *mut GLint) { gl::GetIntegerv(pname, params); }
    unsafe fn DepthFunc(&mut self, f: GLenum) { gl::DepthFunc(f); }
    unsafe fn DepthMask(&mut self, f: GLboolean) { gl::DepthMask(f); }
    unsafe fn CullFace(&mut self, f: GLenum) { gl::CullFace(f); }
    unsafe fn FrontFace(&mut self, f: GLenum) { gl::FrontFace(f); }
    unsafe fn BlendFunc(&mut self, s: GLenum, d: GLenum) { gl::BlendFunc(s, d); }
    unsafe fn BlendEquationOES(&mut self, m: GLenum) { gl::BlendEquation(m); }
    unsafe fn ColorMask(&mut self, r: GLboolean, g: GLboolean, b: GLboolean, a: GLboolean) { gl::ColorMask(r, g, b, a); }
    unsafe fn LineWidth(&mut self, w: GLfloat) { gl::LineWidth(w); }
    unsafe fn Finish(&mut self) { gl::Finish(); }
    unsafe fn Flush(&mut self) { gl::Flush(); }
    unsafe fn ReadPixels(&mut self, x: GLint, y: GLint, w: GLsizei, h: GLsizei, format: GLenum, type_: GLenum, pixels: *mut GLvoid) { gl::ReadPixels(x, y, w, h, format, type_, pixels); }
    unsafe fn PixelStorei(&mut self, p: GLenum, v: GLint) { gl::PixelStorei(p, v); }
    unsafe fn GenFramebuffersOES(&mut self, n: GLsizei, p: *mut GLuint) { gl::GenFramebuffers(n, p); }
    unsafe fn DeleteFramebuffersOES(&mut self, n: GLsizei, p: *const GLuint) { gl::DeleteFramebuffers(n, p); }
    unsafe fn BindFramebufferOES(&mut self, t: GLenum, f: GLuint) { gl::BindFramebuffer(t, f); }
    unsafe fn GenRenderbuffersOES(&mut self, n: GLsizei, p: *mut GLuint) { gl::GenRenderbuffers(n, p); }
    unsafe fn DeleteRenderbuffersOES(&mut self, n: GLsizei, p: *const GLuint) { gl::DeleteRenderbuffers(n, p); }
    unsafe fn BindRenderbufferOES(&mut self, t: GLenum, r: GLuint) { gl::BindRenderbuffer(t, r); }
    unsafe fn RenderbufferStorageOES(&mut self, t: GLenum, f: GLenum, w: GLsizei, h: GLsizei) { gl::RenderbufferStorage(t, f, w, h); }
    unsafe fn GetRenderbufferParameterivOES(&mut self, t: GLenum, p: GLenum, params: *mut GLint) { gl::GetRenderbufferParameteriv(t, p, params); }
    unsafe fn FramebufferRenderbufferOES(&mut self, t: GLenum, a: GLenum, rt: GLenum, r: GLuint) { gl::FramebufferRenderbuffer(t, a, rt, r); }
    unsafe fn FramebufferTexture2DOES(&mut self, t: GLenum, a: GLenum, tt: GLenum, tex: GLuint, level: GLint) { gl::FramebufferTexture2D(t, a, tt, tex, level); }
    unsafe fn GetFramebufferAttachmentParameterivOES(&mut self, t: GLenum, a: GLenum, p: GLenum, params: *mut GLint) { gl::GetFramebufferAttachmentParameteriv(t, a, p, params); }
    unsafe fn GenerateMipmapOES(&mut self, t: GLenum) { gl::GenerateMipmap(t); }
    unsafe fn CheckFramebufferStatus(&mut self, t: GLenum) -> GLenum { gl::CheckFramebufferStatus(t) }
    unsafe fn BindFramebuffer(&mut self, t: GLenum, f: GLuint) { gl::BindFramebuffer(t, f); }
    unsafe fn DeleteFramebuffers(&mut self, n: GLsizei, p: *const GLuint) { gl::DeleteFramebuffers(n, p); }
    unsafe fn GenFramebuffers(&mut self, n: GLsizei, p: *mut GLuint) { gl::GenFramebuffers(n, p); }
    unsafe fn BindRenderbuffer(&mut self, t: GLenum, r: GLuint) { gl::BindRenderbuffer(t, r); }
    unsafe fn RenderbufferStorage(&mut self, t: GLenum, f: GLenum, w: GLsizei, h: GLsizei) { gl::RenderbufferStorage(t, f, w, h); }
    unsafe fn FramebufferRenderbuffer(&mut self, t: GLenum, a: GLenum, rt: GLenum, r: GLuint) { gl::FramebufferRenderbuffer(t, a, rt, r); }
    unsafe fn FramebufferTexture2D(&mut self, t: GLenum, a: GLenum, tt: GLenum, tex: GLuint, level: GLint) { gl::FramebufferTexture2D(t, a, tt, tex, level); }
    unsafe fn DeleteRenderbuffers(&mut self, n: GLsizei, p: *const GLuint) { gl::DeleteRenderbuffers(n, p); }
    unsafe fn GenRenderbuffers(&mut self, n: GLsizei, p: *mut GLuint) { gl::GenRenderbuffers(n, p); }
    unsafe fn IsFramebuffer(&mut self, f: GLuint) -> GLboolean { gl::IsFramebuffer(f) }
    unsafe fn IsRenderbuffer(&mut self, r: GLuint) -> GLboolean { gl::IsRenderbuffer(r) }
    unsafe fn IsTexture(&mut self, t: GLuint) -> GLboolean { gl::IsTexture(t) }
    unsafe fn PointSize(&mut self, size: GLfloat) {
        self.state.point_size = size;
    }
    unsafe fn PointSizex(&mut self, size: GLfixed) {
        self.state.point_size = fixed_to_float(size);
    }
    unsafe fn DrawArrays(&mut self, mode: GLenum, first: GLint, count: GLsizei) {
        let program = match self.state.program {
            Some(program) => program,
            None => {
                let Ok(program) = create_program() else {
                    log!("Warning: GLES1-on-GLES2 shader program unavailable; skipping draw");
                    return;
                };
                self.state.program = Some(program);
                program
            }
        };
        gl::UseProgram(program);
        let mvp = unsafe { self.state.mvp() };
        let mvp_loc = gl::GetUniformLocation(program, b"u_mvp\0".as_ptr() as *const _);
        gl::UniformMatrix4fv(mvp_loc, 1, gl::FALSE, mvp.as_ptr());
        let modelview_loc = gl::GetUniformLocation(program, b"u_modelview\0".as_ptr() as *const _);
        gl::UniformMatrix4fv(modelview_loc, 1, gl::FALSE, self.state.modelview.current.as_ptr());
        let texture_matrix_loc = gl::GetUniformLocation(program, b"u_texture_matrix0\0".as_ptr() as *const _);
        gl::UniformMatrix4fv(texture_matrix_loc, 1, gl::FALSE, self.state.texture[0].current.as_ptr());
        let color_loc = gl::GetUniformLocation(program, b"u_color\0".as_ptr() as *const _);
        gl::Uniform4fv(color_loc, 1, self.state.color.as_ptr());
        let alpha_test_loc = gl::GetUniformLocation(program, b"u_alpha_test_enabled\0".as_ptr() as *const _);
        gl::Uniform1i(alpha_test_loc, if self.state.alpha_test_enabled { 1 } else { 0 });
        let alpha_func_loc = gl::GetUniformLocation(program, b"u_alpha_func\0".as_ptr() as *const _);
        gl::Uniform1i(alpha_func_loc, self.state.alpha_func as GLint);
        let alpha_ref_loc = gl::GetUniformLocation(program, b"u_alpha_ref\0".as_ptr() as *const _);
        gl::Uniform1f(alpha_ref_loc, self.state.alpha_ref);
        let fog_enabled_loc = gl::GetUniformLocation(program, b"u_fog_enabled\0".as_ptr() as *const _);
        gl::Uniform1i(fog_enabled_loc, if self.state.fog_enabled { 1 } else { 0 });
        let fog_color_loc = gl::GetUniformLocation(program, b"u_fog_color\0".as_ptr() as *const _);
        gl::Uniform4fv(fog_color_loc, 1, self.state.fog_color.as_ptr());
        let fog_density_loc = gl::GetUniformLocation(program, b"u_fog_density\0".as_ptr() as *const _);
        gl::Uniform1f(fog_density_loc, self.state.fog_density);
        let fog_start_loc = gl::GetUniformLocation(program, b"u_fog_start\0".as_ptr() as *const _);
        gl::Uniform1f(fog_start_loc, self.state.fog_start);
        let fog_end_loc = gl::GetUniformLocation(program, b"u_fog_end\0".as_ptr() as *const _);
        gl::Uniform1f(fog_end_loc, self.state.fog_end);
        let fog_mode_loc = gl::GetUniformLocation(program, b"u_fog_mode\0".as_ptr() as *const _);
        gl::Uniform1i(fog_mode_loc, self.state.fog_mode as GLint);
        let point_size_loc = gl::GetUniformLocation(program, b"u_point_size\0".as_ptr() as *const _);
        gl::Uniform1f(point_size_loc, self.state.point_size);
        let tex_enabled = self.state.texture_enabled[0];
        let enabled_loc = gl::GetUniformLocation(program, b"u_tex_enabled0\0".as_ptr() as *const _);
        gl::Uniform1i(enabled_loc, if tex_enabled { 1 } else { 0 });
        let mode_loc = gl::GetUniformLocation(program, b"u_tex_mode0\0".as_ptr() as *const _);
        gl::Uniform1i(mode_loc, match self.state.texture_env_mode[0] as GLenum { es1::REPLACE => 1, es1::ADD => 3, es1::DECAL => 4, _ => 2 });
        let env_color_loc = gl::GetUniformLocation(program, b"u_env_color0\0".as_ptr() as *const _);
        gl::Uniform4fv(env_color_loc, 1, self.state.texture_env_color[0].as_ptr());
        gl::Uniform1i(gl::GetUniformLocation(program, b"u_tex0\0".as_ptr() as *const _), 0);
        let position = self.state.arrays[0];
        let color = self.state.arrays[1];
        let normal = self.state.arrays[2];
        let tex0 = self.state.texcoord_arrays[0];
        self.bind_array(ATTR_POSITION, &position);
        self.bind_array(ATTR_COLOR, &color);
        self.bind_array(ATTR_NORMAL, &normal);
        self.bind_array(ATTR_TEX0, &tex0);
        gl::DrawArrays(mode, first, count);
    }
    unsafe fn DrawElements(&mut self, mode: GLenum, count: GLsizei, type_: GLenum, indices: *const GLvoid) {
        let program = match self.state.program {
            Some(program) => program,
            None => {
                let Ok(program) = create_program() else {
                    log!("Warning: GLES1-on-GLES2 shader program unavailable; skipping indexed draw");
                    return;
                };
                self.state.program = Some(program);
                program
            }
        };
        gl::UseProgram(program);
        let mvp = self.state.mvp();
        let mvp_loc = gl::GetUniformLocation(program, b"u_mvp\0".as_ptr() as *const _);
        gl::UniformMatrix4fv(mvp_loc, 1, gl::FALSE, mvp.as_ptr());
        let modelview_loc = gl::GetUniformLocation(program, b"u_modelview\0".as_ptr() as *const _);
        gl::UniformMatrix4fv(modelview_loc, 1, gl::FALSE, self.state.modelview.current.as_ptr());
        let texture_matrix_loc = gl::GetUniformLocation(program, b"u_texture_matrix0\0".as_ptr() as *const _);
        gl::UniformMatrix4fv(texture_matrix_loc, 1, gl::FALSE, self.state.texture[0].current.as_ptr());
        let color_loc = gl::GetUniformLocation(program, b"u_color\0".as_ptr() as *const _);
        gl::Uniform4fv(color_loc, 1, self.state.color.as_ptr());
        let point_size_loc = gl::GetUniformLocation(program, b"u_point_size\0".as_ptr() as *const _);
        gl::Uniform1f(point_size_loc, self.state.point_size);
        let tex_enabled = self.state.texture_enabled[0];
        let enabled_loc = gl::GetUniformLocation(program, b"u_tex_enabled0\0".as_ptr() as *const _);
        gl::Uniform1i(enabled_loc, if tex_enabled { 1 } else { 0 });
        let mode_loc = gl::GetUniformLocation(program, b"u_tex_mode0\0".as_ptr() as *const _);
        gl::Uniform1i(mode_loc, match self.state.texture_env_mode[0] as GLenum { es1::REPLACE => 1, es1::ADD => 3, es1::DECAL => 1, _ => 2 });
        gl::Uniform1i(gl::GetUniformLocation(program, b"u_tex0\0".as_ptr() as *const _), 0);
        let position = self.state.arrays[0];
        let color = self.state.arrays[1];
        let normal = self.state.arrays[2];
        let tex0 = self.state.texcoord_arrays[0];
        self.bind_array(ATTR_POSITION, &position);
        self.bind_array(ATTR_COLOR, &color);
        self.bind_array(ATTR_NORMAL, &normal);
        self.bind_array(ATTR_TEX0, &tex0);
        gl::DrawElements(mode, count, type_, indices);
    }
}

impl GLES1OnGLES2<'_> {
    unsafe fn bind_array(&mut self, index: GLuint, array: &ArrayState) {
        if !array.enabled {
            gl::DisableVertexAttribArray(index);
            let value = if index == ATTR_COLOR { self.state.color } else if index == ATTR_TEX0 { self.state.texcoords[0] } else if index == ATTR_NORMAL { [self.state.normal[0], self.state.normal[1], self.state.normal[2], 1.0] } else { [0.0, 0.0, 0.0, 1.0] };
            gl::VertexAttrib4fv(index, value.as_ptr());
            return;
        }
        gl::EnableVertexAttribArray(index);
        gl::VertexAttribPointer(index, array.size, array.type_, if array.normalized { gl::TRUE } else { gl::FALSE }, array.stride, array.pointer);
    }
}
