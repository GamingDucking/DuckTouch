# Scoped dyld symbol resolution

Guest symbol bindings no longer use the global host implementation catalogue.
The catalogue is still needed for internal HLE runtime construction and symbol
inventory dumps; it is intentionally separate from guest image visibility.

Implemented:

* Per-import nlist library ordinals, including self, main executable and dynamic
  lookup; flat images and MH_FORCE_FLAT retain flat lookup over loaded images.
* LC_DYLD_INFO pointer binds and eager resolution of its lazy pointer binds,
  preserving ordinals, weak-import flags and regular bind addends.
* Dependency ordering includes weak, upward, lazy-load and reexport commands.
* Guest install names and recursive LC_REEXPORT_DYLIB traversal with cycle guards.
* Imported constants/functions/classes use the selected scope. Non-exported guest
  symbols are not visible outside their defining image.
* Missing weak imports are null rather than resolved from an unrelated image.
* Function addresses are cached by export identity; constant addresses are shared
  by late relocation binding and dlsym.
* dlopen handles are owned by dyld and reference-count host library visibility.
  Closing the last explicit handle does not hide a link-time dependency.
* dlsym searches the named loaded library, or loaded images for RTLD_DEFAULT;
  RTLD_MAIN_ONLY is restricted to the executable, and SELF/NEXT use the guest LR
  to identify the calling image.

This is not a complete replacement for Apple's dyld. Loading previously unmapped
**guest** dylibs via dlopen, dlopen mode flags (LOCAL/GLOBAL/FIRST/NOLOAD/NODELETE),
weak-definition coalescing, export-trie symbol reexports, full @rpath resolution
and host framework dependency/reexport metadata remain outside this change.
Existing explicit HLE runtime ABI overrides (Swift/C++ and compatibility shims)
remain; they are not ordinary symbol-table exports. No global catalogue fallback
is used for ordinary guest imports or dlsym.

Regression checks (with the project toolchain and native dependencies installed):

```
cargo test --lib dyld::symbol_lookup::tests
cargo test --lib nlist_library_ordinals_and_weak_imports
cargo test --lib bind_special_ordinals_are_signed
```

The tests cover duplicate names with different ordinals (including actual
non-lazy relocation dispatch), missing libraries, aliases, self/main scopes,
private symbols, forced-flat lookup, reexport cycles, handle ownership/lifetime,
pinned dependencies and function-cache isolation. Guest application testing is
still necessary, especially for SDKs whose framework export ownership differs
from the host catalogue.
