/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
//! The Core Motion framework.
//!
//! Per Apple's CoreMotion Framework Reference (iOS 4.0+, available since 2.0
//! as UIAccelerometer):
//!
//! - CMMotionManager: The gateway object for accelerometer, gyroscope, and
//!   device-motion services.
//! - CMAccelerometerData: Contains a single accelerometer reading
//!   (CMAcceleration with x, y, z in G-force units).
//! - CMGyroData: Contains a single gyroscope reading (CMRotationRate with
//!   x, y, z in radians/second).
//! - CMDeviceMotion: Contains processed device-motion data combining
//!   accelerometer + gyroscope: attitude, rotationRate, gravity,
//!   userAcceleration.
//! - CMAcceleration: struct { x: f64, y: f64, z: f64 }
//! - CMRotationRate: struct { x: f64, y: f64, z: f64 }
//!
//! This implementation integrates with the SDL sensor subsystem (via the
//! window's accelerometer reading) to provide real accelerometer data when
//! available, and falls back to simulated gravity (0, 0, -1) otherwise.

use crate::abi::{impl_GuestRet_for_large_struct, GuestArg};
use crate::dyld::HostDylib;
use crate::objc::{
    autorelease, id, msg_class, nil, objc_classes, ClassExports, HostObject, NSZonePtr,
};
use crate::Environment;
use std::time::Instant;

pub const DYLIB: HostDylib = HostDylib {
    path: "/System/Library/Frameworks/CoreMotion.framework/CoreMotion",
    aliases: &[],
    class_exports: &[CLASSES],
    constant_exports: &[],
    function_exports: &[],
};

// =============================================================================
// Host object types
// =============================================================================

/// Per Apple docs: CMAcceleration is a structure with x, y, z fields
/// representing acceleration in G-force units along each axis.
/// iPhone coordinate system:
///   x: lateral (positive = right)
///   y: longitudinal (positive = up toward top of device)
///   z: perpendicular to screen (positive = toward user)
/// When device is flat on table face-up: x=0, y=0, z=-1 (gravity pulling down)
#[derive(Clone, Copy, Debug, Default, PartialEq)]
#[repr(C, packed)]
struct CMAcceleration {
    x: f64,
    y: f64,
    z: f64,
}
unsafe impl crate::mem::SafeRead for CMAcceleration {}
impl_GuestRet_for_large_struct!(CMAcceleration);
impl GuestArg for CMAcceleration {
    const REG_COUNT: usize = 6;
    fn from_regs(regs: &[u32]) -> Self {
        CMAcceleration {
            x: f64::from_regs(&regs[0..2]),
            y: f64::from_regs(&regs[2..4]),
            z: f64::from_regs(&regs[4..6]),
        }
    }
    fn to_regs(self, regs: &mut [u32]) {
        let (x, y, z) = (self.x, self.y, self.z);
        x.to_regs(&mut regs[0..2]);
        y.to_regs(&mut regs[2..4]);
        z.to_regs(&mut regs[4..6]);
    }
}

/// Per Apple docs: CMRotationRate in radians per second around each axis.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
#[repr(C, packed)]
struct CMRotationRate {
    x: f64,
    y: f64,
    z: f64,
}
unsafe impl crate::mem::SafeRead for CMRotationRate {}
impl_GuestRet_for_large_struct!(CMRotationRate);
impl GuestArg for CMRotationRate {
    const REG_COUNT: usize = 6;
    fn from_regs(regs: &[u32]) -> Self {
        CMRotationRate {
            x: f64::from_regs(&regs[0..2]),
            y: f64::from_regs(&regs[2..4]),
            z: f64::from_regs(&regs[4..6]),
        }
    }
    fn to_regs(self, regs: &mut [u32]) {
        let (x, y, z) = (self.x, self.y, self.z);
        x.to_regs(&mut regs[0..2]);
        y.to_regs(&mut regs[2..4]);
        z.to_regs(&mut regs[4..6]);
    }
}

#[derive(Default)]
struct CMMotionManagerHostObject {
    accelerometer_update_interval: f64,
    gyro_update_interval: f64,
    device_motion_update_interval: f64,
    accelerometer_active: bool,
    gyro_active: bool,
    device_motion_active: bool,
    /// Cached last accelerometer reading
    last_acceleration: CMAcceleration,
    /// Timestamp of last accelerometer update
    last_accel_timestamp: f64,
    /// Reference time for computing timestamps. `None` only for the phantom
    /// fallback created via `Default::default()`; real motion managers
    /// allocated through `+alloc` always populate this with `Instant::now()`.
    start_time: Option<Instant>,
    /// Attitude quaternion (x, y, z, w) from the complementary filter, and
    /// the timestamp of the last filter step. `None` until initialised on
    /// the first device-motion read.
    attitude_state: Option<((f64, f64, f64, f64), Instant)>,
}
impl HostObject for CMMotionManagerHostObject {}

#[derive(Default)]
struct CMAccelerometerDataHostObject {
    acceleration: CMAcceleration,
    timestamp: f64,
}
impl HostObject for CMAccelerometerDataHostObject {}

#[derive(Default)]
struct CMGyroDataHostObject {
    rotation_rate: CMRotationRate,
    timestamp: f64,
}
impl HostObject for CMGyroDataHostObject {}

