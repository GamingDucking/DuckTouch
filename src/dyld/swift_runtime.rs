/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
//! Swift runtime support for apps written in Swift (e.g. Alto's Adventure).
//!
//! Swift binaries reference the Swift runtime (`libswiftCore.dylib` and
//! friends) both through function calls (`__swift_retain`, `__swift_allocObject`,
//! …) and through data pointers (`__T0SSN` type metadata, `__T0*Ma` metadata
//! accessors, `__T0*WP` witness tables, `__T0BOWV` bridge metadata, …).
//!
//! Without linking these symbols the guest image keeps NULL in every
//! corresponding slot and the app dies during startup (Swift closures and
//! generics run at init time, long before `application:didFinishLaunching…`).
//!
//! This module provides:
//! 1. Host implementations of the handful of Swift runtime entry points that
//!    guest code actually calls on the hot path (retain/release/alloc — all
//!    effectively no-ops or small allocations under touchHLE's memory model).
//! 2. Stable per-symbol data slots for metadata/witness-table symbols (zeroed
//!    buffers; Swift type metadata is only compared for identity here).
//! 3. A pattern match for `__swift_FORCE_LOAD_$_*` autolink shims (no-ops).
//!
//! The referenced-but-unresolvable `_CNPostalAddress*Key` /
//! `_PKContactField*` NSString constants and the `PK*` ObjC classes live in
//! the `contacts` / `pass_kit` framework stubs, and the empty `libswift*`
//! dylib entries silence the "depends on missing dylib" warnings.

use crate::abi::GuestFunction;
use crate::dyld::HostFunction;
use crate::environment::Environment;
use crate::mem::{ConstVoidPtr, Mem, MutPtr, MutVoidPtr};
use std::collections::HashMap;

/// Classification of a Swift-runtime symbol for the linker.
pub enum SwiftLink {
    /// Symbol resolves to a host function trampoline.
    Function(GuestFunction),
    /// Symbol resolves to a stable data slot (metadata / witness table /
    /// prototype). The slot is zeroed and cached per symbol so that every
    /// reference to the same symbol yields the same address.
    Data(ConstVoidPtr),
}

/// Cache of Swift-runtime function trampolines, keyed by symbol name.
pub type SwiftFnCache = HashMap<String, GuestFunction>;
/// Stable per-symbol data-slot addresses for Swift metadata symbols.
pub type SwiftDataSlots = HashMap<String, u32>;

/// True if `name` looks like a Swift mangling (`__T0…` / `_$s…`).
fn is_swift_mangled(name: &str) -> bool {
    name.starts_with("__T0") || name.starts_with("_$s")
}

/// True if `name` is a Swift runtime *function* we implement.
fn is_swift_function(name: &str) -> bool {
    matches!(
        name,
        "__swift_retain"
            | "__swift_release"
            | "__swift_retain_n"
            | "__swift_release_n"
            | "__swift_slowAlloc"
            | "__swift_slowDealloc"
            | "__swift_allocObject"
            | "__swift_deallocObject"
            | "__swift_dynamicCast"
            | "__swift_isUniquelyReferenced_nonNull_native"
            | "__swift_getEnumCaseSinglePayload"
            | "__swift_storeEnumTagSinglePayload"
            | "__swift_getExistentialTypeMetadata"
            | "__swift_getInitializedObjCClass"
            | "_swift_deletedMethodError"
    ) || name.starts_with("__swift_FORCE_LOAD_$_")
        || (name.starts_with("__T0") && name.ends_with("Ma"))
}

