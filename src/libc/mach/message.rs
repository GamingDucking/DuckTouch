/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
//! In-process Mach IPC for simple inline messages on allocated receive ports.
//! Supports bounded FIFO queues, COPY_SEND/MOVE_SEND/MAKE_SEND destinations,
//! send/receive timeouts, and MACH_RCV_LARGE. Waiting yields guest execution.
//! Complex descriptors, reply-right transfer, port sets, notifications, kernel
//! MIG services and non-default trailers are not implemented. Unsupported
//! requests fail explicitly instead of reporting delivery that never happened.

use std::time::{Duration, Instant};

use crate::dyld::{export_c_func, FunctionExports};
use crate::libc::mach::core_types::boolean_t;
use crate::libc::mach::mach_port::State;
use crate::libc::mach::thread_info::KERN_SUCCESS;
use crate::mem::MutVoidPtr;
use crate::Environment;

const MACH_SEND_MSG: i32 = 0x1;
const MACH_RCV_MSG: i32 = 0x2;
const MACH_RCV_LARGE: i32 = 0x4;
const MACH_SEND_TIMEOUT: i32 = 0x10;
const MACH_SEND_INTERRUPT: i32 = 0x40;
const MACH_RCV_TIMEOUT: i32 = 0x100;
const MACH_RCV_INTERRUPT: i32 = 0x400;
const MACH_SEND_INVALID_DATA: i32 = 0x10000002;
const MACH_SEND_INVALID_DEST: i32 = 0x10000003;
const MACH_SEND_TIMED_OUT: i32 = 0x10000004;
const MACH_SEND_MSG_TOO_SMALL: i32 = 0x10000008;
const MACH_SEND_TOO_LARGE: i32 = 0x1000000e;
const MACH_SEND_INVALID_TYPE: i32 = 0x1000000f;
const MACH_SEND_INVALID_HEADER: i32 = 0x10000010;
const MACH_RCV_INVALID_NAME: i32 = 0x10004002;
const MACH_RCV_TIMED_OUT: i32 = 0x10004003;
const MACH_RCV_TOO_LARGE: i32 = 0x10004004;
const MACH_RCV_PORT_DIED: i32 = 0x10004009;
const MACH_RCV_INVALID_DATA: i32 = 0x10004008;

const HEADER_SIZE: usize = 24;
const TRAILER_SIZE: usize = 8;
const QUEUE_LIMIT: usize = 5; // MACH_PORT_QLIMIT_DEFAULT
const MAX_MESSAGE_SIZE: u32 = 1024 * 1024;

fn word(bytes: &[u8], index: usize) -> u32 {
    u32::from_le_bytes(bytes[index * 4..index * 4 + 4].try_into().unwrap())
}

fn set_word(bytes: &mut [u8], index: usize, value: u32) {
    bytes[index * 4..index * 4 + 4].copy_from_slice(&value.to_le_bytes());
}

/// None means the queue is full; no rights or message bytes were consumed.
fn try_send(state: &mut State, bytes: &[u8]) -> Option<i32> {
    let bits = word(bytes, 0);
    // Only a destination disposition is supported. In particular, do not
    // silently copy out-of-line pointers as though they were inline data.
    if bits & !0xff != 0 || word(bytes, 3) != 0 {
        return Some(MACH_SEND_INVALID_TYPE);
    }
    let destination = word(bytes, 2);
    let Some(port) = state.ports.get_mut(&destination) else {
        return Some(MACH_SEND_INVALID_DEST);
    };
    match bits {
        17 | 19 if port.send_refs != 0 => (),
        20 => (),
        17 | 19 => return Some(MACH_SEND_INVALID_DEST),
        _ => return Some(MACH_SEND_INVALID_TYPE),
    }
    if port.messages.len() >= QUEUE_LIMIT {
        return None;
    }
    let mut received = bytes.to_vec();
    // On receive local_port is the destination receive right; remote_port
    // would be the reply right (none in the supported subset).
    set_word(&mut received, 0, 17 << 8); // received destination type: PORT_SEND
    set_word(&mut received, 1, bytes.len() as u32);
    set_word(&mut received, 2, 0);
    set_word(&mut received, 3, destination);
    set_word(&mut received, 4, 0); // reserved
    port.messages.push_back(received);
    if bits == 17 {
        port.send_refs -= 1;
    }
    Some(KERN_SUCCESS)
}

#[derive(Debug, PartialEq)]
enum Receive {
    Empty,
    TooLarge(u32),
    Message(Vec<u8>),
}