#[derive(Default)]
struct CMDeviceMotionHostObject {
    /// Gravity component of acceleration
    gravity: CMAcceleration,
    /// User-generated acceleration (total minus gravity)
    user_acceleration: CMAcceleration,
    /// Rotation rate
    rotation_rate: CMRotationRate,
    timestamp: f64,
    /// Attitude from the sensor-fusion filter (pitch/roll/yaw + quaternion).
    attitude: CMAttitude,
    quaternion: CMQuaternion,
}
impl HostObject for CMDeviceMotionHostObject {}

#[derive(Clone, Copy, Debug, Default)]
struct CMAttitude {
    roll: f64,
    pitch: f64,
    yaw: f64,
}

/// Per Apple docs: CMQuaternion is (x, y, z, w) as IEEE doubles. Rotation of
/// angle theta about the unit axis (x, y, z) is represented as
/// (x*sin(theta/2), y*sin(theta/2), z*sin(theta/2), cos(theta/2)).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
#[repr(C, packed)]
struct CMQuaternion {
    x: f64,
    y: f64,
    z: f64,
    w: f64,
}
unsafe impl crate::mem::SafeRead for CMQuaternion {}
impl_GuestRet_for_large_struct!(CMQuaternion);
impl GuestArg for CMQuaternion {
    const REG_COUNT: usize = 8;
    fn from_regs(regs: &[u32]) -> Self {
        CMQuaternion {
            x: f64::from_regs(&regs[0..2]),
            y: f64::from_regs(&regs[2..4]),
            z: f64::from_regs(&regs[4..6]),
            w: f64::from_regs(&regs[6..8]),
        }
    }
    fn to_regs(self, regs: &mut [u32]) {
        let (x, y, z, w) = (self.x, self.y, self.z, self.w);
        x.to_regs(&mut regs[0..2]);
        y.to_regs(&mut regs[2..4]);
        z.to_regs(&mut regs[4..6]);
        w.to_regs(&mut regs[6..8]);
    }
}

#[derive(Default)]
struct CMAttitudeHostObject {
    attitude: CMAttitude,
    quaternion: CMQuaternion,
}
impl HostObject for CMAttitudeHostObject {}

/// Per Apple docs: CMRotationMatrix is the device attitude expressed as a 3x3
/// rotation matrix of IEEE doubles (row-major m11..m33).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
#[repr(C, packed)]
struct CMRotationMatrix {
    m11: f64,
    m12: f64,
    m13: f64,
    m21: f64,
    m22: f64,
    m23: f64,
    m31: f64,
    m32: f64,
    m33: f64,
}
unsafe impl crate::mem::SafeRead for CMRotationMatrix {}
impl_GuestRet_for_large_struct!(CMRotationMatrix);
impl GuestArg for CMRotationMatrix {
    const REG_COUNT: usize = 18;
    fn from_regs(regs: &[u32]) -> Self {
        CMRotationMatrix {
            m11: f64::from_regs(&regs[0..2]),
            m12: f64::from_regs(&regs[2..4]),
            m13: f64::from_regs(&regs[4..6]),
            m21: f64::from_regs(&regs[6..8]),
            m22: f64::from_regs(&regs[8..10]),
            m23: f64::from_regs(&regs[10..12]),
            m31: f64::from_regs(&regs[12..14]),
            m32: f64::from_regs(&regs[14..16]),
            m33: f64::from_regs(&regs[16..18]),
        }
    }
    fn to_regs(self, regs: &mut [u32]) {
        self.m11.to_regs(&mut regs[0..2]);
        self.m12.to_regs(&mut regs[2..4]);
        self.m13.to_regs(&mut regs[4..6]);
        self.m21.to_regs(&mut regs[6..8]);
        self.m22.to_regs(&mut regs[8..10]);
        self.m23.to_regs(&mut regs[10..12]);
        self.m31.to_regs(&mut regs[12..14]);
        self.m32.to_regs(&mut regs[14..16]);
        self.m33.to_regs(&mut regs[16..18]);
    }
}

/// Standard quaternion -> rotation matrix conversion.
fn quat_to_rotation_matrix(q: (f64, f64, f64, f64)) -> CMRotationMatrix {
    let (x, y, z, w) = q;
    CMRotationMatrix {
        m11: 1.0 - 2.0 * (y * y + z * z),
        m12: 2.0 * (x * y - w * z),
        m13: 2.0 * (x * z + w * y),
        m21: 2.0 * (x * y + w * z),
        m22: 1.0 - 2.0 * (x * x + z * z),
        m23: 2.0 * (y * z - w * x),
        m31: 2.0 * (x * z - w * y),
        m32: 2.0 * (y * z + w * x),
        m33: 1.0 - 2.0 * (x * x + y * y),
    }
}

/// Tait-Bryan Z-Y-X decomposition with Apple's axis mapping (pitch about the
/// device x-axis, roll about the y-axis, yaw about the z-axis).
fn quat_to_apple_angles(q: (f64, f64, f64, f64)) -> (f64, f64, f64) {
    let (x, y, z, w) = q;
    let pitch = (2.0 * (w * x + y * z)).atan2(1.0 - 2.0 * (x * x + y * y));
    let roll = (2.0 * (w * y - z * x)).clamp(-1.0, 1.0).asin();
    let yaw = (2.0 * (w * z + x * y)).atan2(1.0 - 2.0 * (y * y + z * z));
    (pitch, roll, yaw)
}

