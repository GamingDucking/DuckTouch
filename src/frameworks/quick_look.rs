/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
//! QuickLook.framework dependency registration.
//!
//! Preview UI is not available in the emulator yet. This entry resolves the
//! framework dependency for applications that link QuickLook transitively.

use crate::dyld::HostDylib;

pub const DYLIB: HostDylib = HostDylib {
    path: "/System/Library/Frameworks/QuickLook.framework/QuickLook",
    aliases: &[],
    class_exports: &[],
    constant_exports: &[],
    function_exports: &[],
};