fn try_receive(state: &mut State, name: u32, capacity: u32, large: bool) -> Result<Receive, i32> {
    let port = state.ports.get_mut(&name).ok_or(MACH_RCV_PORT_DIED)?;
    let Some(front) = port.messages.front() else {
        return Ok(Receive::Empty);
    };
    let size = front.len();
    if ((size + 3) & !3) + TRAILER_SIZE > capacity as usize {
        if !large {
            port.messages.pop_front();
        }
        return Ok(Receive::TooLarge(size as u32));
    }
    Ok(Receive::Message(port.messages.pop_front().unwrap()))
}

fn wait(env: &mut Environment, start: Instant, timeout: Option<Duration>) -> bool {
    let quantum = Duration::from_millis(1);
    let duration = if let Some(limit) = timeout {
        let remaining = limit.saturating_sub(start.elapsed());
        if remaining.is_zero() {
            return false;
        }
        quantum.min(remaining)
    } else {
        quantum
    };
    // Guest threads are coroutines: never sleep the host OS thread here.
    env.sleep(duration);
    true
}

#[allow(clippy::too_many_arguments)]
fn mach_msg(
    env: &mut Environment,
    msg: MutVoidPtr,
    option: i32,
    send_size: u32,
    rcv_size: u32,
    rcv_name: u32,
    timeout: u32,
    notify: u32,
) -> i32 {
    let supported = MACH_SEND_MSG | MACH_RCV_MSG | MACH_RCV_LARGE
        | MACH_SEND_TIMEOUT | MACH_SEND_INTERRUPT | MACH_RCV_TIMEOUT | MACH_RCV_INTERRUPT;
    if option & !supported != 0 || notify != 0 {
        log!("mach_msg: unsupported options {:#x} or notify port {:#x}", option, notify);
        return if option & MACH_SEND_MSG != 0 {
            MACH_SEND_INVALID_HEADER
        } else {
            MACH_RCV_INVALID_DATA
        };
    }
    let limit = Duration::from_millis(u64::from(timeout));
    if option & MACH_SEND_MSG != 0 {
        if send_size < HEADER_SIZE as u32 {
            return MACH_SEND_MSG_TOO_SMALL;
        }
        if send_size > MAX_MESSAGE_SIZE {
            return MACH_SEND_TOO_LARGE;
        }
        if msg.is_null() || msg.to_bits().checked_add(send_size).is_none() {
            return MACH_SEND_INVALID_DATA;
        }
        let bytes = env.mem.bytes_at(msg.cast().cast_const(), send_size).to_vec();
        let start = Instant::now();
        loop {
            if let Some(result) = try_send(&mut env.libc_state.mach_ports, &bytes) {
                if result != KERN_SUCCESS {
                    return result;
                }
                break;
            }
            if !wait(env, start, (option & MACH_SEND_TIMEOUT != 0).then_some(limit)) {
                return MACH_SEND_TIMED_OUT;
            }
        }
    }
    // A combined operation sends only once, even when receive must wait.
    if option & MACH_RCV_MSG == 0 {
        return KERN_SUCCESS;
    }
    if msg.is_null() || msg.to_bits().checked_add(rcv_size).is_none() {
        return MACH_RCV_INVALID_DATA;
    }
    if !env.libc_state.mach_ports.ports.contains_key(&rcv_name) {
        return MACH_RCV_INVALID_NAME;
    }
    let start = Instant::now();
    loop {
        match try_receive(&mut env.libc_state.mach_ports, rcv_name, rcv_size, option & MACH_RCV_LARGE != 0) {
            Err(error) => return error,
            Ok(Receive::TooLarge(size)) => {
                if option & MACH_RCV_LARGE != 0 && rcv_size >= 8 {
                    env.mem.write(msg.cast::<u32>() + 1, size);
                }
                return MACH_RCV_TOO_LARGE;
            }
            Ok(Receive::Message(bytes)) => {
                let size = bytes.len();
                env.mem.bytes_at_mut(msg.cast(), size as u32).copy_from_slice(&bytes);
                // Default trailer, excluded from msgh_size. try_receive has
                // already checked capacity including alignment and trailer.
                let aligned = (size as u32 + 3) & !3;
                env.mem.bytes_at_mut(msg.cast::<u8>() + size as u32, aligned - size as u32).fill(0);
                let trailer = msg.cast::<u8>() + aligned;
                env.mem.write(trailer.cast::<u32>(), 0u32);
                env.mem.write(trailer.cast::<u32>() + 1, TRAILER_SIZE as u32);
                return KERN_SUCCESS;
            }
            Ok(Receive::Empty) => (),
        }
        if !wait(env, start, (option & MACH_RCV_TIMEOUT != 0).then_some(limit)) {
            return MACH_RCV_TIMED_OUT;
        }
    }
}

