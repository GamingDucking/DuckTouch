/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
//! Stack Smashing Protection (SSP)

use crate::dyld::{export_c_func, ConstantExports, FunctionExports, HostConstant};
use crate::environment::Environment;

// Stack-protector failure is `noreturn` in the guest ABI. Returning from this
// host stub normally would execute unreachable guest code, so use the same
// validated recovery / controlled-session-end path as abort and assertions.
pub fn __stack_chk_fail(env: &mut Environment) {
    log!(
        "*** __stack_chk_fail: stack smashing detected in guest! The guest's stack canary was \
         corrupted. Attempting validated recovery without terminating the emulator process."
    );
    crate::libc::stdlib::recover_or_end_guest_termination(env, "__stack_chk_fail()");
}

pub const FUNCTIONS: FunctionExports = &[
    // Экспортируем функцию. Макрос автоматически добавит нужное подчеркивание
    // для C.
    export_c_func!(__stack_chk_fail()),
];

pub const CONSTANTS: ConstantExports = &[
    // Используем гарантированно существующий вариант.
    // Игра получит валидный указатель на 0x00000000 и использует его как
    // канарейку.
    ("___stack_chk_guard", HostConstant::NullPtr),
];
