/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
//! `cxxabi.h` and the SjLj exception unwinder.
//!
//! Resources:
//! - [Itanium C++ ABI specification](https://itanium-cxx-abi.github.io/cxx-abi/abi.html)
//! - [SjLj-style exception unwinding overview](https://gcc.gnu.org/wiki/SjLjEH)

use crate::abi::{GuestFunction, FRAME_POINTER};
use crate::cpu::Cpu;
use crate::dyld::{export_c_func, FunctionExports};
use crate::mem::{ConstPtr, ConstVoidPtr, GuestUSize, MutPtr, MutVoidPtr, Ptr};
use crate::Environment;
use std::sync::Mutex;

// === atexit / finalize ===

static ATEXIT_HANDLERS: Mutex<Vec<(GuestFunction, MutVoidPtr, MutVoidPtr)>> =
    Mutex::new(Vec::new());

fn __cxa_atexit(_env: &mut Environment, func: GuestFunction, p: MutVoidPtr, d: MutVoidPtr) -> i32 {
    if let Ok(mut handlers) = ATEXIT_HANDLERS.lock() {
        handlers.push((func, p, d));
        0
    } else {
        -1
    }
}

fn __cxa_finalize(_env: &mut Environment, d: MutVoidPtr) {
    let mut to_run = Vec::new();
    if let Ok(mut handlers) = ATEXIT_HANDLERS.lock() {
        if d.is_null() {
            to_run = handlers.drain(..).collect();
        } else {
            let mut i = 0;
            while i < handlers.len() {
                if handlers[i].2 == d {
                    to_run.push(handlers.remove(i));
                } else {
                    i += 1;
                }
            }
        }
    }
    for (_func, _p, _d) in to_run.into_iter().rev() {
        // touchHLE relies on host-process exit for cleanup; we just drop
        // the registered destructors.
    }
}

// === C++ Itanium ABI: guard variables for static initialisation ===
//
// `__cxa_guard_acquire(guard*)` returns 1 if the guarded static still
// needs to be initialised, 0 if it's already initialised. After a
// successful initialisation the caller invokes `__cxa_guard_release`.
//
// On 32-bit ARM the guard is a 64-bit object whose first byte is the
// "initialised" flag. touchHLE is single-threaded for static init so
// we don't need locking. We pre-mark it 1 so a re-entrant call sees
// "already done".

fn __cxa_guard_acquire(env: &mut Environment, guard: MutPtr<u8>) -> i32 {
    let initialized = env.mem.read(guard);
    if initialized != 0 {
        0
    } else {
        env.mem.write(guard, 1);
        1
    }
}

fn __cxa_guard_release(env: &mut Environment, guard: MutPtr<u8>) {
    env.mem.write(guard, 1);
}

fn __cxa_guard_abort(env: &mut Environment, guard: MutPtr<u8>) {
    env.mem.write(guard, 0);
}

// === Best-effort frame-pointer recovery ===
//
// touchHLE has no real C++ unwinder. Implementing one means parsing
// .gcc_except_table LSDAs, walking the SjLj jmpbuf chain and dispatching
// to the right `catch` clause. That's a multi-week project.
//
// A number of guest termination paths can still be made survivable by
// pretending that the current function returned an error. We find its saved
// return address through the ARM frame-pointer chain, restore the caller's
// SP/FP/LR, clear R0 and branch to that address. This deliberately skips
// destructors and may leave local state inconsistent, but is preferable to
// letting an expected guest-side failure terminate the host emulator.
//
// This helper is shared with `stdlib`'s `abort`/`exit` handling. Keep it
// conservative: every frame record must be in the current thread's recorded
// stack, the chain must move toward older frames, the continuation must be in
// the main executable, and we must not cross a host-call/thread-exit
// trampoline.

const MAX_FRAME_POINTER_UNWIND: usize = 64;

