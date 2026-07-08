/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
//! `UIEvent`.

use super::ui_touch::{touch_is_ended_or_cancelled, UITouchHostObject};
use crate::frameworks::foundation::{NSInteger, NSTimeInterval, NSUInteger};
use crate::mem::MutVoidPtr;
use crate::objc::{
    autorelease, id, msg, msg_class, nil, objc_classes, release, retain, ClassExports, HostObject,
    NSZonePtr,
};
use crate::Environment;

type UIEventType = NSUInteger;
type UIEventSubtype = NSInteger;

const UIEventTypeTouches: UIEventType = 0;
const UIEventTypePresses: UIEventType = 1;
const UIEventTypeMotion: UIEventType = 2;
const UIEventTypeRemoteControl: UIEventType = 4;

const UIEventSubtypeNone: UIEventSubtype = 0;
const UIEventSubtypeMotionShake: UIEventSubtype = 1;
const UIEventSubtypeRemoteControlPlay: UIEventSubtype = 100;
const UIEventSubtypeRemoteControlPause: UIEventSubtype = 101;
const UIEventSubtypeRemoteControlStop: UIEventSubtype = 102;
const UIEventSubtypeRemoteControlTogglePlayPause: UIEventSubtype = 103;
const UIEventSubtypeRemoteControlNextTrack: UIEventSubtype = 104;
const UIEventSubtypeRemoteControlPreviousTrack: UIEventSubtype = 105;
const UIEventSubtypeRemoteControlBeginSeekingBackward: UIEventSubtype = 106;
const UIEventSubtypeRemoteControlEndSeekingBackward: UIEventSubtype = 107;
const UIEventSubtypeRemoteControlBeginSeekingForward: UIEventSubtype = 108;
const UIEventSubtypeRemoteControlEndSeekingForward: UIEventSubtype = 109;

#[derive(Default)]
pub(super) struct UIEventHostObject {
    /// `NSSet<UITouch*>*`
    touches: id,
    timestamp: NSTimeInterval,
    /// `touchesEnded:` / `touchesCancelled:` events must expose their ended
    /// touch while dispatch is happening. Older retained Began/Moved events
    /// should not keep showing the same touch forever after it has ended.
    include_ended_touches: bool,
}
impl HostObject for UIEventHostObject {}

fn touch_visible_for_event(env: &Environment, touch: id, include_ended_touches: bool) -> bool {
    if touch == nil {
        return false;
    }
    include_ended_touches || !touch_is_ended_or_cancelled(env, touch)
}

fn filtered_touches_for_event(env: &mut Environment, touches: id, include_ended_touches: bool) -> id {
    if touches == nil {
        return nil;
    }

    let filtered: id = msg_class![env; NSMutableSet allocWithZone:(MutVoidPtr::null())];
    autorelease(env, filtered);

    let touches_arr: id = msg![env; touches allObjects];
    let count: NSUInteger = msg![env; touches_arr count];
    for i in 0..count {
        let touch: id = msg![env; touches_arr objectAtIndex:i];
        if touch_visible_for_event(env, touch, include_ended_touches) {
            let _: () = msg![env; filtered addObject:touch];
        }
    }

    filtered
}

pub const CLASSES: ClassExports = objc_classes! {

(env, this, _cmd);

@implementation UIEvent: NSObject

+ (id)allocWithZone:(NSZonePtr)_zone {
    let host_object = Box::new(UIEventHostObject {
        touches: nil,
        timestamp: 0.0,
        include_ended_touches: false,
    });
    env.objc.alloc_object(this, host_object, &mut env.mem)
}

- (())dealloc {
    let &UIEventHostObject { touches, .. } = env.objc.borrow(this);
    release(env, touches);
}

- (NSTimeInterval)timestamp {
    env.objc.borrow::<UIEventHostObject>(this).timestamp
}

- (id)touchesForView:(id)view_ {
    let &UIEventHostObject { touches, include_ended_touches, .. } = env.objc.borrow(this);

    let touches_for_view: id = msg_class![env; NSMutableSet allocWithZone:(MutVoidPtr::null())];
    autorelease(env, touches_for_view);

    let touches_arr: id = msg![env; touches allObjects];
    let touches_count: NSUInteger = msg![env; touches_arr count];
    for i in 0..touches_count {
        let touch: id = msg![env; touches_arr objectAtIndex:i];
        if !touch_visible_for_event(env, touch, include_ended_touches) {
            continue;
        }
        let &UITouchHostObject { view, .. } = env.objc.borrow(touch);
        if view_ == view {
            let _: () = msg![env; touches_for_view addObject:touch];
            if !msg![env; view isMultipleTouchEnabled] {
                break;
            }
        }
    }

    touches_for_view
}

- (id)allTouches {
    let &UIEventHostObject { touches, include_ended_touches, .. } = env.objc.borrow(this);
    filtered_touches_for_event(env, touches, include_ended_touches)
}

- (id)touchesForWindow:(id)window {
    let &UIEventHostObject { touches, include_ended_touches, .. } = env.objc.borrow(this);
    let result: id = msg_class![env; NSMutableSet allocWithZone:(MutVoidPtr::null())];
    autorelease(env, result);

    let touches_arr: id = msg![env; touches allObjects];
    let count: NSUInteger = msg![env; touches_arr count];
    for i in 0..count {
        let touch: id = msg![env; touches_arr objectAtIndex:i];
        if !touch_visible_for_event(env, touch, include_ended_touches) {
            continue;
        }
        let touch_window: id = msg![env; touch window];
        if touch_window == window {
            let _: () = msg![env; result addObject:touch];
        }
    }
    result
}

- (id)touchesForGestureRecognizer:(id)_recognizer {
    let &UIEventHostObject { touches, include_ended_touches, .. } = env.objc.borrow(this);
    filtered_touches_for_event(env, touches, include_ended_touches)
}

- (UIEventType)type {
    UIEventTypeTouches
}

- (UIEventSubtype)subtype {
    UIEventSubtypeNone
}

// TODO: more accessors

@end

};

fn new_event_with_policy(env: &mut Environment, touches: id, include_ended_touches: bool) -> id {
    let event: id = msg_class![env; UIEvent alloc];
    retain(env, touches);
    let timestamp: NSTimeInterval = {
        let process_info = msg_class![env; NSProcessInfo processInfo];
        msg![env; process_info systemUptime]
    };
    let borrow = env.objc.borrow_mut::<UIEventHostObject>(event);
    borrow.touches = touches;
    borrow.timestamp = timestamp;
    borrow.include_ended_touches = include_ended_touches;
    event
}

/// For use by [super::ui_touch]: create a `UIEvent` with a set of active `UITouch*`.
/// Ended/cancelled touches are filtered if the event is retained and queried later.
pub(super) fn new_event(env: &mut Environment, touches: id) -> id {
    new_event_with_policy(env, touches, false)
}

/// For use by [super::ui_touch] while dispatching `touchesEnded:` or
/// `touchesCancelled:`. The ended touch must still be visible during that
/// dispatch so Cocos/Kobold2D/Unity can release dragged gameplay objects.
pub(super) fn new_event_including_ended(env: &mut Environment, touches: id) -> id {
    new_event_with_policy(env, touches, true)
}
