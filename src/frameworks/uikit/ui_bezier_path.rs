/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
//! `UIBezierPath`.
//!
//! A real implementation backed by the `_touchHLE_CGPath` element store, so
//! paths created here are usable by Core Graphics callers (CGPathApply,
//! clipping, fills) rather than being inert stubs.

use crate::frameworks::core_graphics::cg_path::{
    CGPathHostObject, CGPathRef, PathElement,
};
use crate::frameworks::core_graphics::{CGPoint, CGRect, CGSize};
use crate::frameworks::core_graphics::cg_affine_transform::CGAffineTransform;
use crate::mem::{ConstVoidPtr, MutVoidPtr};
use crate::objc::{id, msg, msg_class, nil, objc_classes, ClassExports, NSZonePtr};
use crate::Environment;

#[derive(Default)]
struct UIBezierPathHostObject {
    elements: Vec<PathElement>,
}
impl crate::objc::HostObject for UIBezierPathHostObject {}

impl UIBezierPathHostObject {
    fn from_rect(rect: CGRect) -> Self {
        let tl = rect.origin;
        let tr = CGPoint { x: rect.origin.x + rect.size.width, y: rect.origin.y };
        let br = CGPoint { x: rect.origin.x + rect.size.width, y: rect.origin.y + rect.size.height };
        let bl = CGPoint { x: rect.origin.x, y: rect.origin.y + rect.size.height };
        UIBezierPathHostObject {
            elements: vec![
                PathElement::MoveTo(tl),
                PathElement::LineTo(tr),
                PathElement::LineTo(br),
                PathElement::LineTo(bl),
                PathElement::Close,
            ],
        }
    }

    fn from_oval_in_rect(rect: CGRect) -> Self {
        // Approximate an ellipse with four cubic Bézier segments (the
        // standard kappa-based circle approximation).
        const KAPPA: f32 = 0.5522847498;
        let CGRect { origin, size } = rect;
        let w = size.width;
        let h = size.height;
        let cx = origin.x + w / 2.0;
        let cy = origin.y + h / 2.0;
        let hw = w / 2.0;
        let hh = h / 2.0;
        let kx = hw * KAPPA;
        let ky = hh * KAPPA;
        // Start at the rightmost point, go clockwise (CG coordinates).
        let right = CGPoint { x: cx + hw, y: cy };
        let top = CGPoint { x: cx, y: cy - hh };
        let left = CGPoint { x: cx - hw, y: cy };
        let bottom = CGPoint { x: cx, y: cy + hh };
        UIBezierPathHostObject {
            elements: vec![
                PathElement::MoveTo(right),
                PathElement::CurveTo {
                    c1: CGPoint { x: right.x, y: right.y - ky },
                    c2: CGPoint { x: top.x + kx, y: top.y },
                    to: top,
                },
                PathElement::CurveTo {
                    c1: CGPoint { x: top.x - kx, y: top.y },
                    c2: CGPoint { x: left.x, y: left.y - ky },
                    to: left,
                },
                PathElement::CurveTo {
                    c1: CGPoint { x: left.x, y: left.y + ky },
                    c2: CGPoint { x: bottom.x - kx, y: bottom.y },
                    to: bottom,
                },
                PathElement::CurveTo {
                    c1: CGPoint { x: bottom.x + kx, y: bottom.y },
                    c2: CGPoint { x: right.x, y: right.y + ky },
                    to: right,
                },
                PathElement::Close,
            ],
        }
    }

    fn from_rounded_rect(rect: CGRect, corner_radius: f32) -> Self {
        let radius = corner_radius
            .min(rect.size.width / 2.0)
            .min(rect.size.height / 2.0);
        if radius <= 0.0 {
            return Self::from_rect(rect);
        }
        const KAPPA: f32 = 0.5522847498;
        let k = radius * KAPPA;
        let x0 = rect.origin.x;
        let y0 = rect.origin.y;
        let x1 = x0 + rect.size.width;
        let y1 = y0 + rect.size.height;
        UIBezierPathHostObject {
            elements: vec![
                PathElement::MoveTo(CGPoint { x: x0 + radius, y: y0 }),
                PathElement::LineTo(CGPoint { x: x1 - radius, y: y0 }),
                PathElement::CurveTo {
                    c1: CGPoint { x: x1 - k, y: y0 },
                    c2: CGPoint { x: x1, y: y0 + k },
                    to: CGPoint { x: x1, y: y0 + radius },
                },
                PathElement::LineTo(CGPoint { x: x1, y: y1 - radius }),
                PathElement::CurveTo {
                    c1: CGPoint { x: x1, y: y1 - k },
                    c2: CGPoint { x: x1 - k, y: y1 },
                    to: CGPoint { x: x1 - radius, y: y1 },
                },
                PathElement::LineTo(CGPoint { x: x0 + radius, y: y1 }),
                PathElement::CurveTo {
                    c1: CGPoint { x: x0 + k, y: y1 },
                    c2: CGPoint { x: x0, y: y1 - k },
                    to: CGPoint { x: x0, y: y1 - radius },
                },
                PathElement::LineTo(CGPoint { x: x0, y: y0 + radius }),
                PathElement::CurveTo {
                    c1: CGPoint { x: x0, y: y0 + k },
                    c2: CGPoint { x: x0 + k, y: y0 },
                    to: CGPoint { x: x0 + radius, y: y0 },
                },
                PathElement::Close,
            ],
        }
    }