/// Return the post-return SP for a readable ARM frame record at `fp`.
///
/// A normal ARM EABI frame record has the previous FP at `[fp]` and the saved
/// LR at `[fp + 4]`. `fp + 8` is the caller's SP. The caller's SP may be one
/// byte past a secondary stack's inclusive upper bound, so it is checked
/// separately from the two readable words.
fn frame_record_caller_sp(
    stack_range: &std::ops::RangeInclusive<u32>,
    fp: u32,
) -> Option<u32> {
    if fp == 0 || !fp.is_multiple_of(4) {
        return None;
    }

    let saved_lr = fp.checked_add(4)?;
    let caller_sp = fp.checked_add(8)?;
    if !stack_range.contains(&fp) || !stack_range.contains(&saved_lr) {
        return None;
    }
    if stack_range
        .end()
        .checked_add(1)
        .is_some_and(|stack_end_after| caller_sp > stack_end_after)
    {
        return None;
    }
    Some(caller_sp)
}

fn address_is_in_image(env: &Environment, address: u32) -> bool {
    let address = address & !1;
    address != 0
        && env
            .bins
            .iter()
            .any(|image| (image.text_base..image.last_segment_end).contains(&address))
}

#[cfg(test)]
mod frame_record_tests {
    use super::frame_record_caller_sp;

    #[test]
    fn accepts_a_complete_record_and_a_caller_sp_at_stack_end() {
        let stack = 0x1000u32..=0x10ff;
        assert_eq!(frame_record_caller_sp(&stack, 0x1000), Some(0x1008));
        assert_eq!(frame_record_caller_sp(&stack, 0x10f8), Some(0x1100));
    }

    #[test]
    fn rejects_incomplete_misaligned_and_overflowing_records() {
        let stack = 0x1000u32..=0x10ff;
        assert_eq!(frame_record_caller_sp(&stack, 0x0ffc), None);
        assert_eq!(frame_record_caller_sp(&stack, 0x1002), None);
        assert_eq!(frame_record_caller_sp(&stack, 0x10fc), None);

        let top_of_address_space = 0xfff0_0000u32..=u32::MAX;
        assert_eq!(
            frame_record_caller_sp(&top_of_address_space, 0xffff_fffc),
            None
        );
    }
}

fn address_is_in_main_executable(env: &Environment, address: u32) -> bool {
    let address = address & !1;
    address != 0
        && env
            .bins
            .first()
            .is_some_and(|image| (image.text_base..image.last_segment_end).contains(&address))
}

/// Best-effort non-local return to an app frame.
///
/// On success this updates the guest CPU state and returns the selected app
/// continuation. Returning `None` leaves the CPU untouched. Callers can then
/// use their normal, controlled guest-failure path instead of terminating the
/// host process.
pub(crate) fn unwind_to_app_frame(env: &mut Environment) -> Option<GuestFunction> {
    let Some(stack_range) = env
        .threads
        .get(env.current_thread)
        .and_then(|thread| thread.stack.clone())
    else {
        log!(
            "Warning: cannot recover guest control flow on thread {}: no stack range.",
            env.current_thread
        );
        return None;
    };

    let return_to_host = env.dyld.return_to_host_routine().addr_with_thumb_bit();
    let thread_exit = env.dyld.thread_exit_routine().addr_with_thumb_bit();
    let mut fp = env.cpu.regs()[FRAME_POINTER];

    for _ in 0..MAX_FRAME_POINTER_UNWIND {
        // `GuestFunction::call_from_host` makes a diagnostic frame on the
        // guest stack. Its saved LR is the interrupted guest LR rather than
        // the return-to-host sentinel, so track it explicitly instead of
        // accidentally resuming across a suspended host callback.
        if env.is_host_to_guest_stack_frame(fp) {
            break;
        }
        let Some(caller_sp) = frame_record_caller_sp(&stack_range, fp) else {
            break;
        };
        let previous_fp: u32 = env.mem.read(ConstPtr::<u32>::from_bits(fp));
        let saved_lr: u32 = env.mem.read(ConstPtr::<u32>::from_bits(fp + 4));

        // These sentinels are the boundary of a host-to-guest call or a guest
        // thread. Do not inspect older frames once one is reached: doing so
        // could resume a host callback with an unrelated guest stack.
        if saved_lr == return_to_host || saved_lr == thread_exit {
            break;
        }

        // ARM's descending stack makes older frame records higher in memory.
        // Check the next record before trusting it as the caller's FP; this
        // prevents cycles, backwards chains and arbitrary memory reads.
        let previous_frame_is_valid = previous_fp == 0
            || (previous_fp > fp
                && !env.is_host_to_guest_stack_frame(previous_fp)
                && frame_record_caller_sp(&stack_range, previous_fp).is_some());
        if !previous_frame_is_valid {
            break;
        }

        if address_is_in_main_executable(env, saved_lr) {
            // Returning from the selected frame later needs the selected
            // caller's LR, not the LR left behind by the host-function stub.
            // If that value cannot be validated, use thread-exit as the safe
            // terminal continuation rather than branching back into the
            // aborted frame.
            let caller_lr = if previous_fp == 0 {
                thread_exit
            } else {
                let candidate: u32 = env.mem.read(ConstPtr::<u32>::from_bits(previous_fp + 4));
                if candidate == return_to_host
                    || candidate == thread_exit
                    || address_is_in_image(env, candidate)
                {
                    candidate
                } else {
                    thread_exit
                }
            };

            let continuation = GuestFunction::from_addr_with_thumb_bit(saved_lr);
            let regs = env.cpu.regs_mut();
            regs[FRAME_POINTER] = previous_fp;
            regs[Cpu::SP] = caller_sp;
            regs[Cpu::LR] = caller_lr;
            regs[0] = 0;
            env.cpu.branch(continuation);
            env.note_guest_control_flow_redirect();
            return Some(continuation);
        }

        if previous_fp == 0 {
            break;
        }
        fp = previous_fp;
    }

    None
}

