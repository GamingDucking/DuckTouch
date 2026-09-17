/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
//! Process-local image visibility and Mach-O two-level lookup scopes.
use super::*;
use crate::mach_o::{ImportInfo, LibraryOrdinal};

pub(super) fn objc_symbol(symbol: &str) -> Option<(&str, bool)> {
    symbol.strip_prefix("_OBJC_CLASS_$_").map(|s| (s, false))
        .or_else(|| symbol.strip_prefix("_OBJC_METACLASS_$_").map(|s| (s, true)))
}

pub(super) struct LookupScope<'a> {
    pub hosts: Vec<&'static HostDylib>,
    pub guests: Vec<&'a MachO>,
    local_image: Option<&'a MachO>,
}
impl<'a> LookupScope<'a> {
    pub fn search_host<T, F>(&self, exports: F, symbol: &str) -> Option<&'static (&'static str, T)>
    where F: Fn(&HostDylib) -> &'static [&'static [(&'static str, T)]] {
        self.hosts.iter().find_map(|d| search_lists(exports(d), symbol))
    }
    pub fn has_symbol(&self, symbol: &str) -> bool {
        self.guest_address(symbol).is_some()
            || self.search_host(|d| d.function_exports, symbol).is_some()
            || self.search_host(|d| d.constant_exports, symbol).is_some()
            || symbol.strip_prefix("_OBJC_CLASS_$_").or_else(|| symbol.strip_prefix("_OBJC_METACLASS_$_"))
                .map(|name| self.search_host(|d| d.class_exports, name).is_some()).unwrap_or(false)
    }
    pub fn guest_address(&self, symbol: &str) -> Option<u32> {
        self.guests.iter().find_map(|d| {
            let local = self.local_image.map(|image| std::ptr::eq(image, *d)).unwrap_or(false);
            if !local && d.private_symbols.contains(symbol) { return None; }
            d.exported_symbols.get(symbol).copied()
        })
    }
    fn add_image(&mut self, image: &'a MachO, bins: &'a [MachO], loaded: &[&'static HostDylib]) {
        if self.guests.iter().any(|d| std::ptr::eq(*d, image)) { return; }
        self.guests.push(image);
        for path in &image.reexported_libraries { self.add_path(path, bins, loaded); }
    }
    fn add_path(&mut self, path: &str, bins: &'a [MachO], loaded: &[&'static HostDylib]) {
        // A real loaded guest image takes precedence over its host replacement.
        if let Some(image) = bins.iter().find(|b| image_matches(b, path)) {
            self.add_image(image, bins, loaded);
        } else if let Some(&host) = loaded.iter().find(|d| d.path == path || d.aliases.contains(&path)) {
            if !self.hosts.iter().any(|d| d.path == host.path) { self.hosts.push(host); }
        }
    }
}
fn image_matches(image: &MachO, path: &str) -> bool {
    match &image.install_name {
        Some(name) => name == path || host_library(name).zip(host_library(path))
            .map(|(a, b)| a.path == b.path).unwrap_or(false),
        None => image.name == path || path.rsplit('/').next() == Some(image.name.as_str()),
    }
}
fn host_library(path: &str) -> Option<&'static HostDylib> {
    DYLIB_LIST.iter().copied().find(|d| d.path == path || d.aliases.contains(&path))
}

