# Real Racing 3 1.0.0: Android crash triage (2026-09-20)

## Report identifiers

- App: `com.ea.realracing3.bv`, Real Racing 3 1.0.0, ARMv7 slice.
- Emulator: `5f2ac86-dirty`, build run
  https://github.com/KlugKlugTG/HyperHLE-Fork/actions/runs/35470021088.
- Host: SM-S928B / Adreno 750, Android AArch64.
- Native SIGSEGV fault address: `0xf93dd9bcde00d2`.
- Last guest PC / LR: `0x2c13b4` / `0xba987`.
- Android artifact: `10592882091`, `HyperHLE_Android_AArch64`.

## Confirmed implementation defect matching a guest warning

At 10:34:37.896 the game reports `info was wrong class in ReachabilityCallback`
(BurstlyCore/AS_Reachability.m:87); its receiver is the guest stack address
`0xfffffcec`. The pre-fix implementation of SCNetworkReachabilitySetCallback
stored the address of the caller's SCNetworkReachabilityContext and forwarded
that address as the callback's third argument. The ABI requires **context.info**,
not **&context**. A context can be a stack-local and must be copied at registration.

The fix copies the five-word ARM32 context, checks version/readability, uses
the retain callback's returned info pointer, and releases owned contexts on
replacement, unregistration and target destruction. Old contexts are kept alive
until active/reentrant callbacks return. Guest code is never called while a
host-object borrow is held. A NULL callout unregisters rather than branching
to address zero. Target lifetime is protected around guest callbacks.

The existing synchronous initial reachability notification remains a stub:
this patch does not implement actual network monitoring, run-loop scheduling,
unscheduling, dispatch-queue delivery, or real HTTP requests.

The loader also explicitly reports the missing function
`_UIAccessibilityIsGuidedAccessEnabled`. UIKit now exports a callable query
returning false (no emulated Guided Access session). This removes that unresolved
import; it does not establish that the observed jumps to `0x1000` came from it.

## Native fault is not yet symbolicated or reproduced

The provided trace does not establish that either defect caused the final native
SIGSEGV. It contains no trainer search/write events. Do not blame WATCH or the
previous NOVA 3 ammo edit, and do not claim RR3 now boots successfully.

Other warnings include null guest-object accesses, fake returns after undefined
instructions at `0x1000`, stubbed networking and a later unimplemented GL API.
These may indicate additional independent compatibility defects; log proximity
alone does not identify the native crash site.

Artifact metadata was accessible, but `gh run download` failed with an EOF from
the signed Azure blob URL. No matching ELF/debug symbols were obtained. The
following are **file offsets**, not yet verified ELF virtual addresses:

`file_offset = PC - 0x7a304da000 + 0x689000`

| Frame | Runtime address | File offset |
|---|---|---|
| #5 | `0x7a30bb5dc0` | `0xd64dc0` |
| #6 | `0x7a30ba7fac` | `0xd56fac` |
| #7 | `0x7a306ee4f4` | `0x89d4f4` |
| #8 | `0x7a305b1b90` | `0x760b90` |
| #9 | `0x7a306c10f0` | `0x8700f0` |
| #10 | `0x7a3095c4cc` | `0xb0b4cc` |
| #11 | `0x7a309c5244` | `0xb74244` |
| #12 | `0x7a306b666c` | `0x86566c` |
| #13 | `0x7a306f33c8` | `0x8a23c8` |
| #14 | `0x7a307e2db0` | `0x991db0` |

To symbolize, obtain libtouchHLE.so from this exact build (and matching debug
symbols if stripped), inspect its PT_LOAD segments, convert file offsets to
ELF virtual addresses, then use llvm-addr2line. Raw ASLR addresses or another
build's ELF cannot reliably identify functions. Initial frames include crash
reporting/signal trampolines and must not be mistaken for the original fault.

## Validation and retest

Rust regressions cover the 20-byte guest layout, copying caller-owned storage,
version/bounds rejection, retained-info replacement, NULL callback guards,
context retirement during nested callbacks, and UIKit export registration.
They have not been executed here: Cargo/rustc are unavailable. Syntax/static
checks are not a build or a device test.

On device, run RR3 from a fresh launch without memory edits. Check whether the
wrong-class ReachabilityCallback warning and unresolved Guided Access import
are gone. If SIGSEGV remains, collect the new complete log and exact build/APK;
a changed stack needs that new build's symbols.

## Retest on 30bdf19: native crash persists

The user retested `30bdf19-dirty` from run
https://github.com/KlugKlugTG/HyperHLE-Fork/actions/runs/35497728843
(commit `30bdf19655513ac186e8d6edf45428a9551c4b57`, Android artifact
`10601462660`). Guided Access resolves, and the supplied new log no longer
contains the wrong-class ReachabilityCallback warning. The native SIGSEGV
still has fault address `0xf93dd9bcde00d2`, guest PC `0x2c13b4`, LR `0xba987`.
The compatibility patch is **not** a confirmed fix for this native crash.

The new libtouchHLE mapping is `799d113000-799dde0000 r-xp`, file offset
`00689000`. The crash log's libtouchHLE frame PCs (raw addresses `0x799d3ba2ec`,
`0x799db11fe0`, then `0x799d7ef168`, `0x799d7e1354`, `0x799d327af8`, … up to
`0x799d113ffc`) belong to this build only; do not mix them with the older
table above. Frames #5/#6/#7 have file offsets `0xd65168`, `0xd57354`,
`0x89daf8` (PC − mapping start + mapping offset).