// === Exception-loop detection (shared) ===
//
// touchHLE's exception "bypass" can return control to a caller that
// immediately re-throws (classic example: `operator new` in a loop that
// keeps getting NULL from a refused huge `malloc`, throwing `bad_alloc`
// every iteration — see P. Harvest, which spins on malloc(0x4420000c)).
// Both the Itanium (`__cxa_throw`) and the SjLj (`_Unwind_SjLj_*`) entry
// points funnel through here so neither can hang the emulator forever.
//
// Returns the number of consecutive throws that share the same `key`.

const THROW_LOOP_LIMIT: u32 = 512;

fn note_exception_throw(key: &str) -> u32 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static LAST_THROW_KEY: Mutex<String> = Mutex::new(String::new());
    static SAME_KEY_COUNT: AtomicU32 = AtomicU32::new(0);

    let mut last = LAST_THROW_KEY.lock().unwrap();
    if *last == key {
        SAME_KEY_COUNT.fetch_add(1, Ordering::Relaxed) + 1
    } else {
        *last = key.to_owned();
        SAME_KEY_COUNT.store(1, Ordering::Relaxed);
        1
    }
}

// === C++ Itanium ABI: exception machinery ===
//
// We allocate the requested storage prefixed by a fake __cxa_exception
// header, so that pointer arithmetic in the app's exception-handling
// code (ABI offsets, exception_class field, etc.) lands inside live
// memory. We never actually free the storage — exceptions are extremely
// rare and the leak is bounded.

const CXA_EXCEPTION_HEADER_SIZE: GuestUSize = 0x60;

fn __cxa_allocate_exception(env: &mut Environment, thrown_size: GuestUSize) -> MutVoidPtr {
    let block: MutVoidPtr = env.mem.alloc(CXA_EXCEPTION_HEADER_SIZE + thrown_size);
    Ptr::from_bits(block.to_bits() + CXA_EXCEPTION_HEADER_SIZE)
}

fn __cxa_free_exception(_env: &mut Environment, _thrown: MutVoidPtr) {
    // Leak — see comment above.
}

fn __cxa_decrement_exception_refcount(_env: &mut Environment, _exception: MutVoidPtr) {}

fn __cxa_increment_exception_refcount(_env: &mut Environment, _exception: MutVoidPtr) {
    // The exception object is intentionally retained by the emulator.
}

