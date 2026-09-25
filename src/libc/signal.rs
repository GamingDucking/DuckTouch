/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
//! `signal.h` — signal dispositions and synchronous signal delivery.
//!
//! touchHLE does not run guest signal handlers on the host. Instead a
//! "signal" is delivered synchronously, on the guest thread that raised
//! it, exactly like a real `raise()`/`pthread_kill()` call does for the
//! calling thread. That is enough for the uses early iOS apps actually
//! have: crash reporters, Unity's unhandled-exception shim, `abort()`
//! implementations, and games that install a handler for `SIGSEGV` or
//! `SIGABRT` and then deliberately `raise()` it.
//!
//! What is *not* modelled (and is documented as such below):
//!
//! * `sigprocmask`/`pthread_sigmask` — signal masks are accepted but
//!   never block delivery, because delivery is synchronous.
//! * `SA_SIGINFO` — handlers always receive just the signal number; the
//!   `siginfo_t`/`ucontext_t` arguments are not synthesized.
//! * Asynchronous signals from outside the process (`kill()` from
//!   another process, timers, faults) — there is no other process.
//!
//! Reference: Apple's `sys/signal.h` and `signal(3)` man pages.

use crate::abi::{CallFromHost, GuestFunction};
use crate::dyld::{export_c_func, ConstantExports, FunctionExports, HostConstant};
use crate::libc::errno::{set_errno, EINVAL};
use crate::mem::{ConstPtr, ConstVoidPtr, MutPtr, MutVoidPtr, Ptr};
use crate::Environment;
use std::collections::HashMap;

/// `SIG_DFL`: the default disposition.
const SIG_DFL: u32 = 0;
/// `SIG_IGN`: the signal is ignored.
const SIG_IGN: u32 = 1;
/// `SIG_ERR`: `(void (*)(int))-1`, returned by `signal()` on failure.
const SIG_ERR: u32 = u32::MAX;

// Signal numbers shared by Darwin/iOS (`sys/signal.h`).
const SIGHUP: i32 = 1;
const SIGINT: i32 = 2;
const SIGQUIT: i32 = 3;
const SIGILL: i32 = 4;
const SIGTRAP: i32 = 5;
pub(crate) const SIGABRT: i32 = 6;
const SIGEMT: i32 = 7;
const SIGFPE: i32 = 8;
const SIGKILL: i32 = 9;
const SIGBUS: i32 = 10;
const SIGSEGV: i32 = 11;
const SIGSYS: i32 = 12;
const SIGPIPE: i32 = 13;
const SIGALRM: i32 = 14;
const SIGTERM: i32 = 15;
const SIGURG: i32 = 16;
const SIGSTOP: i32 = 17;
const SIGTSTP: i32 = 18;
const SIGCONT: i32 = 19;
const SIGCHLD: i32 = 20;
const SIGTTIN: i32 = 21;
const SIGTTOU: i32 = 22;
const SIGIO: i32 = 23;
const SIGXCPU: i32 = 24;
const SIGXFSZ: i32 = 25;
const SIGVTALRM: i32 = 26;
const SIGPROF: i32 = 27;
const SIGWINCH: i32 = 28;
const SIGINFO: i32 = 29;
const SIGUSR1: i32 = 30;
const SIGUSR2: i32 = 31;
/// `NSIG` on Apple platforms (the `_sys_siglist` table has this size).
const NSIG: i32 = 32;

// `sa_flags` bits used below (Darwin's `sys/signal.h`).
/// Restart syscalls interrupted by the signal. Stored, but touchHLE has
/// no restartable guest syscalls, so it has no observable effect.
const SA_RESTART: i32 = 0x0002;
/// Reset the disposition to `SIG_DFL` before running the handler.
const SA_RESETHAND: i32 = 0x0004;
/// The handler expects `(int, siginfo_t *, void *)` arguments.
const SA_SIGINFO: i32 = 0x0040;

/// A per-signal disposition, equivalent to Darwin's `struct sigaction`.
///
/// On 32-bit ARM/iOS the guest-visible layout is three words:
/// `__sigaction_u` (handler or `SIG_DFL`/`SIG_IGN`), `sa_mask`
/// (`sigset_t`, i.e. `uint32_t`) and `sa_flags`.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
struct SignalAction {
    /// `SIG_DFL`, `SIG_IGN`, or the address of a guest handler.
    handler: u32,
    /// `sa_mask`; recorded for `sigaction()` round-trips only.
    mask: u32,
    /// `sa_flags`; only `SA_RESETHAND` and `SA_SIGINFO` are observed.
    flags: i32,
}

