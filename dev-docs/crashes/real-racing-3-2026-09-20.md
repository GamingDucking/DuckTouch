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
`00689000`. All supplied libtouchHLE frame addresses for this report are saved
in `real-racing-3-30bdf19.json`; do not mix them with the older table above.
Frames #5/#6/#7 have file offsets `0xd65168`, `0xd57354`, `0x89daf8`.
These still require PT_LOAD conversion before using addr2line.

The `statvfs` TODO message does not mean it leaves the output untouched:
`src/libc/posix_io/statvfs.rs` writes its guest structure and returns success.
The log continues through null-object warnings, networking stubs and rendered
frames after this call. Neither the statvfs message nor the last unimplemented
GLES message establishes the cause of the native crash. No speculative runtime
change was made in response to these messages.

Both `gh run download` and a separate Python HTTPS request to the new artifact's
storage URL failed with TLS/EOF in the agent environment. The chat UI only
accepts images, so asking for an APK attachment is not a usable next step.

### Obtain text without transferring the APK through chat

The existing **Build HyperHLE** workflow now has a `diagnose_rr3_crash` boolean
input. Select branch `arena/01a0bad7-hyperhle-fork`, enable this input, and run it.
This mode skips all four builds and release preparation/publishing. Its own
concurrency group avoids cancelling an ordinary build. No additional workflow
was introduced, and the emulator binary is not modified by this change.

The `diagnose-rr3` job downloads the **saved original** run's APK on the GitHub
runner, verifies the source run ID and commit against the recorded trace, reads
ELF PT_LOAD segments, and runs AArch64 addr2line. It also emits nearby machine
instructions for frames #5/#6 and APK/ELF hashes plus ELF build ID notes. If the
APK has been stripped, the report says so; unknown functions are not guessed.
A failed source download fails the job rather than substituting a new build.

For the investigation branch only, this job also runs alongside ordinary manual
builds. This provides a fallback if GitHub's UI displays an older input form:
just select the branch and run Build HyperHLE normally. The normal builds are
not skipped in that case. The branch-specific fallback can be removed once
this incident is resolved.

Output is available in the job summary, the `RR3-native-crash-report` text
artifact, and workflow annotations. The annotations can be read through the
GitHub REST API without downloading another Azure blob:

```sh
# Use the NEW diagnostics run ID here, not the old source build ID.
gh api repos/KlugKlugTG/HyperHLE-Fork/actions/runs/RUN_ID/jobs \
  --jq '.jobs[] | select(.name == "diagnose-rr3") | .check_run_url'
# The last component of check_run_url is CHECK_RUN_ID.
gh api --paginate repos/KlugKlugTG/HyperHLE-Fork/check-runs/CHECK_RUN_ID/annotations \
  --jq '.[] | .message'
```

The agent's prior workflow dispatch attempt was denied (403); this change does
not grant dispatch permission. The user must start the workflow. The exact APK
has not yet been symbolicated and no RR3 native-crash fix is claimed.

Local validation: 13 Python tests pass, including PT_LOAD translation with a
real compiled ELF whose virtual addresses differ from file offsets, rejection
of malformed/wrong-run inputs, stripped-symbol reporting, annotation escaping,
and the recorded RR3 offset arithmetic. This is not an Android runtime test.