fn __cxa_throw(env: &mut Environment, _exc: MutVoidPtr, tinfo: ConstVoidPtr, _dtor: GuestFunction) {
    // Itanium type_info layout (32-bit):
    //   +0  vptr
    //   +4  const char *name
    let type_name = if !tinfo.is_null() {
        let name_field: ConstPtr<ConstPtr<u8>> = tinfo.cast();
        let name_ptr: ConstPtr<u8> = env.mem.read(name_field + 1);
        if !name_ptr.is_null() {
            env.mem
                .cstr_at_utf8(name_ptr)
                .unwrap_or("(non-utf8)")
                .to_owned()
        } else {
            "(unknown)".to_owned()
        }
    } else {
        "(null type_info)".to_owned()
    };

    // Throw-rate limiter: if the app enters an exception loop (e.g. because
    // our SjLj bypass returns it to a `while (true) new X;` path that throws
    // again the next iteration), we'll silently burn CPU forever. Track the
    // rate of throws of the same type and abort after a reasonable ceiling so
    // the emulator stays responsive.
    let count = note_exception_throw(&type_name);

    if count <= 3 || count.is_multiple_of(64) {
        log!(
            "Guest threw a C++ exception of type {:?} (consecutive #{}): \
             touchHLE has no real unwinder, so we unwind to the nearest \
             app-level frame via the frame-pointer chain.",
            type_name,
            count
        );
    }
    if count >= THROW_LOOP_LIMIT {
        log!(
            "Warning: Exception loop detected: {} threw {} times in a row. \
             touchHLE's SjLj bypass is returning into a caller that re-throws \
             every iteration. Returning to caller to break the loop; the \
             guest will likely abort on its own shortly.",
            type_name,
            count
        );
        return;
    }

    if unwind_to_app_frame(env).is_none() {
        log!(
            "Warning: Could not unwind past C++ exception ({}); no app-level \
             frame on the stack. Returning to caller; guest will likely abort.",
            type_name
        );
    }
}

fn __cxa_rethrow(env: &mut Environment) {
    log!("__cxa_rethrow — bypassing");
    if unwind_to_app_frame(env).is_none() {
        log!(
            "Warning: Could not unwind past __cxa_rethrow; no app-level frame. \
             Returning to caller; guest will likely abort."
        );
    }
}

fn __cxa_begin_catch(_env: &mut Environment, exception_obj: MutVoidPtr) -> MutVoidPtr {
    // Return the exception object unchanged. Combined with the unwind
    // bypass above, no frame ever actually reaches __cxa_begin_catch
    // unless someone called it manually; in that case keep the value
    // sane.
    exception_obj
}

fn __cxa_end_catch(_env: &mut Environment) {}

fn __cxa_pure_virtual(env: &mut Environment) {
    log!("Pure virtual function called — vtable slot was NULL. Bypassing.");
    if unwind_to_app_frame(env).is_none() {
        log!(
            "Warning: Pure virtual function called and no recoverable frame; \
             returning to caller. Guest will likely abort."
        );
    }
}

/// `__cxa_uncaught_exception` (Itanium C++ ABI; also re-exported by
/// libc++abi). Returns whether an exception is currently in flight
/// ("thrown but not yet caught"). touchHLE's exception bypass never
/// leaves an exception in flight after `__cxa_throw` returns control to
/// an app frame, so the truthful answer is `false`. libstdc++/libc++
/// use this in `std::uncaught_exception()` and in stream/destructor
/// guards.
fn __cxa_uncaught_exception(_env: &mut Environment) -> bool {
    false
}

fn __cxa_call_unexpected(env: &mut Environment, _exc: MutVoidPtr) {
    log!("__cxa_call_unexpected — bypassing");
    if unwind_to_app_frame(env).is_none() {
        log!(
            "Warning: __cxa_call_unexpected with no recoverable frame; \
             returning to caller. Guest will likely abort."
        );
    }
}

/// Itanium ABI `__dynamic_cast`:
///
/// ```c
/// void *__dynamic_cast(const void *src,
///                      const __class_type_info *src_type,
///                      const __class_type_info *dst_type,
///                      ptrdiff_t src2dst_offset);
/// ```
///
/// Returns the casted pointer on success, or NULL on failure (the cast
/// does not apply / a `dynamic_cast<T*>` should evaluate to nullptr).
///
/// touchHLE has no real RTTI walk because every Itanium type_info vtable
/// is stubbed (see [crate::dyld::do_non_lazy_linking]). We can't ever
/// say "yes this is the right cast", so always returning NULL is the
/// only safe answer — it matches the language semantics for failed
/// casts. Apps that rely on dynamic_cast to *succeed* (rather than just
/// using it as a defensive nullptr check) will still misbehave, but
/// they were already going to crash on the broken vtables anyway.
const MAX_RTTI_SUBOBJECTS: usize = 4096;
const MAX_RTTI_BASES: u32 = 512;
const MAX_RTTI_DEPTH: u8 = 64;