// =============================================================================
// Helper: read real accelerometer data from SDL sensor via window
// =============================================================================

/// Attempts to read the current accelerometer from the SDL sensor subsystem.
/// Returns (x, y, z) in G-force units matching iOS coordinate convention,
/// or None if no sensor is available.
fn read_sdl_accelerometer(env: &Environment) -> Option<CMAcceleration> {
    // `Window::get_acceleration` already returns the real or simulated
    // accelerometer reading in iOS G-force units (it converts SDL's m/s^2 and
    // flips the sign internally), so we just forward those values.
    let window = env.window.as_ref()?;
    let (x, y, z) = window.get_acceleration(&env.options);
    Some(CMAcceleration {
        x: x as f64,
        y: y as f64,
        z: z as f64,
    })
}

/// Returns (x, y, z) in radians per second around the device axes, matching
/// CMRotationRate's frame and units, or None if the host has no usable
/// gyroscope sensor.
fn read_sdl_gyroscope(env: &Environment) -> Option<CMRotationRate> {
    // `Window::get_rotation_rate` already returns the host gyroscope reading
    // in radians per second in the same device frame CMRotationRate uses, so
    // we just forward those values.
    let window = env.window.as_ref()?;
    let (x, y, z) = window.get_rotation_rate()?;
    Some(CMRotationRate {
        x: x as f64,
        y: y as f64,
        z: z as f64,
    })
}

// =============================================================================
// Attitude estimation (complementary gyroscope + accelerometer filter)
//
// Earlier versions derived roll/pitch with atan2() directly from the raw
// gravity vector. That is mathematically correct, but ill-conditioned exactly
// in the way phones are held while gaming (held upright in landscape, where
// gravity's z component is near zero and tiny tilts swing the estimated
// angles up to the full +/-90 degrees — the camera "snaps to max" symptom).
// Apple's own deviceMotion comes from sensor fusion and behaves smoothly in
// those holds, so we do a miniature version of the same: integrate the
// gyroscope and continuously correct the estimate towards the measured
// gravity vector. Without a host gyroscope the correction alone applies,
// which still smooths the old atan2-behaviour.
// =============================================================================

/// Smallest angular step worth applying, to avoid normalising ~zero vectors.
const ATTITUDE_EPSILON: f64 = 1.0e-9;
/// Gain [0..1] of the gravity correction applied per second of elapsed time.
const ATTITUDE_CORRECTION_PER_SEC: f64 = 8.0;
/// Gyroscope integration is skipped for steps larger than this: beyond it a
/// stale rate sample would integrate garbage (e.g. after a suspended frame).
const ATTITUDE_MAX_GYRO_DT: f64 = 0.1;

/// Rotate world-frame vector `v` into the device frame expressed by
/// attitude quaternion `q` (q maps device orientation -> world; applying the
/// conjugate gives world->device).
fn quat_world_to_device(q: (f64, f64, f64, f64), v: (f64, f64, f64)) -> (f64, f64, f64) {
    let (qx, qy, qz, qw) = q;
    // v' = q^-1 * v * q, expanded without building intermediate quaternions.
    let (vx, vy, vz) = v;
    // t = 2 * q_vec x v
    let tx = 2.0 * (qy * vz - qz * vy);
    let ty = 2.0 * (qz * vx - qx * vz);
    let tz = 2.0 * (qx * vy - qy * vx);
    // v' = v - qw * t + q_vec x t ... (conjugate form of q v q*)
    (
        vx - qw * tx + (qy * tz - qz * ty),
        vy - qw * ty + (qz * tx - qx * tz),
        vz - qw * tz + (qx * ty - qy * tx),
    )
}

/// Compute the shortest-arc quaternion rotating vector `from` onto `to`
/// (both need not be normalised). Used for gravity-vector corrections.
fn quat_shortest_arc(from: (f64, f64, f64), to: (f64, f64, f64)) -> (f64, f64, f64, f64) {
    let (fx, fy, fz) = from;
    let (tx, ty, tz) = to;
    let cross = (fy * tz - fz * ty, fz * tx - fx * tz, fx * ty - fy * tx);
    let dot = fx * tx + fy * ty + fz * tz;
    let w = ((fx * fx + fy * fy + fz * fz) * (tx * tx + ty * ty + tz * tz)).sqrt() + dot;
    let mut q = (cross.0, cross.1, cross.2, w);
    let norm = (q.0 * q.0 + q.1 * q.1 + q.2 * q.2 + q.3 * q.3).sqrt();
    if norm < ATTITUDE_EPSILON {
        // Vectors are opposite; pick any perpendicular axis.
        return (
            1.0, 0.0, 0.0, 0.0,
        );
    }
    q.0 /= norm;
    q.1 /= norm;
    q.2 /= norm;
    q.3 /= norm;
    q
}

