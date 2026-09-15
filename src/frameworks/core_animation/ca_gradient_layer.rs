/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
//! `CAGradientLayer`.
//!
//! Chrome (and other apps) create gradient layers via `+[CAGradientLayer
//! layer]` for fading toolbar / scroll-edge effects. touchHLE's compositor
//! doesn't render gradients yet, but the layer must be a real `CALayer`
//! subclass so it composites (showing its background color) and so property
//! accessors round-trip instead of warning "class is unimplemented".

use super::ca_layer::CALayerHostObject;
use crate::frameworks::core_graphics::CGFloat;
use crate::frameworks::foundation::ns_string::get_static_str;
use crate::objc::{id, msg, msg_class, objc_classes, release, retain, ClassExports};
use crate::Environment;
use std::collections::HashMap;

/// Gradient properties stored outside `CALayerHostObject` so we don't have
/// to touch CALayer's allocator. Retained ids are released on drop.
#[derive(Default)]
pub struct State {
    /// `CAGradientLayer*` -> (colors: `NSArray*` or nil, start: (x, y),
    /// end: (x, y), type_name)
    gradients: HashMap<id, GradientProps>,
}

#[derive(Clone, Copy)]
struct GradientProps {
    /// Normalized start/end points of the gradient axis.
    start: (CGFloat, CGFloat),
    end: (CGFloat, CGFloat),
}

impl Default for GradientProps {
    fn default() -> Self {
        // Apple's defaults: vertical gradient from top (0.5, 0.0) to
        // bottom (0.5, 1.0).
        GradientProps {
            start: (0.5, 0.0),
            end: (0.5, 1.0),
        }
    }
}

pub const CLASSES: ClassExports = objc_classes! {

(env, this, _cmd);

@implementation CAGradientLayer: CALayer

+ (id)allocWithZone:(NSZonePtr)_zone {
    let host_object = Box::new(CALayerHostObject::new());
    env.objc.alloc_object(this, host_object, &mut env.mem)
}

// CAGradientLayer's designated creation path (also used by the older
// `+[CAGradientLayer layer]` convenience constructor inherited from CALayer).
+ (id)layer {
    let layer: id = msg_class![env; CAGradientLayer alloc];
    let layer: id = msg![env; layer init];
    log_dbg!("CAGradientLayer created ({:#x})", layer.to_bits());
    layer
}

- (id)colors { // NSArray* of CGColorRef/UIColor
    let props = env.framework_state.core_animation.gradients.get(&this);
    match props {
        Some(p) => p.colors,
        None => nil,
    }
}

- (())setColors:(id)colors { // NSArray*
    retain(env, colors);
    let state = &mut env.framework_state.core_animation;
    let entry = state.gradients.entry(this).or_default();
    let old = std::mem::replace(&mut entry.colors, colors);
    release(env, old);
}

- (CGPoint)startPoint {
    env.framework_state
        .core_animation
        .gradients
        .get(&this)
        .map(|p| p.start)
        .unwrap_or(GradientProps::default().start)
}

- (())setStartPoint:(CGPoint)point {
    let state = &mut env.framework_state.core_animation;
    let entry = state.gradients.entry(this).or_default();
    entry.start = (point.x, point.y);
}

- (CGPoint)endPoint {
    env.framework_state
        .core_animation
        .gradients
        .get(&this)
        .map(|p| p.end)
        .unwrap_or(GradientProps::default().end)
}

- (())setEndPoint:(CGPoint)point {
    let state = &mut env.framework_state.core_animation;
    let entry = state.gradients.entry(this).or_default();
    entry.end = (point.x, point.y);
}

- (id)gradientType {
    // iOS 3.x-era string property ("kCAGradientLayerAxial" etc.)
    get_static_str(env, "kCAGradientLayerAxial")
}

- (())setGradientType:(id)_type {
    // Only axial gradients are meaningful without real gradient rendering.
}

- (())dealloc {
    let old = env
        .framework_state
        .core_animation
        .gradients
        .remove(&this);
    if let Some(props) = old {
        release(env, props.colors);
    }
    msg_super![env; this dealloc]
}

@end

};