#[derive(Clone, Copy)]
struct RttiSubobject {
    type_info: u32,
    object: u32,
    parent: Option<usize>,
    public_from_parent: bool,
    public_from_root: bool,
    depth: u8,
}

fn read_guest_u32(env: &Environment, address: u32) -> Option<u32> {
    if address < env.mem.null_segment_size() || address & 3 != 0 {
        return None;
    }
    let bytes = env
        .mem
        .get_bytes_fallible(ConstVoidPtr::from_bits(address), 4)?;
    Some(u32::from_le_bytes(bytes.get(..4)?.try_into().ok()?))
}

fn guest_add_signed(base: u32, offset: i32) -> Option<u32> {
    let address = i64::from(base).checked_add(i64::from(offset))?;
    (0..=i64::from(u32::MAX))
        .contains(&address)
        .then_some(address as u32)
}

fn rtti_type_name(env: &Environment, type_info: u32) -> Option<&str> {
    let name_ptr = read_guest_u32(env, type_info.checked_add(4)?)?;
    if name_ptr < env.mem.null_segment_size() {
        return None;
    }
    let bytes = env
        .mem
        .get_bytes_fallible(ConstVoidPtr::from_bits(name_ptr), 256)?;
    let length = bytes.iter().position(|byte| *byte == 0)?;
    std::str::from_utf8(&bytes[..length]).ok()
}

fn rtti_types_equal(env: &Environment, left: u32, right: u32) -> bool {
    left == right
        || matches!(
            (rtti_type_name(env, left), rtti_type_name(env, right)),
            (Some(left), Some(right)) if left == right
        )
}

fn rtti_typeinfo_kind(env: &Environment, type_info: u32) -> Option<crate::dyld::CxxAbiTypeInfoKind> {
    let vtable = read_guest_u32(env, type_info)?;
    env.dyld.cxxabi_typeinfo_kind(vtable)
}

fn rtti_base_object_address(env: &Environment, derived: u32, offset_flags: u32) -> Option<u32> {
    let flags = offset_flags & 0xff;
    let offset = (offset_flags as i32) >> 8;
    if flags & 1 == 0 {
        return guest_add_signed(derived, offset);
    }
    let vtable = read_guest_u32(env, derived)?;
    let virtual_offset_address = guest_add_signed(vtable, offset)?;
    let virtual_offset = read_guest_u32(env, virtual_offset_address)? as i32;
    guest_add_signed(derived, virtual_offset)
}

fn push_rtti_subobject(
    nodes: &mut Vec<RttiSubobject>,
    type_info: u32,
    object: u32,
    parent: usize,
    is_public: bool,
) -> Option<()> {
    if nodes.len() >= MAX_RTTI_SUBOBJECTS {
        return None;
    }
    let parent_node = nodes.get(parent)?;
    if parent_node.depth >= MAX_RTTI_DEPTH {
        return None;
    }
    let public_from_root = parent_node.public_from_root && is_public;
    let depth = parent_node.depth + 1;
    nodes.push(RttiSubobject {
        type_info,
        object,
        parent: Some(parent),
        public_from_parent: is_public,
        public_from_root,
        depth,
    });
    Some(())
}

fn rtti_subobjects(
    env: &Environment,
    dynamic_type: u32,
    dynamic_object: u32,
) -> Option<Vec<RttiSubobject>> {
    use crate::dyld::CxxAbiTypeInfoKind;

    let mut nodes = vec![RttiSubobject {
        type_info: dynamic_type,
        object: dynamic_object,
        parent: None,
        public_from_parent: true,
        public_from_root: true,
        depth: 0,
    }];
    let mut index = 0;
    while index < nodes.len() {
        let node = nodes[index];
        match rtti_typeinfo_kind(env, node.type_info)? {
            CxxAbiTypeInfoKind::Class => {}
            CxxAbiTypeInfoKind::SingleInheritance => {
                let base_type = read_guest_u32(env, node.type_info.checked_add(8)?)?;
                push_rtti_subobject(&mut nodes, base_type, node.object, index, true)?;
            }
            CxxAbiTypeInfoKind::MultipleInheritance => {
                let base_count = read_guest_u32(env, node.type_info.checked_add(12)?)?;
                if base_count > MAX_RTTI_BASES {
                    return None;
                }
                for base_index in 0..base_count {
                    let entry = node
                        .type_info
                        .checked_add(16)?
                        .checked_add(base_index.checked_mul(8)?)?;
                    let base_type = read_guest_u32(env, entry)?;
                    let offset_flags = read_guest_u32(env, entry.checked_add(4)?)?;
                    let base_object =
                        rtti_base_object_address(env, node.object, offset_flags)?;
                    push_rtti_subobject(
                        &mut nodes,
                        base_type,
                        base_object,
                        index,
                        offset_flags & 2 != 0,
                    )?;
                }
            }
        }
        index += 1;
    }
    Some(nodes)
}