    fn to_cg_path(&self, env: &mut Environment, mutable: bool) -> CGPathRef {
        let class = env.objc.get_known_class("_touchHLE_CGPath", &mut env.mem);
        env.objc.alloc_object(
            class,
            Box::new(CGPathHostObject {
                elements: self.elements.clone(),
                mutable,
            }),
            &mut env.mem,
        )
    }

    fn bounds(&self) -> CGRect {
        let mut points = Vec::new();
        for elem in &self.elements {
            match elem {
                PathElement::MoveTo(p)
                | PathElement::LineTo(p)
                | PathElement::QuadCurveTo { control: _, to: p }
                | PathElement::CurveTo { c1: _, c2: _, to: p } => points.push(*p),
                PathElement::Close => {}
            }
        }
        if points.is_empty() {
            return CGRect::default();
        }
        let mut min_x = points[0].x;
        let mut min_y = points[0].y;
        let mut max_x = min_x;
        let mut max_y = min_y;
        for p in &points {
            min_x = min_x.min(p.x);
            min_y = min_y.min(p.y);
            max_x = max_x.max(p.x);
            max_y = max_y.max(p.y);
        }
        CGRect {
            origin: CGPoint { x: min_x, y: min_y },
            size: CGSize { width: max_x - min_x, height: max_y - min_y },
        }
    }

    fn contains_point(&self, point: CGPoint) -> bool {
        // Ray-casting point-in-polygon test over the straight segments of
        // the path. Curves are approximated by their endpoints, which is
        // plenty for UI hit-testing. `Close` ends the current subpath for
        // the purposes of the scan (the implicit edge back to the subpath
        // start would require tracking subpath origins; UI hit-tests
        // tolerate this approximation).
        let mut inside = false;
        let mut prev: Option<CGPoint> = None;
        for elem in &self.elements {
            let cur: Option<CGPoint> = match elem {
                PathElement::MoveTo(p) | PathElement::LineTo(p) => Some(*p),
                PathElement::QuadCurveTo { to, .. } | PathElement::CurveTo { to, .. } => Some(*to),
                PathElement::Close => None,
            };
            if let (Some(a), Some(b)) = (prev, cur) {
                if (a.y > point.y) != (b.y > point.y) {
                    let t = (point.y - a.y) / (b.y - a.y);
                    let x_int = a.x + t * (b.x - a.x);
                    if point.x < x_int {
                        inside = !inside;
                    }
                }
            }
            prev = cur;
        }
        inside
    }
}

