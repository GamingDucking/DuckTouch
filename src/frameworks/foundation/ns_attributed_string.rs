/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
//! `NSAttributedString` and `NSMutableAttributedString`.
//!
//! Chrome (and many other apps) use attributed strings for styled text.
//! We store the plain text plus an ordered attribute dictionary per range.
//! Styles are not rendered (our text stack draws plain text), but the
//! object model is complete: ranges, attributes, mutable editing.

use crate::abi::GuestArg;
use crate::dyld::{export_c_func, FunctionExports};
use crate::frameworks::core_foundation::CFRange;
use crate::frameworks::foundation::{ns_dictionary, ns_string, NSRange, NSInteger, NSUInteger};
use crate::mem::{ConstPtr, MutPtr, SafeRead};
use crate::frameworks::foundation::ns_string::NSUTF8StringEncoding;
use crate::objc::{id, msg, msg_class, nil, objc_classes, release, retain, ClassExports, NSZonePtr};
use crate::Environment;

/// Host object: text plus `(range, attrs)` pairs, non-overlapping, sorted.
#[derive(Default)]
pub struct NSAttributedStringHostObject {
    text: id, // NSString*
    /// Sorted by range.location.
    runs: Vec<(NSRange, id /* NSDictionary* */)>,
}
impl crate::objc::HostObject for NSAttributedStringHostObject {}