fn rtti_path_is_public(nodes: &[RttiSubobject], source: usize, target: usize) -> bool {
    let mut current = target;
    while current != source {
        let Some(node) = nodes.get(current) else {
            return false;
        };
        if !node.public_from_parent {
            return false;
        }
        let Some(parent) = node.parent else {
            return false;
        };
        current = parent;
    }
    true
}

fn rtti_cast_from_subobjects<F>(
    nodes: &[RttiSubobject],
    src_type: u32,
    src_object: u32,
    dst_type: u32,
    same_type: F,
) -> Option<u32>
where
    F: Fn(u32, u32) -> bool,
{
    let sources: Vec<usize> = nodes
        .iter()
        .enumerate()
        .filter_map(|(index, node)| {
            (node.object == src_object && same_type(node.type_info, src_type)).then_some(index)
        })
        .collect();
    if sources.is_empty() {
        return None;
    }
    let targets: Vec<usize> = nodes
        .iter()
        .enumerate()
        .filter_map(|(index, node)| same_type(node.type_info, dst_type).then_some(index))
        .collect();

    let mut downcast_targets = Vec::new();
    for &target in &targets {
        if sources
            .iter()
            .any(|&source| rtti_path_is_public(nodes, target, source))
        {
            let address = nodes[target].object;
            if !downcast_targets.contains(&address) {
                downcast_targets.push(address);
            }
        }
    }
    if !downcast_targets.is_empty() {
        return (downcast_targets.len() == 1).then_some(downcast_targets[0]);
    }

    let source_is_public = sources.iter().any(|&source| nodes[source].public_from_root);
    if !source_is_public {
        return None;
    }
    let mut public_targets = Vec::new();
    for target in targets {
        if nodes[target].public_from_root {
            let address = nodes[target].object;
            if !public_targets.contains(&address) {
                public_targets.push(address);
            }
        }
    }
    if public_targets.len() == 1 {
        Some(public_targets[0])
    } else {
        None
    }
}

fn exact_dynamic_type_cast(dynamic_object: u32, offset_to_top: i32, hint: i32) -> Option<u32> {
    if hint < 0 || offset_to_top != hint.checked_neg()? {
        return None;
    }
    Some(dynamic_object)
}