/// Per-process signal state.
#[derive(Default)]
pub struct State {
    /// Dispositions set via `signal()` or `sigaction()`. Signals that
    /// were never touched have no entry and behave like `SIG_DFL`.
    actions: HashMap<i32, SignalAction>,
    /// Signals whose handler is running right now. The kernel blocks a
    /// signal while its own handler executes, so a nested `raise()` of
    /// the same signal must not re-enter the handler (it would recurse
    /// forever); re-raising it performs the default action instead.
    handling: Vec<i32>,
    /// Whether a fault-class signal (SIGSEGV/SIGBUS/SIGILL/SIGFPE/SIGTRAP)
    /// has been delivered to a guest handler this session. On real Darwin
    /// such a signal means the process already crashed; if the guest then
    /// falls through to `abort()`/`exit()`, validated frame recovery would
    /// resume it in a broken state (observed as a permanently frozen
    /// screen in Turbo Dismount), so recovery must refuse and the session
    /// must end instead.
    pub(crate) fatal_delivered: bool,
}

/// Whether `signum` is a hardware-fault signal whose Darwin default action
/// terminates the process.
fn is_fault_signal(signum: i32) -> bool {
    matches!(signum, SIGILL | SIGTRAP | SIGFPE | SIGBUS | SIGSEGV)
}

/// Whether `signum` names a real signal on this platform.
fn valid_signal(signum: i32) -> bool {
    signum > 0 && signum < NSIG
}

/// `SIGKILL` and `SIGSTOP` have no catchable or ignorable disposition.
fn uncatchable(signum: i32) -> bool {
    signum == SIGKILL || signum == SIGSTOP
}

/// The current disposition of `signum` (`SIG_DFL` when never set).
fn get_action(env: &Environment, signum: i32) -> SignalAction {
    env.libc_state
        .signal
        .actions
        .get(&signum)
        .copied()
        .unwrap_or_default()
}

/// Install `new` as the disposition of `signum`, returning the old one.
fn set_action(env: &mut Environment, signum: i32, new: SignalAction) -> SignalAction {
    env.libc_state
        .signal
        .actions
        .insert(signum, new)
        .unwrap_or_default()
}

fn sigaction(env: &mut Environment, signum: i32, act: ConstVoidPtr, old_act: MutVoidPtr) -> i32 {
    set_errno(env, 0);
    if !valid_signal(signum) || uncatchable(signum) {
        set_errno(env, EINVAL);
        return -1;
    }

    let old = get_action(env, signum);

    // POSIX fills in `old_act` with the previous disposition.
    if !old_act.is_null() {
        let out: MutPtr<u32> = old_act.cast();
        env.mem.write(out, old.handler);
        env.mem.write(out + 1, old.mask);
        env.mem.write(out + 2, old.flags as u32);
    }

    // A null `act` means "query only".
    if !act.is_null() {
        let src: ConstPtr<u32> = act.cast();
        let handler: u32 = env.mem.read(src);
        let mask: u32 = env.mem.read(src + 1);
        let flags: u32 = env.mem.read(src + 2);
        set_action(
            env,
            signum,
            SignalAction {
                handler,
                mask,
                flags: flags as i32,
            },
        );
    }

    0
}

fn signal(env: &mut Environment, signum: i32, handler: MutVoidPtr) -> MutVoidPtr {
    set_errno(env, 0);
    if !valid_signal(signum) || uncatchable(signum) {
        set_errno(env, EINVAL);
        return MutVoidPtr::from_bits(SIG_ERR);
    }

    // BSD `signal()` is `sigaction()` with `SA_RESTART` and a persistent
    // handler (it does *not* reset to `SIG_DFL` after the first signal).
    let new = SignalAction {
        handler: handler.to_bits(),
        mask: 0,
        flags: SA_RESTART,
    };
    let old = set_action(env, signum, new);
    log_dbg!(
        "signal({}, {:?}) => old handler {:#x}",
        signum,
        handler,
        old.handler
    );
    MutVoidPtr::from_bits(old.handler)
}

/// What [`raise_signal`] did with a signal.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum RaiseOutcome {
    /// The signal was ignored (`SIG_IGN`, or an ignorable default).
    Ignored,
    /// A guest handler was called and returned to us.
    HandlerCalled,
    /// The default action was taken; execution was redirected and the
    /// caller must not continue as if nothing happened.
    DefaultActionPerformed,
}

