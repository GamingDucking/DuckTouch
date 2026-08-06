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
use super::util::fixed_to_float;
use super::GLESContext;
use crate::window::{GLContext, GLVersion, Window};
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
    enabled: bool,
    fixed: bool,
}

impl Default for ArrayState {
    fn default() -> Self {
        Self {
            size: 4,
            type_: gl::FLOAT,
            stride: 0,
            pointer: std::ptr::null(),
            enabled: false,
            fixed: false,
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
    point_size: GLfloat,
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
            point_size: 1.0,
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
            gl::GetShaderInfoLog(shader, log.len() as GLsizei, &mut len, log.as_mut_ptr());
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
uniform vec4 u_color;
uniform float u_point_size;
varying vec4 v_color;
varying vec2 v_tex0;
void main() {
    gl_Position = u_mvp * a_position;
    gl_PointSize = u_point_size;
    v_color = a_color * u_color;
    v_tex0 = a_tex0.xy;
}
"#)?;
    let fragment = compile_shader(gl::FRAGMENT_SHADER, r#"#version 100
precision mediump float;
varying vec4 v_color;
varying vec2 v_tex0;
uniform sampler2D u_tex0;
uniform vec4 u_env_color0;
uniform int u_tex_enabled0;
uniform int u_tex_mode0;
void main() {
    vec4 color = v_color;
    if (u_tex_enabled0 != 0) {
        vec4 texel = texture2D(u_tex0, v_tex0);
        if (u_tex_mode0 == 1) color = texel;
        else if (u_tex_mode0 == 2) color = vec4(color.rgb * texel.rgb, color.a * texel.a);
        else if (u_tex_mode0 == 3) color = vec4(color.rgb + texel.rgb, color.a * texel.a);
        else color = color * texel;
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
            gl::GetProgramInfoLog(program, log.len() as GLsizei, &mut len, log.as_mut_ptr());
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
    unsafe fn Enable(&mut self, cap: GLenum) {
        if cap == es1::TEXTURE_2D {
            self.state.texture_enabled[self.state.active_texture] = true;
        } else if cap != es1::LIGHTING && cap != es1::FOG && cap != es1::ALPHA_TEST {
            gl::Enable(cap);
        }
    }
    unsafe fn Disable(&mut self, cap: GLenum) {
        if cap == es1::TEXTURE_2D {
            self.state.texture_enabled[self.state.active_texture] = false;
        } else if cap != es1::LIGHTING && cap != es1::FOG && cap != es1::ALPHA_TEST {
            gl::Disable(cap);
        }
    }
    unsafe fn IsEnabled(&mut self, cap: GLenum) -> GLboolean {
        if cap == es1::TEXTURE_2D { return if self.state.texture_enabled[self.state.active_texture] { gl::TRUE } else { gl::FALSE }; }
        gl::IsEnabled(cap)
    }
    unsafe fn ClientActiveTexture(&mut self, texture: GLenum) {
        self.state.client_active_texture = (texture - es1::TEXTURE0).min((MAX_TEXTURE_UNITS - 1) as GLenum) as usize;
    }
    unsafe fn ActiveTexture(&mut self, texture: GLenum) {
        self.state.active_texture = (texture - es1::TEXTURE0).min((MAX_TEXTURE_UNITS - 1) as GLenum) as usize;
        gl::ActiveTexture(texture);
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
    unsafe fn Color4f(&mut self, r: GLfloat, g: GLfloat, b: GLfloat, a: GLfloat) { self.state.color = [r, g, b, a]; }
    unsafe fn Color4x(&mut self, r: GLfixed, g: GLfixed, b: GLfixed, a: GLfixed) { self.Color4f(fixed_to_float(r), fixed_to_float(g), fixed_to_float(b), fixed_to_float(a)); }
    unsafe fn Color4ub(&mut self, r: GLubyte, g: GLubyte, b: GLubyte, a: GLubyte) { self.state.color = [r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, a as f32 / 255.0]; }
    unsafe fn Normal3f(&mut self, x: GLfloat, y: GLfloat, z: GLfloat) { self.state.normal = [x, y, z]; }
    unsafe fn Normal3x(&mut self, x: GLfixed, y: GLfixed, z: GLfixed) { self.Normal3f(fixed_to_float(x), fixed_to_float(y), fixed_to_float(z)); }
    unsafe fn MultiTexCoord4f(&mut self, texture: GLenum, s: GLfloat, t: GLfloat, r: GLfloat, q: GLfloat) { let i = (texture - es1::TEXTURE0).min((MAX_TEXTURE_UNITS - 1) as GLenum) as usize; self.state.texcoords[i] = [s, t, r, q]; }
    unsafe fn MultiTexCoord4x(&mut self, texture: GLenum, s: GLfixed, t: GLfixed, r: GLfixed, q: GLfixed) { self.MultiTexCoord4f(texture, fixed_to_float(s), fixed_to_float(t), fixed_to_float(r), fixed_to_float(q)); }
    unsafe fn TexCoordPointer(&mut self, size: GLint, type_: GLenum, stride: GLsizei, pointer: *const GLvoid) { let a = &mut self.state.texcoord_arrays[self.state.client_active_texture]; *a = ArrayState { size, type_, stride, pointer, enabled: a.enabled, fixed: type_ == es1::FIXED }; }
    unsafe fn ColorPointer(&mut self, size: GLint, type_: GLenum, stride: GLsizei, pointer: *const GLvoid) { let enabled = self.state.arrays[1].enabled; self.state.arrays[1] = ArrayState { size, type_, stride, pointer, enabled, fixed: type_ == es1::FIXED }; }
    unsafe fn NormalPointer(&mut self, type_: GLenum, stride: GLsizei, pointer: *const GLvoid) { let enabled = self.state.arrays[2].enabled; self.state.arrays[2] = ArrayState { size: 3, type_, stride, pointer, enabled, fixed: type_ == es1::FIXED }; }
    unsafe fn VertexPointer(&mut self, size: GLint, type_: GLenum, stride: GLsizei, pointer: *const GLvoid) { let enabled = self.state.arrays[0].enabled; self.state.arrays[0] = ArrayState { size, type_, stride, pointer, enabled, fixed: type_ == es1::FIXED }; }
    unsafe fn BindBuffer(&mut self, target: GLenum, buffer: GLuint) { gl::BindBuffer(target, buffer); }
    unsafe fn GenBuffers(&mut self, n: GLsizei, buffers: *mut GLuint) { gl::GenBuffers(n, buffers); }
    unsafe fn DeleteBuffers(&mut self, n: GLsizei, buffers: *const GLuint) { gl::DeleteBuffers(n, buffers); }
    unsafe fn BufferData(&mut self, target: GLenum, size: GLsizeiptr, data: *const GLvoid, usage: GLenum) { gl::BufferData(target, size, data, usage); }
    unsafe fn BufferSubData(&mut self, target: GLenum, offset: GLintptr, size: GLsizeiptr, data: *const GLvoid) { gl::BufferSubData(target, offset, size, data); }
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
    unsafe fn CompressedTexImage2D(&mut self, target: GLenum, level: GLint, internalformat: GLenum, width: GLsizei, height: GLsizei, border: GLint, image_size: GLsizei, data: *const GLvoid) { gl::CompressedTexImage2D(target, level, internalformat, width, height, border, image_size, data); }
    unsafe fn TexEnvi(&mut self, _target: GLenum, pname: GLenum, param: GLint) { if pname == es1::TEXTURE_ENV_MODE { self.state.texture_env_mode[self.state.active_texture] = param; } }
    unsafe fn TexEnvf(&mut self, target: GLenum, pname: GLenum, param: GLfloat) { self.TexEnvi(target, pname, param as GLint); }
    unsafe fn TexEnvx(&mut self, target: GLenum, pname: GLenum, param: GLfixed) { self.TexEnvi(target, pname, param); }
    unsafe fn TexEnviv(&mut self, target: GLenum, pname: GLenum, params: *const GLint) { self.TexEnvi(target, pname, *params); }
    unsafe fn TexEnvfv(&mut self, target: GLenum, pname: GLenum, params: *const GLfloat) { self.TexEnvi(target, pname, *params as GLint); }
    unsafe fn TexEnvxv(&mut self, target: GLenum, pname: GLenum, params: *const GLfixed) { self.TexEnvi(target, pname, *params); }
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
    unsafe fn FramebufferRenderbufferOES(&mut self, t: GLenum, a: GLenum, rt: GLenum, r: GLuint) { gl::FramebufferRenderbuffer(t, a, rt, r); }
    unsafe fn FramebufferTexture2DOES(&mut self, t: GLenum, a: GLenum, tt: GLenum, tex: GLuint, level: GLint) { gl::FramebufferTexture2D(t, a, tt, tex, level); }
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
        if array.enabled {
            gl::EnableVertexAttribArray(index);
            gl::VertexAttribPointer(index, array.size, array.type_, gl::FALSE, array.stride, array.pointer);
        } else {
            gl::DisableVertexAttribArray(index);
            let value = if index == ATTR_COLOR { self.state.color } else if index == ATTR_TEX0 { self.state.texcoords[0] } else if index == ATTR_NORMAL { [self.state.normal[0], self.state.normal[1], self.state.normal[2], 1.0] } else { [0.0, 0.0, 0.0, 1.0] };
            gl::VertexAttrib4fv(index, value.as_ptr());
        }
    }
}