fn __dynamic_cast(
    env: &mut Environment,
    src: ConstVoidPtr,
    src_type: ConstVoidPtr,
    dst_type: ConstVoidPtr,
    src2dst_offset: i32,
) -> ConstVoidPtr {
    static TRACE_COUNTER: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);
    let trace = std::env::var_os("TOUCHHLE_TRACE_DYNAMIC_CAST").is_some()
        && TRACE_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 64;
    if trace {
        log!(
            "__dynamic_cast input: src={:#010x} src_type={:#010x} ({:?}) dst_type={:#010x} ({:?}) hint={}",
            src.to_bits(),
            src_type.to_bits(),
            rtti_type_name(env, src_type.to_bits()),
            dst_type.to_bits(),
            rtti_type_name(env, dst_type.to_bits()),
            src2dst_offset
        );
    }
    if src.is_null() {
        return Ptr::null();
    }
    let src_type = src_type.to_bits();
    let dst_type = dst_type.to_bits();
    if src_type == 0 || dst_type == 0 {
        return Ptr::null();
    }
    if rtti_types_equal(env, src_type, dst_type) {
        return src;
    }

    let Some(vtable) = read_guest_u32(env, src.to_bits()) else {
        return Ptr::null();
    };
    let Some(offset_to_top_address) = vtable.checked_sub(8) else {
        return Ptr::null();
    };
    let Some(type_info_address) = vtable.checked_sub(4) else {
        return Ptr::null();
    };
    let Some(offset_to_top) = read_guest_u32(env, offset_to_top_address).map(|x| x as i32) else {
        return Ptr::null();
    };
    let Some(dynamic_type) = read_guest_u32(env, type_info_address) else {
        return Ptr::null();
    };
    let Some(dynamic_object) = guest_add_signed(src.to_bits(), offset_to_top) else {
        return Ptr::null();
    };
    if trace {
        log!(
            "__dynamic_cast object: vtable={:#010x} dynamic_type={:#010x} ({:?}) dynamic_object={:#010x} offset_to_top={}",
            vtable,
            dynamic_type,
            rtti_type_name(env, dynamic_type),
            dynamic_object,
            offset_to_top
        );
    }

    if rtti_types_equal(env, dynamic_type, dst_type) && src2dst_offset >= 0 {
        return exact_dynamic_type_cast(dynamic_object, offset_to_top, src2dst_offset)
            .map_or(Ptr::null(), ConstVoidPtr::from_bits);
    }

    let Some(nodes) = rtti_subobjects(env, dynamic_type, dynamic_object) else {
        if trace {
            log!("__dynamic_cast hierarchy decode failed for {:#010x}", dynamic_type);
        }
        return Ptr::null();
    };
    let result = rtti_cast_from_subobjects(
        &nodes,
        src_type,
        src.to_bits(),
        dst_type,
        |left, right| rtti_types_equal(env, left, right),
    );
    if trace {
        let hierarchy: Vec<_> = nodes
            .iter()
            .map(|node| {
                (
                    format!("{:#010x}", node.type_info),
                    rtti_type_name(env, node.type_info),
                    format!("{:#010x}", node.object),
                    node.parent,
                    node.public_from_parent,
                    node.public_from_root,
                )
            })
            .collect();
        log!("__dynamic_cast hierarchy={hierarchy:?} result={result:#?}");
    }
    result.map_or(Ptr::null(), ConstVoidPtr::from_bits)
}

// === SjLj unwinder entry points ===
//
// `_Unwind_SjLj_Register/Unregister` push and pop a jmpbuf onto the
// thread-local SjLj exception chain. We never actually consult the
// chain (because __cxa_throw doesn't walk it), so register/unregister
// are no-ops. `_Unwind_SjLj_RaiseException` and `_Unwind_SjLj_Resume`
// fall through to the same frame-pointer bypass as __cxa_throw.

#[allow(non_snake_case)]
fn _Unwind_SjLj_Register(_env: &mut Environment, _jmpbuf: MutVoidPtr) {}

#[allow(non_snake_case)]
fn _Unwind_SjLj_Unregister(_env: &mut Environment, _jmpbuf: MutVoidPtr) {}

#[allow(non_snake_case)]
fn _Unwind_SjLj_RaiseException(env: &mut Environment, _exc: MutVoidPtr) -> i32 {
    // Break runaway exception loops (e.g. `operator new` retrying a refused
    // huge allocation and re-throwing bad_alloc every iteration). Key on the
    // throwing call site (LR) so unrelated throws don't share a counter.
    let site = env.cpu.regs()[Cpu::LR];
    let count = note_exception_throw(&format!("sjlj:{:#x}", site));
    if count <= 3 || count.is_multiple_of(64) {
        log!(
            "_Unwind_SjLj_RaiseException — bypassing (site={:#x}, consecutive #{})",
            site,
            count
        );
    }
    if count >= THROW_LOOP_LIMIT {
        log!(
            "Warning: SjLj exception loop detected (site={:#x} raised {} times \
             in a row). Returning _URC_FATAL_PHASE1_ERROR to break the loop; \
             the guest will likely abort on its own shortly.",
            site,
            count
        );
        // _URC_FATAL_PHASE1_ERROR
        return 3;
    }
    if unwind_to_app_frame(env).is_none() {
        log!(
            "Warning: _Unwind_SjLj_RaiseException with no recoverable frame; \
             returning _URC_FATAL_PHASE1_ERROR to caller."
        );
        // _URC_FATAL_PHASE1_ERROR
        return 3;
    }
    0
}

#[allow(non_snake_case)]
fn _Unwind_SjLj_Resume(env: &mut Environment, _exc: MutVoidPtr) {
    log!("_Unwind_SjLj_Resume — bypassing");
    if unwind_to_app_frame(env).is_none() {
        log!(
            "Warning: _Unwind_SjLj_Resume with no recoverable frame; returning \
             to caller. Guest will likely abort."
        );
    }
}