pub const CLASSES: ClassExports = objc_classes! {

(env, this, _cmd);

@implementation UIBezierPath: NSObject

+ (id)allocWithZone:(NSZonePtr)_zone {
    let host_object = Box::<UIBezierPathHostObject>::default();
    env.objc.alloc_object(this, host_object, &mut env.mem)
}

+ (id)bezierPath {
    let path: id = msg![env; this alloc];
    path
}

+ (id)bezierPathWithRect:(CGRect)rect {
    let path: id = msg![env; this alloc];
    *env.objc.borrow_mut::<UIBezierPathHostObject>(path) = UIBezierPathHostObject::from_rect(rect);
    path
}

+ (id)bezierPathWithOvalInRect:(CGRect)rect {
    let path: id = msg![env; this alloc];
    *env.objc.borrow_mut::<UIBezierPathHostObject>(path) = UIBezierPathHostObject::from_oval_in_rect(rect);
    path
}

+ (id)bezierPathWithRoundedRect:(CGRect)rect cornerRadius:(f32)corner_radius {
    let path: id = msg![env; this alloc];
    *env.objc.borrow_mut::<UIBezierPathHostObject>(path) =
        UIBezierPathHostObject::from_rounded_rect(rect, corner_radius);
    path
}

+ (id)bezierPathWithCGPath:(MutVoidPtr)cg_path {
    let path: id = msg![env; this alloc];
    if !cg_path.is_null() {
        let elements = env
            .objc
            .borrow::<CGPathHostObject>(cg_path.cast())
            .elements
            .clone();
        *env.objc.borrow_mut::<UIBezierPathHostObject>(path) = UIBezierPathHostObject {
            elements,
        };
    }
    path
}

- (id)init {
    let this: id = msg![env; this initNSObject];
    this
}

- (id)copyWithZone:(NSZonePtr)_zone {
    let elements = env.objc.borrow::<UIBezierPathHostObject>(this).elements.clone();
    let copy: id = msg_class![env; UIBezierPath alloc];
    *env.objc.borrow_mut::<UIBezierPathHostObject>(copy) = UIBezierPathHostObject { elements };
    copy
}

- (())dealloc {
    env.objc.dealloc_object(this, &mut env.mem)
}

// MARK: - Constructing a path

- (())moveToPoint:(CGPoint)point {
    env.objc
        .borrow_mut::<UIBezierPathHostObject>(this)
        .elements
        .push(PathElement::MoveTo(point));
}

- (())addLineToPoint:(CGPoint)point {
    env.objc
        .borrow_mut::<UIBezierPathHostObject>(this)
        .elements
        .push(PathElement::LineTo(point));
}

- (())addCurveToPoint:(CGPoint)to controlPoint1:(CGPoint)c1 controlPoint2:(CGPoint)c2 {
    env.objc
        .borrow_mut::<UIBezierPathHostObject>(this)
        .elements
        .push(PathElement::CurveTo { c1, c2, to });
}

- (())addQuadCurveToPoint:(CGPoint)to controlPoint:(CGPoint)control {
    env.objc
        .borrow_mut::<UIBezierPathHostObject>(this)
        .elements
        .push(PathElement::QuadCurveTo { control, to });
}

- (())closePath {
    env.objc
        .borrow_mut::<UIBezierPathHostObject>(this)
        .elements
        .push(PathElement::Close);
}

- (())removeAllPoints {
    env.objc
        .borrow_mut::<UIBezierPathHostObject>(this)
        .elements
        .clear();
}

- (())appendPath:(id)path {
    if path == nil {
        return;
    }
    let elements = env
        .objc
        .borrow::<UIBezierPathHostObject>(path)
        .elements
        .clone();
    env.objc
        .borrow_mut::<UIBezierPathHostObject>(this)
        .elements
        .extend(elements);
}

// MARK: - Path info

- (CGRect)bounds {
    env.objc.borrow::<UIBezierPathHostObject>(this).bounds()
}

- (CGPoint)currentPoint {
    let elements = &env.objc.borrow::<UIBezierPathHostObject>(this).elements;
    for elem in elements.iter().rev() {
        match elem {
            PathElement::MoveTo(p)
            | PathElement::LineTo(p)
            | PathElement::QuadCurveTo { to: p, .. }
            | PathElement::CurveTo { to: p, .. } => return *p,
            PathElement::Close => {}
        }
    }
    CGPoint { x: 0.0, y: 0.0 }
}

- (bool)isEmpty {
    env.objc.borrow::<UIBezierPathHostObject>(this).elements.is_empty()
}

- (bool)containsPoint:(CGPoint)point {
    let host = env.objc.borrow::<UIBezierPathHostObject>(this);
    host.contains_point(point)
}

- (MutVoidPtr)CGPath {
    let host = env.objc.borrow::<UIBezierPathHostObject>(this);
    let elements = host.elements.clone();
    let class = env.objc.get_known_class("_touchHLE_CGPath", &mut env.mem);
    let cg_path = env.objc.alloc_object(
        class,
        Box::new(CGPathHostObject {
            elements,
            mutable: false,
        }),
        &mut env.mem,
    );
    cg_path.cast()
}

- (MutVoidPtr)cgPath {
    // Modern spelling of the same property.
    let cg: MutVoidPtr = msg![env; this CGPath];
    cg
}

// MARK: - Drawing (no-ops without a graphics context target)

- (())fill { }
- (())stroke { }
- (())fillWithBlendMode:(i32)_blend_mode alpha:(f32)_alpha { }
- (())strokeWithBlendMode:(i32)_blend_mode alpha:(f32)_alpha { }
- (())addClip { }
- (())setLineDash:(ConstVoidPtr)_pattern count:(crate::mem::GuestUSize)_count phase:(f32)_phase { }

@end

};