/// Slerp-style attenuation of a corrective quaternion: scale its rotation
/// angle by `gain` in [0, 1].
fn quat_scale_angle(q: (f64, f64, f64, f64), gain: f64) -> (f64, f64, f64, f64) {
    let (qx, qy, qz, qw) = q;
    let half_angle = qw.clamp(-1.0, 1.0).acos();
    let scaled = half_angle * gain;
    let sin_scaled = scaled.sin();
    let sin_half = half_angle.sin();
    if sin_half.abs() < ATTITUDE_EPSILON {
        return (0.0, 0.0, 0.0, 1.0);
    }
    let factor = sin_scaled / sin_half;
    (
        qx * factor,
        qy * factor,
        qz * factor,
        scaled.cos(),
    )
}

/// One step of the complementary attitude filter.
///
/// - `q`: attitude quaternion (x, y, z, w) expressing device orientation.
/// - `gyro`: rotation rate about the device axes, rad/s.
/// - `accel`: total acceleration in g units (gravity + user), device frame.
/// - `dt`: elapsed time in seconds since the previous step.
///
/// Returns the updated quaternion, the gravity estimate (device frame) and
/// the user acceleration (device frame).
fn attitude_filter_step(
    q: (f64, f64, f64, f64),
    gyro: (f64, f64, f64),
    accel: (f64, f64, f64),
    dt: f64,
) -> ((f64, f64, f64, f64), (f64, f64, f64), (f64, f64, f64)) {
    let mut q = q;

    // 1. Integrate the gyroscope (body-frame rate => multiply on the right).
    if dt > 0.0 && dt <= ATTITUDE_MAX_GYRO_DT {
        let half_dt_angle_scale = dt * 0.5;
        let dq = (
            gyro.0 * half_dt_angle_scale,
            gyro.1 * half_dt_angle_scale,
            gyro.2 * half_dt_angle_scale,
            1.0,
        );
        q = quat_mul(q, dq);
        q = quat_normalized(q);
    }

    // 2. Correct towards the measured gravity vector (when sane).
    let accel_len = (accel.0 * accel.0 + accel.1 * accel.1 + accel.2 * accel.2).sqrt();
    if accel_len > 1.0e-3 {
        let measured_g = (accel.0 / accel_len, accel.1 / accel_len, accel.2 / accel_len);
        let predicted_g = quat_world_to_device(q, (0.0, 0.0, -1.0));
        // NOTE the arc direction: the implied gravity is
        // quat_world_to_device(q, ¦-z¦) = R(q)^-1(-z), and right-multiplying
        // q <- q*r rotates the implied vector by R(r)^-1. So to move the
        // predicted vector towards the measured one we need the arc that
        // maps *measured -> predicted*; its inverse then maps
        // predicted -> measured. (The other way round mirrors the estimate
        // and fights the gain every step: inverted and jerky.)
        let correction = quat_shortest_arc(measured_g, predicted_g);
        // Gain must be "per second" so behaviour doesn't depend on the
        // polling rate of the game.
        let gain = (dt * ATTITUDE_CORRECTION_PER_SEC).clamp(0.0, 1.0);
        let correction = quat_scale_angle(correction, gain);
        q = quat_mul(q, correction);
        q = quat_normalized(q);
    }

    // 3. Derived outputs.
    let gravity = quat_world_to_device(q, (0.0, 0.0, -1.0));
    let user = (accel.0 - gravity.0, accel.1 - gravity.1, accel.2 - gravity.2);
    (q, gravity, user)
}

fn quat_mul(a: (f64, f64, f64, f64), b: (f64, f64, f64, f64)) -> (f64, f64, f64, f64) {
    let (ax, ay, az, aw) = a;
    let (bx, by, bz, bw) = b;
    (
        aw * bx + ax * bw + ay * bz - az * by,
        aw * by - ax * bz + ay * bw + az * bx,
        aw * bz + ax * by - ay * bx + az * bw,
        aw * bw - ax * bx - ay * by - az * bz,
    )
}

fn quat_normalized(q: (f64, f64, f64, f64)) -> (f64, f64, f64, f64) {
    let norm = (q.0 * q.0 + q.1 * q.1 + q.2 * q.2 + q.3 * q.3).sqrt();
    if norm < ATTITUDE_EPSILON {
        (0.0, 0.0, 0.0, 1.0)
    } else {
        (q.0 / norm, q.1 / norm, q.2 / norm, q.3 / norm)
    }
}

/// Normalise a 3-vector, returning the unit vector (or (0, 0, -1) for
/// degenerate input).
fn attitude_normalize_vec(v: (f64, f64, f64)) -> (f64, f64, f64) {
    let len = (v.0 * v.0 + v.1 * v.1 + v.2 * v.2).sqrt();
    if len < ATTITUDE_EPSILON {
        (0.0, 0.0, -1.0)
    } else {
        (v.0 / len, v.1 / len, v.2 / len)
    }
}