pub const CLASSES: ClassExports = objc_classes! {

(env, this, _cmd);

@implementation NSAttributedString: NSObject

+ (id)allocWithZone:(NSZonePtr)_zone {
    env.objc.alloc_object(this, Box::<NSAttributedStringHostObject>::default(), &mut env.mem)
}

+ (id)attributedStringWithString:(id)string { // NSString*
    let new: id = msg_class![env; NSAttributedString alloc];
    let new: id = msg![env; new initWithString:string];
    let _: () = msg![env; new autorelease];
    new
}

- (id)init {
    let text: id = msg_class![env; NSString new];
    msg![env; this initWithString:text]
}

- (id)initWithString:(id)string { // NSString*
    msg![env; this initWithString:string attributes:nil]
}

- (id)initWithString:(id)string attributes:(id)attributes { // (NSString*, NSDictionary*)
    if string == nil {
        let _: () = msg![env; this release];
        return nil;
    }
    let retained_string = retain(env, string);
    let retained_attrs = if attributes != nil {
        let len: NSUInteger = msg![env; string length];
        let attrs = retain(env, attributes);
        Some((NSRange { location: 0, length: len }, attrs))
    } else {
        None
    };
    let host = env.objc.borrow_mut::<NSAttributedStringHostObject>(this);
    host.text = retained_string;
    if let Some((range, attrs)) = retained_attrs {
        host.runs.push((range, attrs));
    }
    this
}

- (id)initWithAttributedString:(id)other {
    let text: id = msg![env; other string];
    msg![env; this initWithString:text]
}

- (id)initWithData:(id)data options:(id)_options documentAttributes:(MutPtr<id>)_doc_attrs error:(MutPtr<id>)_error {
    // HTML/RTF import: fall back to interpreting the data as UTF-8 text.
    let string: id = msg_class![env; NSString alloc];
    let string: id = msg![env; string initWithData:data encoding:NSUTF8StringEncoding];
    if string == nil {
        let _: () = msg![env; this release];
        return nil;
    }
    let _: () = msg![env; string autorelease];
    msg![env; this initWithString:string]
}

- (NSUInteger)length {
    let text = env.objc.borrow::<NSAttributedStringHostObject>(this).text;
    msg![env; text length]
}

- (id)string {
    env.objc.borrow::<NSAttributedStringHostObject>(this).text
}

// ---- attribute access ----

- (id)attributesAtIndex:(NSUInteger)location effectiveRange:(MutPtr<NSRange>)range_ptr {
    let host = env.objc.borrow::<NSAttributedStringHostObject>(this);
    for (range, attrs) in host.runs.iter() {
        if location >= range.location && location < range.location + range.length {
            if !range_ptr.is_null() {
                env.mem.write(range_ptr, *range);
            }
            return *attrs;
        }
    }
    if !range_ptr.is_null() {
        env.mem.write(range_ptr, NSRange { location: location, length: 0 });
    }
    nil
}

- (id)attribute:(id)name atIndex:(NSUInteger)location effectiveRange:(MutPtr<NSRange>)range_ptr {
    let attrs: id = msg![env; this attributesAtIndex:location effectiveRange:range_ptr];
    if attrs == nil { return nil; }
    msg![env; attrs objectForKey:name]
}

- (id)attributedSubstringFromRange:(NSRange)range {
    let host = env.objc.borrow::<NSAttributedStringHostObject>(this);
    let sub: id = msg![env; (host.text) substringWithRange:range];
    let new: id = msg_class![env; NSAttributedString alloc];
    let _: () = msg![env; new initWithString:sub];
    let _: () = msg![env; sub release];
    new
}

- (bool)isEqualToAttributedString:(id)other {
    if other == nil { return false; }
    let a: id = msg![env; this string];
    let b: id = msg![env; other string];
    let eq: bool = msg![env; a isEqualToString:b];
    eq
}

- (id)copyWithZone:(NSZonePtr)_zone {
    retain(env, this)
}
- (id)mutableCopyWithZone:(NSZonePtr)_zone {
    let host_text: id = msg![env; this string];
    let mutable: id = msg_class![env; NSMutableAttributedString alloc];
    let _: () = msg![env; mutable initWithString:host_text];
    mutable
}

- (())dealloc {
    let old_text;
    let old_runs;
    {
        let host = env.objc.borrow_mut::<NSAttributedStringHostObject>(this);
        old_text = std::mem::replace(&mut host.text, nil);
        old_runs = std::mem::take(&mut host.runs);
    }
    release(env, old_text);
    for (_, attrs) in old_runs {
        release(env, attrs);
    }
    env.objc.dealloc_object(this, &mut env.mem)
}

@end

@implementation NSMutableAttributedString: NSAttributedString

// ---- mutable editing ----

- (())setAttributedString:(id)other {
    let text: id = msg![env; other string];
    let new_text = retain(env, text);
    let old_text;
    let old_runs;
    {
        let host = env.objc.borrow_mut::<NSAttributedStringHostObject>(this);
        old_text = std::mem::replace(&mut host.text, new_text);
        old_runs = std::mem::take(&mut host.runs);
    }
    release(env, old_text);
    for (_, attrs) in old_runs {
        release(env, attrs);
    }
}

- (())addAttribute:(id)name value:(id)value range:(NSRange)range {
    // Merge: get existing attributes for the run covering the range start.
    let range_ptr: MutPtr<NSRange> = MutPtr::null();
    let existing: id = msg![env; this attributesAtIndex:(range.location) effectiveRange:range_ptr];
    let dict: id = if existing != nil {
        let mutable: id = msg![env; existing mutableCopy];
        let _: () = msg![env; mutable setObject:value forKey:name];
        mutable
    } else {
        let arr: id = msg_class![env; NSArray arrayWithObject:name];
        let arr2: id = msg_class![env; NSArray arrayWithObject:value];
        let _: () = msg![env; arr release];
        let d: id = msg_class![env; NSDictionary dictionaryWithObjects:arr2 forKeys:arr];
        let _: () = msg![env; arr2 release];
        d
    };
    let host = env.objc.borrow_mut::<NSAttributedStringHostObject>(this);
    host.runs.push((range, dict));
    host.runs.sort_by_key(|(r, _)| r.location);
}

- (())removeAttribute:(id)name range:(NSRange)range {
    let mut targets: Vec<id> = Vec::new();
    {
        let host = env.objc.borrow_mut::<NSAttributedStringHostObject>(this);
        for (r, attrs) in host.runs.iter_mut() {
            if r.location <= range.location && range.location + range.length <= r.location + r.length {
                targets.push(*attrs);
            }
        }
    }
    for attrs in targets {
        let _: () = msg![env; attrs removeObjectForKey:name];
    }
}

- (())replaceCharactersInRange:(NSRange)range withString:(id)string {
    let new_len: NSUInteger = msg![env; string length];
    let delta: NSInteger = new_len as NSInteger - range.length as NSInteger;
    let old_text: id = msg![env; this string];
    let new_text: id = msg![env; old_text stringByReplacingCharactersInRange:range withString:string];
    let stored_text = retain(env, new_text);
    let _ = new_text;
    {
        let host = env.objc.borrow_mut::<NSAttributedStringHostObject>(this);
        host.text = stored_text;
        for (r, _attrs) in host.runs.iter_mut() {
            if r.location >= range.location + range.length {
                r.location = (r.location as NSInteger + delta) as NSUInteger;
            } else if r.location + r.length > range.location {
                r.length = range.location.saturating_sub(r.location);
            }
        }
        host.runs.retain(|(r, _)| r.length > 0 || new_len == 0);
    }
    release(env, old_text);
}

- (())setAttributes:(id)attributes range:(NSRange)range {
    let attrs = retain(env, attributes);
    let host = env.objc.borrow_mut::<NSAttributedStringHostObject>(this);
    host.runs.push((range, attrs));
    host.runs.sort_by_key(|(r, _)| r.location);
}

@end

};
