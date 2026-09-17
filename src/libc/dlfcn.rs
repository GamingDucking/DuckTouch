/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! `dlfcn.h` (`dlopen()` and friends)
//! Реализация подсистемы динамического связывания POSIX для HLE-эмуляции.
//! Код спроектирован с учетом устойчивости к некорректному доступу к памяти со
//! стороны гостевого приложения.

use crate::dyld::{export_c_func, FunctionExports};
use crate::mem::{ConstPtr, MutVoidPtr, Ptr};
use crate::Environment;

/// Псевдо-дескриптор для доступа к глобальной области видимости символов (main
//executable).
/// В операционных системах семейства Darwin/iOS RTLD_DEFAULT традиционно равен
//(void*)-2.
const RTLD_DEFAULT: MutVoidPtr = Ptr::from_bits(-2 as _);
const RTLD_NEXT: MutVoidPtr = Ptr::from_bits(-1 as _);
const RTLD_SELF: MutVoidPtr = Ptr::from_bits(-3 as _);
const RTLD_MAIN_ONLY: MutVoidPtr = Ptr::from_bits(-5 as _);

/// Open only implemented host libraries or guest images already mapped by the
/// loader. A handle is owned by dyld, not borrowed from the caller's path buffer.
fn dlopen(env: &mut Environment, path: ConstPtr<u8>, _mode: i32) -> MutVoidPtr {
    if path.is_null() { return RTLD_DEFAULT; }
    let Ok(path) = env.mem.cstr_at_utf8(path) else { return Ptr::null(); };
    let path = path.to_string();
    env.dyld.open_library(&path, &env.bins, &mut env.mem).unwrap_or_else(Ptr::null)
}

fn dlsym(env: &mut Environment, handle: MutVoidPtr, symbol: ConstPtr<u8>) -> MutVoidPtr {
    if symbol.is_null() { return Ptr::null(); }
    let Ok(name) = env.mem.cstr_at_utf8(symbol) else { return Ptr::null(); };
    let name = format!("_{}", name);
    crate::dyld::Dyld::dynamic_symbol(env, handle, &name)
        .map(|p| p.cast_mut()).unwrap_or_else(Ptr::null)
}

fn dlclose(env: &mut Environment, handle: MutVoidPtr) -> i32 {
    if handle == RTLD_DEFAULT || handle == RTLD_NEXT || handle == RTLD_SELF || handle == RTLD_MAIN_ONLY {
        return 0;
    }
    if env.dyld.close_library(handle, &mut env.mem) { 0 } else { -1 }
}

/// Реализация функции `dlerror` стандарта POSIX (man 3 dlerror на Darwin).
///
/// Apple: "If no errors have occurred since initialization or since
/// `dlerror()` was last called, `dlerror()` returns NULL." Because our
/// `dlopen` / `dlsym` / `dlclose` never publish a per-thread error message
/// (they log internally and return NULL/-1 to the guest), the correct
/// POSIX-conforming reply is always `NULL`. Without this entry point the
/// dynamic linker installed a generic return-0 stub, which a few guests
/// (notably the iPhone OS port of `libstdc++`'s exception handling) treat
/// as "no error" but others (Bionic-style helpers) tried to call
/// `strlen()` on. Returning a real `NULL` pointer is unambiguous.
fn dlerror(_env: &mut Environment) -> ConstPtr<u8> {
    Ptr::null()
}

// Экспорт C-функций в глобальное адресное пространство гостевого процесса.
pub const FUNCTIONS: FunctionExports = &[
    export_c_func!(dlopen(_, _)),
    export_c_func!(dlsym(_, _)),
    export_c_func!(dlclose(_)),
    export_c_func!(dlerror()),
];