/// This function is to `Handle kernel-reported thread exception.`
/// See [exc_server](https://web.mit.edu/darwin/src/modules/xnu/osfmk/man/exc_server.html) for more details.
fn exc_server(
    _env: &mut Environment,
    request_msg: MutVoidPtr, // TODO: use MutPtr<mach_msg_header_t>,
    reply_msg: MutVoidPtr,   // TODO: use MutPtr<mach_msg_header_t>,
) -> boolean_t {
    log_dbg!("TODO: exc_server({:?}, {:?})", request_msg, reply_msg);
    // Note: Because Unity _doesn't_ check the return value of this function
    // with an assert, we can just return a false here.
    // (See [mini-darwin.c](https://github.com/mono/mono/blob/62121afbb28f0b62f100ec9a942d10c5e0f4814f/mono/mini/mini-darwin.c#L142))
    0 // FALSE
}

pub const FUNCTIONS: FunctionExports = &[
    export_c_func!(mach_msg(_, _, _, _, _, _, _)),
    export_c_func!(exc_server(_, _)),
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::libc::mach::mach_port::Port;

    fn state() -> State {
        let mut state = State::default();
        state.ports.insert(0x100, Port { send_refs: 1, ..Port::default() });
        state
    }

    fn message(id: u32) -> Vec<u8> {
        let mut bytes = vec![0; 28];
        set_word(&mut bytes, 0, 19); // COPY_SEND
        set_word(&mut bytes, 2, 0x100);
        set_word(&mut bytes, 5, id);
        set_word(&mut bytes, 6, 0xdeadbeef);
        bytes
    }

    #[test]
    fn inline_fifo_and_header_conversion() {
        let mut state = state();
        for id in 0..2 { assert_eq!(try_send(&mut state, &message(id)), Some(0)); }
        for id in 0..2 {
            let Receive::Message(bytes) = try_receive(&mut state, 0x100, 36, false).unwrap() else { panic!(); };
            assert_eq!(word(&bytes, 0), 17 << 8);
            assert_eq!(word(&bytes, 1), 28);
            assert_eq!(word(&bytes, 2), 0);
            assert_eq!(word(&bytes, 3), 0x100);
            assert_eq!(word(&bytes, 5), id);
            assert_eq!(word(&bytes, 6), 0xdeadbeef);
        }
        assert_eq!(try_receive(&mut state, 0x100, 36, false), Ok(Receive::Empty));
        assert_eq!(state.ports[&0x100].send_refs, 1);
    }

    #[test]
    fn large_receive_preserves_message_otherwise_discards_it() {
        let mut state = state();
        assert_eq!(try_send(&mut state, &message(1)), Some(0));
        assert_eq!(try_receive(&mut state, 0x100, 28, true), Ok(Receive::TooLarge(28)));
        assert_eq!(state.ports[&0x100].messages.len(), 1);
        assert_eq!(try_receive(&mut state, 0x100, 28, false), Ok(Receive::TooLarge(28)));
        assert!(state.ports[&0x100].messages.is_empty());
    }

    #[test]
    fn full_queue_does_not_consume_move_send_right() {
        let mut state = state();
        let mut bytes = message(1);
        for _ in 0..QUEUE_LIMIT { assert_eq!(try_send(&mut state, &bytes), Some(0)); }
        set_word(&mut bytes, 0, 17);
        assert_eq!(try_send(&mut state, &bytes), None);
        assert_eq!(state.ports[&0x100].send_refs, 1);
        try_receive(&mut state, 0x100, 36, false).unwrap();
        assert_eq!(try_send(&mut state, &bytes), Some(0));
        assert_eq!(state.ports[&0x100].send_refs, 0);
        assert_eq!(try_send(&mut state, &bytes), Some(MACH_SEND_INVALID_DEST));
    }

    #[test]
    fn invalid_and_unsupported_messages_do_not_enqueue() {
        let mut state = state();
        let mut bytes = message(1);
        set_word(&mut bytes, 0, 0x80000013); // complex message
        assert_eq!(try_send(&mut state, &bytes), Some(MACH_SEND_INVALID_TYPE));
        set_word(&mut bytes, 0, 19);
        set_word(&mut bytes, 3, 0x100); // unsupported reply right
        assert_eq!(try_send(&mut state, &bytes), Some(MACH_SEND_INVALID_TYPE));
        assert!(state.ports[&0x100].messages.is_empty());
        state.ports.remove(&0x100);
        assert_eq!(try_send(&mut state, &message(1)), Some(MACH_SEND_INVALID_DEST));
        assert_eq!(try_receive(&mut state, 0x100, 36, false), Err(MACH_RCV_PORT_DIED));
    }
}