The `statvfs` TODO message does not mean it leaves the output untouched:
`src/libc/posix_io/statvfs.rs` writes its guest structure and returns success.
The log continues through null-object warnings, networking stubs and rendered
frames after this call. Neither the statvfs message nor the last unimplemented
GLES message establishes the cause of the native crash. No speculative runtime
change was made in response to these messages.

Both `gh run download` and a separate Python HTTPS request to the new artifact's
storage URL failed with TLS/EOF in the agent environment. The chat UI only
accepts images, so asking for an APK attachment is not a usable next step.

### How the stack was obtained (temporary diagnostics, since removed)

Because the chat UI only accepts images and direct APK downloads kept failing
with Azure TLS/EOF, a temporary `diagnose_rr3_crash` mode was added to the
**Build HyperHLE** workflow: a runner downloaded the saved run 35497728843 APK,
converted the recorded PCs through the ELF PT_LOAD segments and resolved them
with AArch64 binutils (symbolication script + tests lived in `dev-scripts/`,
trace fixture in `dev-docs/crashes/real-racing-3-30bdf19.json`). It ran
successfully on 2026-09-20 as
https://github.com/KlugKlugTG/HyperHLE-Fork/actions/runs/35499114018; the
resulting stack is preserved below. The diagnostics mode, script and fixture
were removed afterwards at the user's request — the results remain in this
document, and the raw annotations stay readable through the Checks API for
run 35499114018 (check-run 106047557050).

## Symbolicated stack: the crash is inside mimalloc v3.3.2

The diagnostics mode was run for real on 2026-09-20: run
https://github.com/KlugKlugTG/HyperHLE-Fork/actions/runs/35499114018
(job `diagnose-rr3`, check-run annotations) downloaded the saved
run 35497728843 APK on a GitHub runner and resolved every recorded
libtouchHLE frame — the APK kept its `.symtab`, so no symbol was guessed.

Innermost-first native stack (annotations, abbreviated):

```
#5  _mi_malloc_generic                        (mimalloc, static.c)
#6  _mi_theap_realloc_zero                    (mimalloc)
#7  <alloc::raw_vec::RawVecInner>::finish_grow
#8  std::io::default_read_to_end::<zip::read::ZipFile>
#9  <touchHLE::fs::bundle::IpaFileRef>::open
#10 touchHLE::libc::posix_io::stat::stat
#11 <CallFromGuest>::call_from_guest
#12 touchHLE::environment::Environment::run_inner
#2  Dynarmic::...::SigHandler::SigAction      (forwards non-guest faults)
#1  touchHLE::crash_handler::imp::handler
```

Fault address `0xf93dd9bcde00d2` is a garbage pointer (not a guard page, not a
near-null jump). The `stat`/`IpaFileRef::open` frames are plain safe Rust on
this path — they are where the corrupt allocator state was *hit*, not
necessarily where it was *caused*.

### Root-cause identification

- The `_mi_theap_*` symbol names prove the build used mimalloc **v3**
  (v2 uses `mi_heap_*` names). libmimalloc-sys 0.1.49 vendors v3 by default,
  pinned at microsoft/mimalloc `30b2d9d8` = **v3.3.2** (2026-04-29).
- Upstream microsoft/mimalloc issue **#1287**: "mimalloc >= 3.3.0 causes
  segmentation faults when used from multiple threads" — 3.3.0/3.3.1/3.3.2
  crash, 3.2.x fine; stack is `_mi_malloc_generic` → `mi_thread_init` →
  dereferencing NULL `theap->tld`; the maintainer identified the cause as the
  main thread being misidentified and reclaimed, after which allocator state
  is corrupt; the issue includes a pure-Rust reproducer (no touchHLE
  involved). Status: fixed later in the dev line.
- Issue **#1288**: the same crash on **Android** (v3.3.1), same
  `_mi_malloc_generic` entry path, maintainer-confirmed.
- The 3.4/3.5 line (through v3.5.3, 2026-09-17) carries a chain of
  theap/thread-reclamation/NULL-theap fixes — this class was still being
  repaired after our pinned revision.

RR3 spawns many short-lived native threads (unlike the tested NOVA3), which
matches the #1287 trigger. `finish_grow` in frame #7 is simply the Vec growth
that performed the realloc at the moment the thread-local heap was broken.

### Fix: pin the global allocator to the mimalloc v2 branch

`Cargo.toml` now builds mimalloc with `features = ["v2"]`, so
libmimalloc-sys 0.1.49 compiles the maintained **v2.3.2** branch — the
default of every mimalloc release before the 3.x flip and the variant
long used by Android builds. No Rust/C source changes; `Cargo.lock` is
unchanged (Cargo does not lock features). Local validation: manifest TOML
parses; cargo is unavailable in this environment, so the next ordinary
**Build HyperHLE** run is also the compile check.

Retest plan: run Build HyperHLE normally (no diagnostics input needed),
install the produced APK, start RR3. If it still SIGSEGVs inside mimalloc,
the next candidates are bumping to the v3.5.x line or removing mimalloc;
decide on fresh evidence, not assumption.