/// The Darwin default action for `signum`, performed with the same
/// controlled guest-termination machinery that `exit()`/`abort()` use.
///
/// Fatal signals must not kill the emulator process: the guest session
/// ends through the return-to-host path instead, and recovery to a valid
/// app frame is attempted first (which is what makes games that
/// deliberately `raise()` a fatal signal during, say, a DRM check
/// survive).
fn default_signal_action(env: &mut Environment, signum: i32) {
    let name = signal_name(signum);
    match signum {
        // Darwin ignores these by default.
        SIGURG | SIGCHLD | SIGIO | SIGWINCH | SIGINFO => {
            log_dbg!("Signal {} ({}) ignored (default action)", signum, name);
        }
        // Stopping would freeze the guest forever, so keep running.
        SIGSTOP | SIGTSTP | SIGTTIN | SIGTTOU => {
            log!(
                "Warning: signal {} ({}) would stop the process; continuing.",
                signum,
                name
            );
        }
        // SIGCONT only resumes a stopped process; we never stop.
        SIGCONT => {
            log_dbg!("Signal {} ({}) ignored (process is not stopped)", signum, name);
        }
        // The remaining signals terminate the process, with a core dump
        // for the fault-like ones.
        SIGHUP | SIGINT | SIGQUIT | SIGILL | SIGTRAP | SIGABRT | SIGEMT | SIGFPE | SIGKILL
        | SIGBUS | SIGSEGV | SIGSYS | SIGPIPE | SIGALRM | SIGTERM | SIGXCPU | SIGXFSZ
        | SIGVTALRM | SIGPROF | SIGUSR1 | SIGUSR2 => {
            log!(
                "Guest received fatal signal {} ({}) via raise(); ending the \
                 guest session through the return-to-host path.",
                signum,
                name
            );
            crate::libc::stdlib::recover_or_end_guest_termination(
                env,
                &format!("raise({signum}) [signal {name}]"),
            );
        }
        _ => {
            log!(
                "Warning: unhandled default action for signal {} ({}); ignoring.",
                signum,
                name
            );
        }
    }
}

/// Deliver `signum` to the calling guest thread, as `raise()` does.
///
/// Returns what happened so that callers with extra obligations (notably
/// `abort()`, which must terminate even if a handler returned) can react.
pub(crate) fn raise_signal(env: &mut Environment, signum: i32) -> RaiseOutcome {
    let action = get_action(env, signum);
    match action.handler {
        SIG_DFL => {
            default_signal_action(env, signum);
            RaiseOutcome::DefaultActionPerformed
        }
        SIG_IGN => {
            log_dbg!("Signal {} ignored (SIG_IGN)", signum);
            RaiseOutcome::Ignored
        }
        handler => {
            if env.libc_state.signal.handling.contains(&signum) {
                // Signals are blocked while their own handler runs, so a
                // nested raise() of the same signal cannot re-enter the
                // handler. Real kernels would deliver it again after the
                // handler returned; ending the guest session now is the
                // safe approximation, and it is what the crash-reporting
                // idiom (`signal(sig, SIG_DFL); raise(sig);` inside the
                // handler) would do if the handler had reset first.
                log!(
                    "Warning: signal {} ({}) was raised from inside its own \
                     handler; performing the default action instead of \
                     re-entering the handler.",
                    signum,
                    signal_name(signum)
                );
                default_signal_action(env, signum);
                return RaiseOutcome::DefaultActionPerformed;
            }
            // A one-shot handler is reset *before* the guest code runs,
            // matching the kernel's ordering (the handler itself may
            // re-install a disposition).
            if action.flags & SA_RESETHAND != 0 {
                set_action(env, signum, SignalAction::default());
            }
            if action.flags & SA_SIGINFO != 0 {
                log!(
                    "Warning: signal {} ({}) was installed with SA_SIGINFO; \
                     calling the handler with a NULL siginfo/ucontext.",
                    signum,
                    signal_name(signum)
                );
            }
            log_dbg!(
                "Calling guest signal handler {:#x} for signal {}",
                handler,
                signum
            );
            if is_fault_signal(signum) {
                env.libc_state.signal.fatal_delivered = true;
            }
            env.libc_state.signal.handling.push(signum);
            let func = GuestFunction::from_addr_with_thumb_bit(handler);
            let _: () = func.call_from_host(env, (signum,));
            // The handler usually returns; if it terminated the guest
            // session instead, this bookkeeping no longer matters.
            if let Some(position) = env
                .libc_state
                .signal
                .handling
                .iter()
                .rposition(|&signal| signal == signum)
            {
                env.libc_state.signal.handling.remove(position);
            }
            RaiseOutcome::HandlerCalled
        }
    }
}