/// True if `name` is a Swift runtime *data* symbol we can give a slot to.
fn is_swift_data_symbol(name: &str) -> bool {
    if !is_swift_mangled(name) {
        return false;
    }
    // `*Ma` accessors are functions, handled by [is_swift_function].
    if name.ends_with("Ma") {
        return false;
    }
    name.ends_with("N")      // direct type metadata (`__T0SSN`)
        || name.ends_with("Mp")  // metadata pointer (`__T0s8HashableMp`)
        || name.ends_with("WP")  // witness table pattern (`__T0SSs8HashablesWP`)
        || name.ends_with("Wvl") // protocol witness table (lazy)
        || name.ends_with("V")   // value-witness table (`__T0BOWV`)
        || name.ends_with("Ma")  // (kept out by the check above)
        || name.ends_with("M")   // metadata descriptor (`__T0SSM`)
        || name.ends_with("m")   // metadata (`__T0SSm`)
        || name.ends_with("C")   // class metadata (`__T0SC…C`)
        || name.ends_with("vp")  // field offset vector
        || name.ends_with("vpB")
        || name.ends_with("ML")  // metadata pattern for generic layouts
        || name.ends_with("XL")  // layout metadata
        || name.ends_with("Xl")
        || name.ends_with("VN")  // nominal type descriptor
        || name.ends_with("F") // function metadata
}

/// Host implementations. All follow the C-ABI-ish `fn(&mut Environment, …)`
/// shape expected by [crate::dyld::Dyld::create_guest_function].
///
/// `__swift_retain(Object)` → Object. touchHLE does not model Swift refcounts
/// (the ObjC bridge handles the app's lifetime), so this is an identity fn.
fn swift_retain(env: &mut Environment, object: MutVoidPtr) -> MutVoidPtr {
    object
}

/// `__swift_release(Object)` — no-op, see [swift_retain].
fn swift_release(_env: &mut Environment, _object: MutVoidPtr) {}

/// `__swift_retain_n(Object, n)` → Object.
fn swift_retain_n(env: &mut Environment, object: MutVoidPtr, _n: u32) -> MutVoidPtr {
    object
}

/// `__swift_release_n(Object, n)` — no-op.
fn swift_release_n(_env: &mut Environment, _object: MutVoidPtr, _n: u32) {}

/// `__swift_slowAlloc(size, align)` → zeroed memory.
///
/// Swift uses this for all box/slow-path allocations. touchHLE's guest heap
/// never frees, so `__swift_slowDealloc` is correspondingly a no-op.
fn swift_slow_alloc(env: &mut Environment, size: u32, _align: u32) -> MutVoidPtr {
    let ptr: MutVoidPtr = env.mem.alloc(size.max(1)).cast();
    let size = size.max(1) as usize;
    env.mem.bytes_at_mut(ptr.cast(), size as u32).fill(0);
    ptr
}

/// `__swift_slowDealloc(ptr, size, align)` — no-op (see [swift_slow_alloc]).
fn swift_slow_dealloc(_env: &mut Environment, _ptr: MutVoidPtr, _size: u32, _align: u32) {}

/// `__swift_allocObject(metadata, requiredSize)` → heap object.
///
/// Swift heap objects start with their metadata pointer, followed by the
/// instance payload. We zero the payload (Swift expects zero-initialized
/// boxes for many aggregate types).
fn swift_alloc_object(env: &mut Environment, metadata: ConstVoidPtr, size: u32) -> MutVoidPtr {
    let total = 4 + size as usize;
    let object: MutPtr<MutVoidPtr> = env.mem.alloc(total as u32).cast();
    env.mem.write(object, metadata.cast_mut());
    if size > 0 {
        let payload: MutPtr<u8> = MutPtr::from_bits(object.to_bits() + 4);
        env.mem.bytes_at_mut(payload, size).fill(0);
    }
    object.cast()
}

/// `__swift_deallocObject(object, size, align)` — no-op.
fn swift_dealloc_object(_env: &mut Environment, _object: MutVoidPtr, _size: u32, _align: u32) {}

/// `__swift_dynamicCast(…)` — the full dynamic-cast algorithm is far out of
/// scope; returning `nil` makes the caller take the "cast failed" branch,
/// which Swift code treats as an optional/`as?` failure instead of a crash.
fn swift_dynamic_cast(
    _env: &mut Environment,
    _value: MutVoidPtr,
    _src_type: ConstVoidPtr,
    _target_type: ConstVoidPtr,
    _flags: u32,
) -> MutVoidPtr {
    MutVoidPtr::null()
}