impl Dyld {
    pub(super) fn register_loaded_images(&mut self, bins: &[MachO]) {
        for image in bins {
            for path in &image.dynamic_libraries {
                if bins.iter().any(|guest| image_matches(guest, path)) { continue; }
                if let Some(host) = host_library(path) {
                    if !self.loaded_host_dylibs.iter().any(|d| d.path == host.path) {
                        self.loaded_host_dylibs.push(host);
                    }
                    self.pinned_host_dylibs.insert(host.path);
                }
            }
        }
    }
    pub(super) fn lookup_scope<'a>(&self, importer: &'a MachO, bins: &'a [MachO], import: ImportInfo) -> LookupScope<'a> {
        let mut scope = LookupScope { hosts: Vec::new(), guests: Vec::new(), local_image: None };
        let ordinal = if bins.first().map(|b| b.force_flat_namespace).unwrap_or(false) {
            LibraryOrdinal::Flat
        } else { import.ordinal };
        match ordinal {
            LibraryOrdinal::Flat => {
                scope.hosts = self.loaded_host_dylibs.clone();
                scope.guests.extend(bins);
            }
            LibraryOrdinal::SelfImage => {
                scope.local_image = Some(importer);
                scope.add_image(importer, bins, &self.loaded_host_dylibs);
            }
            LibraryOrdinal::MainExecutable => {
                if let Some(main) = bins.first() { scope.add_image(main, bins, &self.loaded_host_dylibs); }
            }
            LibraryOrdinal::Dependency(n) => {
                if let Some(path) = n.checked_sub(1).and_then(|i| importer.dynamic_libraries.get(i)) {
                    scope.add_path(path, bins, &self.loaded_host_dylibs);
                }
            }
            LibraryOrdinal::Invalid => {},
        }
        scope
    }
    pub(super) fn scoped_proc_address(&mut self, mem: &mut Mem, scope: &LookupScope<'_>, symbol: &str) -> Result<GuestFunction, ()> {
        let entry = scope.search_host(|d| d.function_exports, symbol).ok_or(())?;
        self.proc_for_export(mem, entry)
    }
    pub(super) fn proc_for_export(&mut self, mem: &mut Mem, entry: &'static (&'static str, HostFunction)) -> Result<GuestFunction, ()> {
        // Cache by provider/export identity, not just symbol spelling.
        let key = entry as *const _ as usize;
        if let Some(&function) = self.scoped_host_functions.get(&key) { return Ok(function); }
        let function = self.create_guest_function(mem, entry.0, entry.1);
        self.scoped_host_functions.insert(key, function);
        Ok(function)
    }

    /// Handles are owned allocations, never pointers to caller-owned path strings.
    pub fn open_library(&mut self, path: &str, bins: &[MachO], mem: &mut Mem) -> Option<MutVoidPtr> {
        let canonical = if let Some(image) = bins.iter().find(|b| image_matches(b, path)) {
            image.install_name.as_deref().unwrap_or(&image.name).to_string()
        } else {
            let host = host_library(path)?;
            if !self.loaded_host_dylibs.iter().any(|d| d.path == host.path) { self.loaded_host_dylibs.push(host); }
            *self.dynamic_host_refs.entry(host.path).or_default() += 1;
            host.path.to_string()
        };
        let handle = mem.alloc(1);
        self.library_handles.insert(handle.to_bits(), canonical);
        Some(handle)
    }
    pub fn close_library(&mut self, handle: MutVoidPtr, mem: &mut Mem) -> bool {
        let Some(path) = self.library_handles.remove(&handle.to_bits()) else { return false; };
        if let Some(host) = host_library(&path) {
            if let Some(count) = self.dynamic_host_refs.get_mut(host.path) {
                *count -= 1;
                if *count == 0 && !self.pinned_host_dylibs.contains(host.path) {
                    self.loaded_host_dylibs.retain(|d| d.path != host.path);
                }
            }
        }
        mem.free(handle);
        true
    }
    pub fn dynamic_symbol(env: &mut Environment, handle: MutVoidPtr, symbol: &str) -> Option<ConstVoidPtr> {
        let mut scope = LookupScope { hosts: Vec::new(), guests: Vec::new(), local_image: None };
        if handle.is_null() || handle.to_bits() == (-2i32 as u32) {
            scope.hosts = env.dyld.loaded_host_dylibs.clone();
            scope.guests.extend(&env.bins);
        } else if handle.to_bits() == (-5i32 as u32) {
            if let Some(main) = env.bins.first() { scope.guests.push(main); }
        } else if handle.to_bits() == (-1i32 as u32) || handle.to_bits() == (-3i32 as u32) {
            let caller = env.cpu.regs()[14] & !1;
            let index = env.bins.iter().position(|b| (b.text_base..b.last_segment_end).contains(&caller))?;
            if handle.to_bits() == (-1i32 as u32) {
                scope.guests.extend(&env.bins[index + 1..]);
                scope.hosts = env.dyld.loaded_host_dylibs.clone();
            } else {
                let image = &env.bins[index];
                scope.add_image(image, &env.bins, &env.dyld.loaded_host_dylibs);
                for path in &image.dynamic_libraries { scope.add_path(path, &env.bins, &env.dyld.loaded_host_dylibs); }
            }
        } else {
            let path = env.dyld.library_handles.get(&handle.to_bits())?;
            scope.add_path(path, &env.bins, &env.dyld.loaded_host_dylibs);
        }
        if let Some(addr) = scope.guest_address(symbol) { return Some(Ptr::from_bits(addr)); }
        let function = scope.search_host(|d| d.function_exports, symbol);
        let constant = scope.search_host(|d| d.constant_exports, symbol).map(|e| &e.1);
        let class = objc_symbol(symbol).filter(|(name, _)| scope.search_host(|d| d.class_exports, name).is_some());
        drop(scope);
        if let Some((name, meta)) = class {
            return Some(env.objc.link_class(name, meta, &mut env.mem).cast().cast_const());
        }
        if let Some(entry) = function {
            let address = env.dyld.proc_for_export(&mut env.mem, entry).ok()?;
            env.cpu.invalidate_cache_range(address.to_ptr().to_bits(), 8);
            return Some(address.to_ptr());
        }
        constant.map(|template| Self::materialize_constant(env, template))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const A: HostDylib = HostDylib {
        path: "/A", aliases: &["/alias-A"], class_exports: &[], function_exports: &[],
        constant_exports: &[&[("_shared", HostConstant::NullPtr), ("_only_a", HostConstant::NullPtr)]],
    };
    const B: HostDylib = HostDylib {
        path: "/B", aliases: &[], class_exports: &[], function_exports: &[],
        constant_exports: &[&[("_shared", HostConstant::NSString("B"))]],
    };
    fn image(name: &str, deps: &[&str]) -> MachO {
        MachO { name: name.into(), install_name: Some(name.into()),
            dynamic_libraries: deps.iter().map(|s| s.to_string()).collect(), ..Default::default() }
    }
    fn import(ordinal: LibraryOrdinal) -> ImportInfo { ImportInfo { ordinal, weak: false } }

    #[test]
    fn duplicate_names_are_resolved_by_importing_library_ordinal() {
        let mut dyld = Dyld::new();
        dyld.loaded_host_dylibs = vec![&A, &B];
        let bins = vec![image("main", &["/A", "/B"]), image("other", &["/B", "/A"])];
        let a = dyld.lookup_scope(&bins[0], &bins, import(LibraryOrdinal::Dependency(1)));
        let b = dyld.lookup_scope(&bins[1], &bins, import(LibraryOrdinal::Dependency(1)));
        assert!(matches!(a.search_host(|d| d.constant_exports, "_shared").unwrap().1, HostConstant::NullPtr));
        assert!(matches!(b.search_host(|d| d.constant_exports, "_shared").unwrap().1, HostConstant::NSString("B")));
        assert!(!b.has_symbol("_only_a")); // No fallback into a different library.
    }

    #[test]
    fn flat_lookup_only_sees_loaded_libraries() {
        let mut dyld = Dyld::new();
        let bins = vec![image("main", &["/A"])];
        let scope = dyld.lookup_scope(&bins[0], &bins, ImportInfo::default());
        assert!(!scope.has_symbol("_only_a"));
        dyld.loaded_host_dylibs.push(&A);
        assert!(dyld.lookup_scope(&bins[0], &bins, ImportInfo::default()).has_symbol("_only_a"));
    }

    #[test]
    fn aliases_and_invalid_ordinals() {
        let mut dyld = Dyld::new();
        dyld.loaded_host_dylibs.push(&A);
        let bins = vec![image("main", &["/alias-A"])];
        assert!(dyld.lookup_scope(&bins[0], &bins, import(LibraryOrdinal::Dependency(1))).has_symbol("_only_a"));
        for ordinal in [LibraryOrdinal::Dependency(0), LibraryOrdinal::Dependency(2), LibraryOrdinal::Invalid] {
            assert!(!dyld.lookup_scope(&bins[0], &bins, import(ordinal)).has_symbol("_only_a"));
        }
    }

    #[test]
    fn guest_self_main_and_reexports_with_cycles() {
        let mut main = image("main", &["/facade"]);
        main.exported_symbols.insert("_shared".into(), 0x1000);
        let mut facade = image("/facade", &["/implementation"]);
        facade.reexported_libraries.push("/implementation".into());
        let mut implementation = image("/implementation", &["/facade"]);
        implementation.reexported_libraries.push("/facade".into());
        implementation.exported_symbols.insert("_shared".into(), 0x9000);
        let bins = vec![main, facade, implementation];
        let dyld = Dyld::new();
        let dep = dyld.lookup_scope(&bins[0], &bins, import(LibraryOrdinal::Dependency(1)));
        assert_eq!(dep.guest_address("_shared"), Some(0x9000));
        assert_eq!(dep.guests.len(), 2);
        let self_scope = dyld.lookup_scope(&bins[2], &bins, import(LibraryOrdinal::SelfImage));
        assert_eq!(self_scope.guest_address("_shared"), Some(0x9000));
        let main_scope = dyld.lookup_scope(&bins[2], &bins, import(LibraryOrdinal::MainExecutable));
        assert_eq!(main_scope.guest_address("_shared"), Some(0x1000));
    }

    #[test]
    fn handles_are_owned_and_reference_counted() {
        let mut dyld = Dyld::new();
        let mut mem = Mem::new();
        mem.set_null_segment_size(0x1000);
        let library = DYLIB_LIST[0];
        let path = library.path.to_string();
        let first = dyld.open_library(&path, &[], &mut mem).unwrap();
        let second = dyld.open_library(&path, &[], &mut mem).unwrap();
        drop(path);
        assert_eq!(dyld.library_handles[&first.to_bits()], library.path);
        assert!(dyld.close_library(first, &mut mem));
        assert!(!dyld.close_library(first, &mut mem));
        assert!(dyld.loaded_host_dylibs.iter().any(|d| d.path == library.path));
        assert!(dyld.close_library(second, &mut mem));
        assert!(!dyld.loaded_host_dylibs.iter().any(|d| d.path == library.path));
    }

    #[test]
    fn closing_handle_does_not_unload_link_time_dependency() {
        let mut dyld = Dyld::new();
        let mut mem = Mem::new();
        mem.set_null_segment_size(0x1000);
        let library = DYLIB_LIST[0];
        let bins = vec![image("main", &[library.path])];
        dyld.register_loaded_images(&bins);
        let handle = dyld.open_library(library.path, &bins, &mut mem).unwrap();
        assert!(dyld.close_library(handle, &mut mem));
        assert!(dyld.loaded_host_dylibs.iter().any(|d| d.path == library.path));
    }

    #[test]
    fn private_symbols_do_not_escape_their_image() {
        let main = image("main", &["/library"]);
        let mut library = image("/library", &[]);
        library.exported_symbols.insert("_private".into(), 0x8000);
        library.private_symbols.insert("_private".into());
        let bins = vec![main, library];
        let dyld = Dyld::new();
        assert_eq!(dyld.lookup_scope(&bins[0], &bins, import(LibraryOrdinal::Dependency(1))).guest_address("_private"), None);
        assert_eq!(dyld.lookup_scope(&bins[1], &bins, import(LibraryOrdinal::SelfImage)).guest_address("_private"), Some(0x8000));
    }

    #[test]
    fn main_executable_can_force_flat_lookup() {
        let mut main = image("main", &["/library"]);
        main.force_flat_namespace = true;
        main.exported_symbols.insert("_shared".into(), 0x1000);
        let mut library = image("/library", &[]);
        library.exported_symbols.insert("_shared".into(), 0x8000);
        let bins = vec![main, library];
        let dyld = Dyld::new();
        assert_eq!(dyld.lookup_scope(&bins[0], &bins, import(LibraryOrdinal::Dependency(1))).guest_address("_shared"), Some(0x1000));
    }


    #[test]
    fn relocations_with_the_same_name_keep_distinct_providers() {
        let mut dyld = Dyld::new();
        dyld.loaded_host_dylibs = vec![&A, &B];
        let mut mem = Mem::new();
        mem.set_null_segment_size(0x1000);
        let slots = mem.alloc(8).to_bits();
        let mut main = image("main", &["/A", "/B"]);
        for (address, ordinal) in [(slots, 1), (slots + 4, 2)] {
            mem.write(Ptr::<u32, true>::from_bits(address), 0);
            main.external_relocations.push((address, "_shared".into()));
            main.relocation_imports.insert(address, import(LibraryOrdinal::Dependency(ordinal)));
        }
        let bins = vec![main];
        let mut objc = ObjC::new();
        dyld.do_non_lazy_linking(&bins[0], &bins, &mut mem, &mut objc);
        assert_eq!(dyld.constants_to_link_later.len(), 2);
        assert!(matches!(dyld.constants_to_link_later[0].1, HostConstant::NullPtr));
        assert!(matches!(dyld.constants_to_link_later[1].1, HostConstant::NSString("B")));
    }

    #[test]
    fn function_cache_does_not_merge_different_exports_with_the_same_name() {
        fn first(_env: &mut Environment) -> u32 { 1 }
        fn second(_env: &mut Environment) -> u32 { 2 }
        const FIRST: (&str, HostFunction) = ("_collision", &(first as fn(&mut Environment) -> u32));
        const SECOND: (&str, HostFunction) = ("_collision", &(second as fn(&mut Environment) -> u32));
        let (first, second) = (&FIRST, &SECOND);
        let mut dyld = Dyld::new();
        let mut mem = Mem::new();
        mem.set_null_segment_size(0x1000);
        let a = dyld.proc_for_export(&mut mem, first).unwrap();
        let b = dyld.proc_for_export(&mut mem, second).unwrap();
        assert_ne!(a.addr_with_thumb_bit(), b.addr_with_thumb_bit());
        assert_eq!(dyld.proc_for_export(&mut mem, first).unwrap().addr_with_thumb_bit(), a.addr_with_thumb_bit());
    }

}