/// `int raise(int sig)` — send `sig` to the calling thread.
///
/// Implemented on top of the disposition store above rather than as a
/// return-0 stub, so handlers installed with `signal()`/`sigaction()`
/// actually run. `errno` is set to `EINVAL` for an invalid signal.
pub(crate) fn raise(env: &mut Environment, signum: i32) -> i32 {
    set_errno(env, 0);
    if !valid_signal(signum) {
        set_errno(env, EINVAL);
        return -1;
    }
    let _ = raise_signal(env, signum);
    0
}

fn sigprocmask(env: &mut Environment, _how: i32, _set: ConstVoidPtr, _old_set: MutVoidPtr) -> i32 {
    // Signal delivery is synchronous, so blocking is not modelled; report
    // success so callers that block signals around critical sections work.
    set_errno(env, 0);
    0
}

fn sigaltstack(env: &mut Environment, _ss: ConstVoidPtr, _old_ss: MutVoidPtr) -> i32 {
    // We never run a handler on an alternate stack; accept and ignore.
    set_errno(env, 0);
    0
}

pub const FUNCTIONS: FunctionExports = &[
    export_c_func!(sigaction(_, _, _)),
    export_c_func!(signal(_, _)),
    export_c_func!(raise(_)),
    export_c_func!(sigprocmask(_, _, _)), // Строго 3 аргумента (how, set, old_set)
    export_c_func!(sigaltstack(_, _)),    // Строго 2 аргумента (ss, old_ss)
];

/// Name of `signum` for logging, matching `_sys_siglist` below.
fn signal_name(signum: i32) -> &'static str {
    if signum >= 0 && (signum as usize) < SIG_NAMES.len() {
        SIG_NAMES[signum as usize]
    } else {
        "Unknown"
    }
}

/// `const char *const sys_siglist[NSIG]` — POSIX/BSD signal-name table.
/// On Apple platforms (`sys/signal.h`) the array has `NSIG` (= 32)
/// entries. Each slot is a pointer to a C string describing the
/// signal (`"Terminated"`, `"Segmentation fault"`, …). Per the
/// libc source `<sys/signal.c>`, the values are stable and exposed
/// as a non-lazy symbol.
///
/// We build the table at first reference time by allocating 32
/// guest-side `char *` slots, each pointing at the canonical name
/// from Apple's `sys/signal.h`.
const SIG_NAMES: [&str; 32] = [
    "Unknown",
    "Hangup",
    "Interrupt",
    "Quit",
    "Illegal instruction",
    "Trace/BPT trap",
    "Abort trap",
    "EMT trap",
    "Floating point exception",
    "Killed",
    "Bus error",
    "Segmentation fault",
    "Bad system call",
    "Broken pipe",
    "Alarm clock",
    "Terminated",
    "Urgent I/O condition",
    "Suspended (signal)",
    "Suspended",
    "Continued",
    "Child exited",
    "Stopped (tty input)",
    "Stopped (tty output)",
    "I/O possible",
    "Cputime limit exceeded",
    "Filesize limit exceeded",
    "Virtual timer expired",
    "Profiling timer expired",
    "Window size changes",
    "Information request",
    "User defined signal 1",
    "User defined signal 2",
];

pub const CONSTANTS: ConstantExports = &[(
    "_sys_siglist",
    HostConstant::Custom(|env| {
        let table: crate::mem::MutPtr<u32> = env.mem.alloc(32 * 4).cast();
        for (i, &name) in SIG_NAMES.iter().enumerate() {
            // Allocate a null-terminated C string in guest memory.
            let bytes = name.as_bytes();
            let s = env.mem.alloc((bytes.len() + 1) as u32);
            env.mem
                .bytes_at_mut(s.cast(), (bytes.len() + 1) as u32)
                .copy_from_slice(
                    &bytes
                        .iter()
                        .copied()
                        .chain(std::iter::once(0u8))
                        .collect::<Vec<_>>(),
                );
            env.mem.write(table + (i as u32), s.to_bits());
        }
        Ptr::from_bits(table.to_bits())
    }),
)];
