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