/// `__swift_isUniquelyReferenced_nonNull_native(object)` → `true`.
///
/// Copy-on-write buffers then always take the "mutable, unique" path, which
/// is both the cheapest and the safest outcome here.
fn swift_is_uniquely_referenced_non_null_native(
    _env: &mut Environment,
    _object: MutPtr<MutVoidPtr>,
) -> bool {
    true
}

/// `__swift_getEnumCaseSinglePayload(enum, emptyCases)` → `0`.
fn swift_get_enum_case_single_payload(
    _env: &mut Environment,
    _enum_ptr: MutVoidPtr,
    _empty_cases: u32,
) -> u32 {
    0
}

/// `__swift_storeEnumTagSinglePayload(enum, whichCase, emptyCases)` — no-op.
fn swift_store_enum_case_single_payload(
    _env: &mut Environment,
    _enum_ptr: MutVoidPtr,
    _which_case: u32,
    _empty_cases: u32,
) {
}

/// `__swift_getExistentialTypeMetadata(…)` → `nil`.
fn swift_get_existential_type_metadata(
    _env: &mut Environment,
    _flags: u32,
    _constrained_type: ConstVoidPtr,
) -> ConstVoidPtr {
    ConstVoidPtr::null()
}

/// `__swift_getInitializedObjCClass(Class)` → Class (already initialized in
/// touchHLE's ObjC runtime before guest code runs).
fn swift_get_initialized_objc_class(_env: &mut Environment, class: ConstVoidPtr) -> ConstVoidPtr {
    class
}

/// `_swift_deletedMethodError` normally traps; the guest only ever takes this
/// path if it dispatches through a stale witness table, which we've stubbed
/// as zeroed — so make it a benign "return 0" instead of killing the app.
fn swift_deleted_method_error(_env: &mut Environment, _type: ConstVoidPtr) -> u32 {
    0
}

/// `__swift_FORCE_LOAD_$_<module>` autolink shims — no-op returning 0.
fn swift_force_load(_env: &mut Environment) -> u32 {
    0
}

/// Shared implementation for all `__T0*Ma` metadata accessors: each accessor
/// returns a stable, non-NULL pointer. A single shared fallback buffer is fine
/// at stub level: this emulator's Swift apps only take the pointer for
/// identity bookkeeping and never inspect the metadata contents.
fn swift_metadata_accessor(env: &mut Environment) -> ConstVoidPtr {
    static FALLBACK: std::sync::Mutex<Option<u32>> = std::sync::Mutex::new(None);
    let mut guard = FALLBACK.lock().unwrap();
    if let Some(addr) = *guard {
        return MutPtr::<std::ffi::c_void>::from_bits(addr).cast_const();
    }
    let ptr: MutVoidPtr = env.mem.alloc(64).cast();
    env.mem.bytes_at_mut(ptr.cast(), 64).fill(0);
    *guard = Some(ptr.to_bits());
    ptr.cast_const()
}