const CLASSES: ClassExports = objc_classes! {

(env, this, _cmd);

// =============================================================================
// CMAccelerometerData
// Per Apple: Encapsulates a single accelerometer sample.
// =============================================================================

@implementation CMAccelerometerData: NSObject

+ (id)allocWithZone:(NSZonePtr)_zone {
    let host_object = Box::new(CMAccelerometerDataHostObject {
        acceleration: CMAcceleration { x: 0.0, y: 0.0, z: -1.0 },
        timestamp: 0.0,
    });
    env.objc.alloc_object(this, host_object, &mut env.mem)
}

- (CMAcceleration)acceleration {
    env.objc.borrow::<CMAccelerometerDataHostObject>(this).acceleration
}

- (f64)timestamp {
    env.objc.borrow::<CMAccelerometerDataHostObject>(this).timestamp
}

- (f64)_accelerationX {
    env.objc.borrow::<CMAccelerometerDataHostObject>(this).acceleration.x
}
- (f64)_accelerationY {
    env.objc.borrow::<CMAccelerometerDataHostObject>(this).acceleration.y
}
- (f64)_accelerationZ {
    env.objc.borrow::<CMAccelerometerDataHostObject>(this).acceleration.z
}

- (id)description {
    let host = env.objc.borrow::<CMAccelerometerDataHostObject>(this);
    let acceleration = host.acceleration;
    let (x, y, z) = (acceleration.x, acceleration.y, acceleration.z);
    let s = format!(
        "<CMAccelerometerData: timestamp={:.4} x={:.4} y={:.4} z={:.4}>",
        host.timestamp, x, y, z
    );
    let cstr = env.mem.alloc_and_write_cstr(s.as_bytes());
    msg_class![env; NSString stringWithUTF8String:cstr]
}

@end

// =============================================================================
// CMGyroData
// =============================================================================

@implementation CMGyroData: NSObject

+ (id)allocWithZone:(NSZonePtr)_zone {
    let host_object = Box::new(CMGyroDataHostObject {
        rotation_rate: CMRotationRate { x: 0.0, y: 0.0, z: 0.0 },
        timestamp: 0.0,
    });
    env.objc.alloc_object(this, host_object, &mut env.mem)
}

- (CMRotationRate)rotationRate {
    env.objc.borrow::<CMGyroDataHostObject>(this).rotation_rate
}

- (f64)timestamp {
    env.objc.borrow::<CMGyroDataHostObject>(this).timestamp
}

- (f64)_rotationRateX {
    env.objc.borrow::<CMGyroDataHostObject>(this).rotation_rate.x
}
- (f64)_rotationRateY {
    env.objc.borrow::<CMGyroDataHostObject>(this).rotation_rate.y
}
- (f64)_rotationRateZ {
    env.objc.borrow::<CMGyroDataHostObject>(this).rotation_rate.z
}

@end

// =============================================================================
// CMAttitude
// Per Apple: describes the orientation of the device as roll/pitch/yaw
// (radians), plus a rotation matrix and quaternion. With no real host gyro
// desktop/Android hosts, we derive roll/pitch from the gravity vector and
// leave yaw at zero. This is enough for games that only read the property to
// avoid the generic "does not respond to selector" path (which floods the log
// and slows the whole emulator down).
// =============================================================================

@implementation CMAttitude: NSObject

+ (id)allocWithZone:(NSZonePtr)_zone {
    let host_object = Box::new(CMAttitudeHostObject::default());
    env.objc.alloc_object(this, host_object, &mut env.mem)
}

- (f64)roll {
    env.objc.borrow::<CMAttitudeHostObject>(this).attitude.roll
}

- (f64)pitch {
    env.objc.borrow::<CMAttitudeHostObject>(this).attitude.pitch
}

- (f64)yaw {
    env.objc.borrow::<CMAttitudeHostObject>(this).attitude.yaw
}

- (CMQuaternion)quaternion {
    env.objc.borrow::<CMAttitudeHostObject>(this).quaternion
}

- (CMRotationMatrix)rotationMatrix {
    let q = env.objc.borrow::<CMAttitudeHostObject>(this).quaternion;
    quat_to_rotation_matrix((q.x, q.y, q.z, q.w))
}

// Per Apple docs, this is how apps turn the absolute world-frame attitude
// into one relative to a stored reference attitude (i.e. "calibrate" the
// neutral pose when a camera mode starts). Games' 3D tilt cameras rely on
// this — without it they fall back to the absolute attitude, which in the
// upright gaming hold sits near its extremes and makes a tiny tilt swing
// the camera wildly. In-place per Apple's signature.
- (())multiplyByInverseOfAttitude:(id)other {
    let q_ref = {
        let CMQuaternion { x, y, z, w } =
            env.objc.borrow::<CMAttitudeHostObject>(other).quaternion;
        (x, y, z, w)
    };
    let q_self = {
        let CMQuaternion { x, y, z, w } =
            env.objc.borrow::<CMAttitudeHostObject>(this).quaternion;
        (x, y, z, w)
    };
    // q_out = q_self * inverse(q_ref), and the inverse of a unit quaternion
    // is its conjugate.
    let q_ref_inv = (-q_ref.0, -q_ref.1, -q_ref.2, q_ref.3);
    let q_out = quat_normalized(quat_mul(q_self, q_ref_inv));
    let (pitch, roll, yaw) = quat_to_apple_angles(q_out);
    let host = env.objc.borrow_mut::<CMAttitudeHostObject>(this);
    host.quaternion = CMQuaternion {
        x: q_out.0,
        y: q_out.1,
        z: q_out.2,
        w: q_out.3,
    };
    host.attitude = CMAttitude { roll, pitch, yaw };
}

@end

// =============================================================================
// CMDeviceMotion
// =============================================================================

@implementation CMDeviceMotion: NSObject

+ (id)allocWithZone:(NSZonePtr)_zone {
    let host_object = Box::new(CMDeviceMotionHostObject {
        gravity: CMAcceleration { x: 0.0, y: 0.0, z: -1.0 },
        user_acceleration: CMAcceleration { x: 0.0, y: 0.0, z: 0.0 },
        rotation_rate: CMRotationRate { x: 0.0, y: 0.0, z: 0.0 },
        timestamp: 0.0,
        attitude: CMAttitude::default(),
        quaternion: CMQuaternion { x: 0.0, y: 0.0, z: 0.0, w: 1.0 },
    });
    env.objc.alloc_object(this, host_object, &mut env.mem)
}

- (id)attitude {
    let (attitude_angles, quaternion) = {
        let host = env.objc.borrow::<CMDeviceMotionHostObject>(this);
        (host.attitude, host.quaternion)
    };
    let attitude: id = msg_class![env; CMAttitude new];
    {
        let attitude_host = env.objc.borrow_mut::<CMAttitudeHostObject>(attitude);
        attitude_host.attitude = attitude_angles;
        attitude_host.quaternion = quaternion;
    }
    autorelease(env, attitude)
}

- (CMAcceleration)gravity {
    env.objc.borrow::<CMDeviceMotionHostObject>(this).gravity
}

- (CMAcceleration)userAcceleration {
    env.objc.borrow::<CMDeviceMotionHostObject>(this).user_acceleration
}

- (CMRotationRate)rotationRate {
    env.objc.borrow::<CMDeviceMotionHostObject>(this).rotation_rate
}

- (f64)timestamp {
    env.objc.borrow::<CMDeviceMotionHostObject>(this).timestamp
}

- (f64)_gravityX {
    env.objc.borrow::<CMDeviceMotionHostObject>(this).gravity.x
}
- (f64)_gravityY {
    env.objc.borrow::<CMDeviceMotionHostObject>(this).gravity.y
}
- (f64)_gravityZ {
    env.objc.borrow::<CMDeviceMotionHostObject>(this).gravity.z
}

- (f64)_userAccelerationX {
    env.objc.borrow::<CMDeviceMotionHostObject>(this).user_acceleration.x
}
- (f64)_userAccelerationY {
    env.objc.borrow::<CMDeviceMotionHostObject>(this).user_acceleration.y
}
- (f64)_userAccelerationZ {
    env.objc.borrow::<CMDeviceMotionHostObject>(this).user_acceleration.z
}

- (f64)_rotationRateX {
    env.objc.borrow::<CMDeviceMotionHostObject>(this).rotation_rate.x
}
- (f64)_rotationRateY {
    env.objc.borrow::<CMDeviceMotionHostObject>(this).rotation_rate.y
}
- (f64)_rotationRateZ {
    env.objc.borrow::<CMDeviceMotionHostObject>(this).rotation_rate.z
}

@end

// =============================================================================
// CMMotionManager
// Per Apple: The gateway object for the device's accelerometer, gyroscope,
// and device-motion services. Your app creates an instance of this class and
// uses its properties and methods to:
//   - Determine which sensors are available (isAccelerometerAvailable, etc.)
//   - Set the update interval
//   - Start/stop updates
//   - Retrieve the most recent data (accelerometerData, gyroData, deviceMotion)
// =============================================================================

@implementation CMMotionManager: NSObject

+ (id)allocWithZone:(NSZonePtr)_zone {
    let host_object = Box::new(CMMotionManagerHostObject {
        accelerometer_update_interval: 1.0 / 60.0, // 60Hz default per Apple docs
        gyro_update_interval: 1.0 / 60.0,
        device_motion_update_interval: 1.0 / 60.0,
        accelerometer_active: false,
        gyro_active: false,
        device_motion_active: false,
        last_acceleration: CMAcceleration { x: 0.0, y: 0.0, z: -1.0 },
        last_accel_timestamp: 0.0,
        start_time: Some(Instant::now()),
        attitude_state: None,
    });
    env.objc.alloc_object(this, host_object, &mut env.mem)
}

// =========================================================================
// Availability checks
// Per Apple: These indicate whether the hardware sensor is available.
// On the emulator: accelerometer and gyroscope come from SDL sensors (with a
// stationary-device stub fallback); magnetometer is not emulated.
// =========================================================================

- (bool)isAccelerometerAvailable {
    // Accelerometer is available if we have an SDL sensor or if user has a
    // mouse (virtual accelerometer via right-click).
    true
}

- (bool)isGyroAvailable {
    // The gyroscope is always reported as available. If the host exposes a
    // real gyro sensor via SDL its readings are used; otherwise readings fall
    // back to a "stationary device" stub (zero rotation rate). Apps commonly
    // gate their motion features on this flag, so a stubbed-but-available
    // gyro is preferable to reporting the hardware as absent.
    true
}

- (bool)isDeviceMotionAvailable {
    // Device motion requires both accelerometer + gyro for full fusion.
    // We can provide gravity-only device motion from accelerometer alone.
    true
}

- (bool)isMagnetometerAvailable {
    false
}

// =========================================================================
// Update intervals
// Per Apple: The interval, in seconds, for providing accelerometer updates.
// A value of 0 means updates come as fast as possible.
// =========================================================================

- (())setAccelerometerUpdateInterval:(f64)interval {
    env.objc.borrow_mut::<CMMotionManagerHostObject>(this).accelerometer_update_interval =
        if interval <= 0.0 { 1.0 / 100.0 } else { interval };
}

- (f64)accelerometerUpdateInterval {
    env.objc.borrow::<CMMotionManagerHostObject>(this).accelerometer_update_interval
}

- (())setGyroUpdateInterval:(f64)interval {
    env.objc.borrow_mut::<CMMotionManagerHostObject>(this).gyro_update_interval =
        if interval <= 0.0 { 1.0 / 100.0 } else { interval };
}

- (f64)gyroUpdateInterval {
    env.objc.borrow::<CMMotionManagerHostObject>(this).gyro_update_interval
}

- (())setDeviceMotionUpdateInterval:(f64)interval {
    env.objc.borrow_mut::<CMMotionManagerHostObject>(this).device_motion_update_interval =
        if interval <= 0.0 { 1.0 / 100.0 } else { interval };
}

- (f64)deviceMotionUpdateInterval {
    env.objc.borrow::<CMMotionManagerHostObject>(this).device_motion_update_interval
}

// =========================================================================
// Start/Stop updates (pull mode — app reads accelerometerData on demand)
// =========================================================================

- (())startAccelerometerUpdates {
    env.objc.borrow_mut::<CMMotionManagerHostObject>(this).accelerometer_active = true;
}

- (())stopAccelerometerUpdates {
    env.objc.borrow_mut::<CMMotionManagerHostObject>(this).accelerometer_active = false;
}

- (())startGyroUpdates {
    if env.window.as_ref().is_none_or(|w| !w.has_gyroscope()) {
        log!("No host gyroscope sensor; reporting a stationary device (zero rotation rate).");
    }
    env.objc.borrow_mut::<CMMotionManagerHostObject>(this).gyro_active = true;
}

- (())stopGyroUpdates {
    env.objc.borrow_mut::<CMMotionManagerHostObject>(this).gyro_active = false;
}

- (())startDeviceMotionUpdates {
    env.objc.borrow_mut::<CMMotionManagerHostObject>(this).device_motion_active = true;
}

- (())stopDeviceMotionUpdates {
    env.objc.borrow_mut::<CMMotionManagerHostObject>(this).device_motion_active = false;
}

// Push mode (handler-based) — we activate the sensor but do NOT call
// handlers since that would require implementing NSOperationQueue dispatch.
// Games that use pull-mode (polling accelerometerData) still work perfectly.
- (())startAccelerometerUpdatesToQueue:(id)_queue withHandler:(id)_handler {
    env.objc.borrow_mut::<CMMotionManagerHostObject>(this).accelerometer_active = true;
}

- (())startGyroUpdatesToQueue:(id)_queue withHandler:(id)_handler {
    if env.window.as_ref().is_none_or(|w| !w.has_gyroscope()) {
        log!("No host gyroscope sensor; reporting a stationary device (zero rotation rate).");
    }
    env.objc.borrow_mut::<CMMotionManagerHostObject>(this).gyro_active = true;
}

- (())startDeviceMotionUpdatesToQueue:(id)_queue withHandler:(id)_handler {
    env.objc.borrow_mut::<CMMotionManagerHostObject>(this).device_motion_active = true;
}

// =========================================================================
// Active state queries
// =========================================================================

- (bool)isAccelerometerActive {
    env.objc.borrow::<CMMotionManagerHostObject>(this).accelerometer_active
}

- (bool)isGyroActive {
    env.objc.borrow::<CMMotionManagerHostObject>(this).gyro_active
}

- (bool)isDeviceMotionActive {
    env.objc.borrow::<CMMotionManagerHostObject>(this).device_motion_active
}

// `-[CMMotionManager isMagnetometerActive]` — per
// <https://developer.apple.com/documentation/coremotion/cmmotionmanager/1616080-magnetometeractive>.
// touchHLE never enables the magnetometer (we report `isMagnetometerAvailable`
// as `NO`), so this always returns `NO`. Apple's documentation requires
// the property be queryable even when the hardware is absent.
- (bool)isMagnetometerActive {
    false
}

// Companion symmetric APIs for the magnetometer-pull-mode API. Apple
// documents `startMagnetometerUpdates` / `stopMagnetometerUpdates` and
// the `magnetometerData` accessor in
// <https://developer.apple.com/documentation/coremotion/cmmotionmanager>.
// With no hardware backing we still acknowledge the calls so that apps
// guarding magnetometer use behind `isMagnetometerAvailable` continue to
// run without crashing if they call these unconditionally.
- (())startMagnetometerUpdates {
}
- (())stopMagnetometerUpdates {
}
- (())startMagnetometerUpdatesToQueue:(id)_queue withHandler:(id)_handler {
}
- (id)magnetometerData {
    nil
}
- (())setMagnetometerUpdateInterval:(f64)_interval {
}
- (f64)magnetometerUpdateInterval {
    0.0
}

// =========================================================================
// Data accessors (pull mode)
// Per Apple: Returns the latest sample of accelerometer/gyro/motion data,
// or nil if updates have not been started.
// =========================================================================

- (id)accelerometerData {
    let active = env.objc.borrow::<CMMotionManagerHostObject>(this).accelerometer_active;
    if !active {
        return nil;
    }

    // Read real accelerometer data from SDL sensor, or fall back to simulated
    let accel = read_sdl_accelerometer(env)
        .unwrap_or(CMAcceleration { x: 0.0, y: 0.0, z: -1.0 });

    let timestamp = env.objc.borrow::<CMMotionManagerHostObject>(this)
        .start_time.unwrap_or_else(std::time::Instant::now).elapsed().as_secs_f64();

    // Update cached value
    {
        let host = env.objc.borrow_mut::<CMMotionManagerHostObject>(this);
        host.last_acceleration = accel;
        host.last_accel_timestamp = timestamp;
    }

    // Create and return a fresh CMAccelerometerData object
    let data: id = msg_class![env; CMAccelerometerData new];
    {
        let data_host = env.objc.borrow_mut::<CMAccelerometerDataHostObject>(data);
        data_host.acceleration = accel;
        data_host.timestamp = timestamp;
    }
    autorelease(env, data)
}

- (id)gyroData {
    let active = env.objc.borrow::<CMMotionManagerHostObject>(this).gyro_active;
    if !active {
        return nil;
    }

    // Read the host gyroscope via SDL when available; otherwise fall back to
    // a "stationary device" stub (zero rotation rate) while still reporting
    // the gyroscope as available.
    let rotation_rate = read_sdl_gyroscope(env)
        .unwrap_or(CMRotationRate { x: 0.0, y: 0.0, z: 0.0 });
    let timestamp = env.objc.borrow::<CMMotionManagerHostObject>(this)
        .start_time.unwrap_or_else(std::time::Instant::now).elapsed().as_secs_f64();

    let data: id = msg_class![env; CMGyroData new];
    {
        let data_host = env.objc.borrow_mut::<CMGyroDataHostObject>(data);
        data_host.rotation_rate = rotation_rate;
        data_host.timestamp = timestamp;
    }
    autorelease(env, data)
}

- (id)deviceMotion {
    let active = env.objc.borrow::<CMMotionManagerHostObject>(this).device_motion_active;
    if !active {
        return nil;
    }

    let accel = read_sdl_accelerometer(env)
        .unwrap_or(CMAcceleration { x: 0.0, y: 0.0, z: -1.0 });
    let rotation_rate = read_sdl_gyroscope(env)
        .unwrap_or(CMRotationRate { x: 0.0, y: 0.0, z: 0.0 });
    let timestamp = env.objc.borrow::<CMMotionManagerHostObject>(this)
        .start_time.unwrap_or_else(std::time::Instant::now).elapsed().as_secs_f64();

    // Sensor fusion: gyro-integrated attitude corrected towards gravity.
    // Without the fusion the gravity-only roll/pitch are ill-conditioned in
    // upright (gaming) holds — tiny tilts then snap the camera to full
    // deflection.
    let now = Instant::now();
    let (_attitude_q, gravity_g, user_g) = {
        let accel_t = (accel.x, accel.y, accel.z);
        let gyro_t = (rotation_rate.x, rotation_rate.y, rotation_rate.z);
        let host = env.objc.borrow_mut::<CMMotionManagerHostObject>(this);
        let (q, dt) = match host.attitude_state {
            Some((q, last)) => (q, now.duration_since(last).as_secs_f64()),
            None => {
                // Initialise so the implied gravity matches the current
                // accelerometer reading (yaw arbitrary = 0, matching the
                // XArbitraryZVertical reference frame Apple uses without a
                // magnetometer).
                let g_measured = attitude_normalize_vec(accel_t);
                (quat_shortest_arc(g_measured, (0.0, 0.0, -1.0)), 0.0)
            }
        };
        let (q, gravity_g, user_g) = attitude_filter_step(q, gyro_t, accel_t, dt);
        host.attitude_state = Some((q, now));
        (q, gravity_g, user_g)
    };

    // Report angles from the *fused* gravity vector using the same
    // conventions as before: pitch = atan2(-gy, -gz), roll = atan2(gx, -gz).
    // Fused gravity is well-behaved in upright holds because the gyroscope
    // integration carries the estimate smoothly through the |gz|≈0 poses
    // where raw-gravity atan2() blew up.
    let pitch = (-gravity_g.1).atan2(-gravity_g.2);
    let roll = gravity_g.0.atan2(-gravity_g.2);
    let yaw = 0.0;
    let (sp, cp) = (pitch / 2.0).sin_cos();
    let (sr, cr) = (roll / 2.0).sin_cos();
    let quaternion = CMQuaternion {
        x: sp * cr,
        y: cp * sr,
        z: -sp * sr,
        w: cp * cr,
    };

    let data: id = msg_class![env; CMDeviceMotion new];
    {
        let data_host = env.objc.borrow_mut::<CMDeviceMotionHostObject>(data);
        data_host.gravity = CMAcceleration {
            x: gravity_g.0,
            y: gravity_g.1,
            z: gravity_g.2,
        };
        data_host.user_acceleration = CMAcceleration {
            x: user_g.0,
            y: user_g.1,
            z: user_g.2,
        };
        data_host.rotation_rate = rotation_rate;
        data_host.timestamp = timestamp;
        data_host.attitude = CMAttitude { roll, pitch, yaw };
        data_host.quaternion = quaternion;
    }
    autorelease(env, data)
}

@end

};