#[allow(non_snake_case)]
fn _Unwind_SjLj_Resume_or_Rethrow(env: &mut Environment, _exc: MutVoidPtr) -> i32 {
    log!("_Unwind_SjLj_Resume_or_Rethrow — bypassing");
    if unwind_to_app_frame(env).is_none() {
        log!(
            "Warning: _Unwind_SjLj_Resume_or_Rethrow with no recoverable frame; \
             returning _URC_FATAL_PHASE2_ERROR."
        );
        // _URC_FATAL_PHASE2_ERROR
        return 2;
    }
    0
}

pub const FUNCTIONS: FunctionExports = &[
    export_c_func!(__cxa_atexit(_, _, _)),
    export_c_func!(__cxa_finalize(_)),
    export_c_func!(__cxa_guard_acquire(_)),
    export_c_func!(__cxa_guard_release(_)),
    export_c_func!(__cxa_guard_abort(_)),
    export_c_func!(__cxa_allocate_exception(_)),
    export_c_func!(__cxa_free_exception(_)),
    export_c_func!(__cxa_decrement_exception_refcount(_)),
    export_c_func!(__cxa_increment_exception_refcount(_)),
    export_c_func!(__cxa_throw(_, _, _)),
    export_c_func!(__cxa_rethrow()),
    export_c_func!(__cxa_begin_catch(_)),
    export_c_func!(__cxa_end_catch()),
    export_c_func!(__cxa_pure_virtual()),
    export_c_func!(__cxa_call_unexpected(_)),
    export_c_func!(__cxa_uncaught_exception()),
    export_c_func!(__dynamic_cast(_, _, _, _)),
    export_c_func!(_Unwind_SjLj_Register(_)),
    export_c_func!(_Unwind_SjLj_Unregister(_)),
    export_c_func!(_Unwind_SjLj_RaiseException(_)),
    export_c_func!(_Unwind_SjLj_Resume(_)),
    export_c_func!(_Unwind_SjLj_Resume_or_Rethrow(_)),
];

#[cfg(test)]
mod dynamic_cast_tests {
    use super::{rtti_cast_from_subobjects, RttiSubobject};

    fn node(
        type_info: u32,
        object: u32,
        parent: Option<usize>,
        public_from_parent: bool,
        public_from_root: bool,
        depth: u8,
    ) -> RttiSubobject {
        RttiSubobject {
            type_info,
            object,
            parent,
            public_from_parent,
            public_from_root,
            depth,
        }
    }

    #[test]
    fn public_base_downcasts_to_derived() {
        let nodes = vec![
            node(10, 0x1000, None, true, true, 0),
            node(20, 0x1010, Some(0), true, true, 1),
        ];
        assert_eq!(
            rtti_cast_from_subobjects(&nodes, 20, 0x1010, 10, |left, right| left == right),
            Some(0x1000)
        );
    }

    #[test]
    fn private_base_does_not_downcast() {
        let nodes = vec![
            node(10, 0x1000, None, true, true, 0),
            node(20, 0x1010, Some(0), false, false, 1),
        ];
        assert_eq!(
            rtti_cast_from_subobjects(&nodes, 20, 0x1010, 10, |left, right| left == right),
            None
        );
    }

    #[test]
    fn public_sibling_base_crosscasts() {
        let nodes = vec![
            node(30, 0x1000, None, true, true, 0),
            node(10, 0x1010, Some(0), true, true, 1),
            node(20, 0x1020, Some(0), true, true, 1),
        ];
        assert_eq!(
            rtti_cast_from_subobjects(&nodes, 10, 0x1010, 20, |left, right| left == right),
            Some(0x1020)
        );
    }

    #[test]
    fn ambiguous_public_sibling_targets_fail() {
        let nodes = vec![
            node(30, 0x1000, None, true, true, 0),
            node(20, 0x1010, Some(0), true, true, 1),
            node(10, 0x1020, Some(0), true, true, 1),
            node(10, 0x1030, Some(0), true, true, 1),
        ];
        assert_eq!(
            rtti_cast_from_subobjects(&nodes, 20, 0x1010, 10, |left, right| left == right),
            None
        );
    }
}
