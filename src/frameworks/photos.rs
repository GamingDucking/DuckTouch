/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
//! Photos.framework dependency registration.
//!
//! The framework is currently used as a link-time dependency only. Photo
//! library access is not available in the emulator, so the empty export
//! table intentionally resolves the dylib without claiming that Photos API
//! classes are implemented.

use crate::dyld::HostDylib;

pub const DYLIB: HostDylib = HostDylib {
    path: "/System/Library/Frameworks/Photos.framework/Photos",
    aliases: &[],
    class_exports: &[],
    constant_exports: &[],
    function_exports: &[],
};