impl crate::dyld::Dyld {
    /// Try to resolve a Swift-runtime symbol. Called from both the
    /// external-relocation and non-lazy-pointer linking loops before the
    /// "unhandled" fallback.
    pub fn swift_intercept(&mut self, mem: &mut Mem, name: &str) -> Option<SwiftLink> {
        if is_swift_function(name) {
            // Trampoline cache: same symbol → same GuestFunction.
            if let Some(cached) = self.swift_fn_cache.get(name) {
                return Some(SwiftLink::Function(*cached));
            }
            let f: HostFunction = match name {
                "__swift_retain" => {
                    &(swift_retain as fn(&mut Environment, MutVoidPtr) -> MutVoidPtr)
                }
                "__swift_release" => &(swift_release as fn(&mut Environment, MutVoidPtr)),
                "__swift_retain_n" => {
                    &(swift_retain_n as fn(&mut Environment, MutVoidPtr, u32) -> MutVoidPtr)
                }
                "__swift_release_n" => {
                    &(swift_release_n as fn(&mut Environment, MutVoidPtr, u32))
                }
                "__swift_slowAlloc" => {
                    &(swift_slow_alloc as fn(&mut Environment, u32, u32) -> MutVoidPtr)
                }
                "__swift_slowDealloc" => {
                    &(swift_slow_dealloc as fn(&mut Environment, MutVoidPtr, u32, u32))
                }
                "__swift_allocObject" => {
                    &(swift_alloc_object as fn(&mut Environment, ConstVoidPtr, u32) -> MutVoidPtr)
                }
                "__swift_deallocObject" => {
                    &(swift_dealloc_object as fn(&mut Environment, MutVoidPtr, u32, u32))
                }
                "__swift_dynamicCast" => &(swift_dynamic_cast as fn(
                    &mut Environment,
                    MutVoidPtr,
                    ConstVoidPtr,
                    ConstVoidPtr,
                    u32,
                ) -> MutVoidPtr),
                "__swift_isUniquelyReferenced_nonNull_native" => {
                    &(swift_is_uniquely_referenced_non_null_native
                        as fn(&mut Environment, MutPtr<MutVoidPtr>) -> bool)
                }
                "__swift_getEnumCaseSinglePayload" => {
                    &(swift_get_enum_case_single_payload
                        as fn(&mut Environment, MutVoidPtr, u32) -> u32)
                }
                "__swift_storeEnumTagSinglePayload" => &(swift_store_enum_case_single_payload
                    as fn(&mut Environment, MutVoidPtr, u32, u32)),
                "__swift_getExistentialTypeMetadata" => &(swift_get_existential_type_metadata
                    as fn(&mut Environment, u32, ConstVoidPtr) -> ConstVoidPtr),
                "__swift_getInitializedObjCClass" => {
                    &(swift_get_initialized_objc_class
                        as fn(&mut Environment, ConstVoidPtr) -> ConstVoidPtr)
                }
                "_swift_deletedMethodError" => {
                    &(swift_deleted_method_error as fn(&mut Environment, ConstVoidPtr) -> u32)
                }
                _ => {
                    if name.starts_with("__swift_FORCE_LOAD_$_")
                        || (name.starts_with("__T0") && name.ends_with("Ma"))
                    {
                        &(swift_metadata_accessor as fn(&mut Environment) -> ConstVoidPtr)
                    } else {
                        return None;
                    }
                }
            };
            // The linker requires a `&'static str` symbol name; intern it once
            // (bounded by the number of distinct Swift symbols in the binary).
            let symbol: &'static str = match self.swift_fn_names.get(name) {
                Some(s) => s,
                None => {
                    let s: &'static str = Box::leak(name.to_string().into_boxed_str());
                    self.swift_fn_names.insert(name.to_string(), s);
                    s
                }
            };
            let function_ptr = self.create_guest_function(mem, symbol, f);
            self.swift_fn_cache
                .insert(symbol.to_string(), function_ptr);
            return Some(SwiftLink::Function(function_ptr));
        }

        if is_swift_data_symbol(name) {
            // Stable per-symbol zeroed slot: every reference to the same
            // symbol must yield the same address (Swift type-identity
            // checks compare metadata pointers).
            if let Some(&addr) = self.swift_data_slots.get(name) {
                return Some(SwiftLink::Data(crate::mem::Ptr::<std::ffi::c_void, false>::from_bits(addr)));
            }
            let slot: MutVoidPtr = mem.alloc(64).cast();
            mem.bytes_at_mut(slot.cast(), 64).fill(0);
            self.swift_data_slots
                .insert(name.to_string(), slot.to_bits());
            return Some(SwiftLink::Data(slot.cast_const()));
        }

        None
    }
}
