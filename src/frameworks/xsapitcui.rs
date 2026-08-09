/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
//! Dependency registration for the private XSAPITCUI framework.

use crate::dyld::HostDylib;

pub const DYLIB: HostDylib = HostDylib {
    path: "/System/Library/Frameworks/XSAPITCUI.framework/XSAPITCUI",
    aliases: &["@rpath/XSAPITCUI.framework/XSAPITCUI"],
    class_exports: &[],
    constant_exports: &[],
    function_exports: &[],
};
