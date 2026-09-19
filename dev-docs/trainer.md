# Cheat Engine overlay: memory editing and game speed

This is HyperHLE's built-in trainer panel, not a connection to the external
Cheat Engine desktop application. Quick Options calls it `Cheat Engine`, the
floating button reads `CE`, and `--trainer` / `--no-trainer` remain compatible.

## Game speed

The panel has `-`, `SPEED 1x`, and `+` controls. Steps are **0.25x, 0.5x, 1x,
2x and 4x**. Tap the centre button to return to 1x. The selection stays active
when the panel is closed and resets to 1x when a new game environment starts.
Speed is independent of SAFE MODE and does not write guessed memory addresses.

The per-game virtual clock scales `mach_absolute_time`, `clock`,
`gettimeofday`/`time`, Mach clock services, NSDate/CFAbsoluteTime,
CACurrentMediaTime and NSProcessInfo uptime. Changing speed rebases the clock
without a time jump: returning to 1x does not undo elapsed virtual time.
NSTimer (including CADisplayLink) and explicit guest sleeps use virtual
deadlines, so pending waits follow rate changes. The GLES frame-rate cap also
scales, helping fixed-per-frame game loops.

The host FPS counter, bulk-confirmation timeout, input polling and audio
playback remain on real time. This is not audio time stretching or a CPU/JIT
speed multiplier. Network/media clocks, condition-variable timeouts and other
unmodelled timing paths are not universally accelerated. Behaviour depends on
the game, vsync and device performance: 4x is a requested rate, not a guarantee
of four times as many frames. Start with 0.5x or 2x; use 1x if audio/video or
network timing falls out of sync. Speed controls fit in portrait and landscape.

`SET ALL` edits memory matches, **not necessarily currency**. An unrelated
counter, length or game-state flag can contain the same number. Even a single
wrong match can crash a game. The trainer cannot universally identify currency,
validate a game's invariants, or bypass its value checks. Back up saves first.

## Search types

- `AUTO` parses the input separately for each concrete type. Searching for
  `600` no longer searches for the truncated byte value `88` as well.
- Decimal values must fit their integer type. For example, setting `U8` to
  `999999` is rejected rather than silently wrapping around.
- `F32` uses numeric float input (`600` means `600.0`, not integer bits `600`).
  Explicit `0x...` input still denotes raw bits, within the selected width.
- Result rows display their actual type, including results of `AUTO` searches.
- `REFINE` with no remaining results stays empty; only `SEARCH` starts a new
  scan of memory.

## Address-purpose hints and categories

An address such as `0x003C8B28` does not encode the meaning of its contents.
The panel now makes **heuristic guesses**, not guaranteed field identifications:
`Money?`, `Ammo?`, `Health?`, `Score?`, `Timer?`, or `Unknown`.

- Tap **GROUP** to cycle through All and the six categories. The button shows
  the current group's count; HITS shows filtered/total results. Counts and
  paging cover the entire stored search, not just its first 200 entries.
- Every row includes its concrete type and category. Tap a row for the reason
  and low/medium confidence in the status line. There are no invented numerical
  probabilities. A changed value is still highlighted separately.
- DUMP exports the full search (up to its existing dump limit), with concrete
  type, category and reason, regardless of the current view filter.
- `SET ALL` previews **only matching results in the selected group**, including
  pages not currently visible. Switching groups cancels confirmation; a changed
  group/eligible batch needs a fresh preview. SAFE MODE is still independent.
  Aliases can share bytes with other results: a filter is not memory isolation.

The classifier reads at most 64 neighbouring bytes on each side of a value,
inside the same live allocation. It checks complete, case-insensitive English
keywords in ASCII or ASCII-compatible UTF-16LE; clipped words and the searched
value's own bytes are excluded. Conflicting keyword categories stay Unknown.
It does not follow pointers, execute guest code, or probe addresses with writes.
Inspection is incremental (at most 1,024 hits per refresh); large searches take
several refreshes to classify. Unknown also includes not-yet-inspected results.

During live refresh, three observed unit decrements of a small nonnegative
integer can suggest Ammo; three small fractional float decreases can suggest
Timer. Neither pattern proves what a value represents: health, money and other
counters may behave identically. A matching keyword plus pattern is medium
confidence at most. Money/Health/Score currently depend on nearby keywords,
not on a number being large or an address having a particular shape. Trainer
writes reset affected observation histories, and frozen ranges/aliases do not
contribute behavioural evidence. Refinement preserves hints for unchanged types.

Most games will still have many Unknown results: field names may be stripped,
stored elsewhere, encrypted, or absent. Hints can be wrong or become stale;
allocation reuse is not reliably detectable. **A Money? label does not prove
currency or make a write safe.** Verify with legitimate in-game changes and
refinement; back up saves before editing.

## SAFE MODE toggle

`SAFE MODE` is a separate latching switch next to the type selector:

- **Yellow with dark text:** on (the default).
- **Grey with light text:** off.
- Clicking it toggles the state; releasing the button does not reset it.
- The choice survives other actions, closing/reopening the panel and app
  changes within the same emulator session. It is not saved across restarts.
- Toggling cancels a pending bulk confirmation. It does not write memory.

With the mode on, `SET ALL` previews and skips structurally suspicious matches
as described below. With it off, normal bulk editing includes unaligned and
potentially overlapping matches, so the risk of a crash is higher. Normal
mode still requires confirmation and a valid type/value/live address for every
write; invalid or stale entries reject the whole plan rather than being
silently skipped. Already-equal values need no write in either mode.

The switch affects bulk edits, not single `SET`, `FREEZE` or hack files.
Neither setting identifies currency or guarantees a crash-free edit.

## SET ALL: preview, then confirm

There is one bulk-edit button, `SET ALL`. The other bottom-row button is
`DUMP`, which only exports the search results. They have distinct widget IDs.

The old **32-address bulk limit is removed**. All stored search results matching the current group are
considered (the existing search-storage cap of 500,000 still applies), not just
those visible in the panel. For large batches, allocation lookup uses a sorted
index rather than a full allocation scan per hit.

1. Enter a replacement value and tap `SET ALL`. **Nothing is written yet.**
2. The status shows `CHECKED N SKIP M: STILL RISKY`. Here `CHECKED` means
   structurally eligible, **not proven to be currency**. The button becomes
   `CONFIRM`. With the mode off, the status warns `SAFE OFF` instead.
3. Review the counts. Tap `CONFIRM` within 15 seconds to apply that exact plan.
   If the input or eligible writes changed, a new preview is shown and another
   confirmation is required. An expired or missing preview cannot authorize a
   write. Rapid clicks before the confirmation UI appears only create previews.
4. Editing inputs, closing the panel or using another action cancels the
   preview. The button returns to `SET ALL`.

With `SAFE MODE` enabled, the preview skips and counts results that:

- have a different type from the selected explicit type, or cannot represent
  the replacement value without overflow (types are never widened);
- are unaligned, no longer fit inside a live allocation, are unreadable, or
  changed since the last result update;
- already hold the requested replacement value;
- overlap another otherwise eligible result (all members of such a group are
  skipped, including duplicate addresses).

Immediately before applying, the **entire confirmed plan** is checked again
against live allocation boundaries, values and the result list. A failed
preflight writes nothing. Successful writes update the displayed values; the
status reports how many were written and skipped. No eligible results means
no write.

These checks reduce accidental corruption, but do **not** prove that an address
represents currency or that a replacement satisfies the game's rules. Reused
allocations with the same boundaries and value cannot be detected. This is not
a crash-recovery mechanism: even a single eligible but unrelated field can
crash a game or damage a save. Back up saves and refine ambiguous matches.

Choose the known storage type (often `I32`, but it depends on the game).
Observing a legitimate value change is still the most useful way to narrow
matches. For a known address, use single `SET` or a per-game saved hack; these
are not subject to the bulk preview/filter and still require care.

## Regression tests

```sh
RUSTFLAGS="-C link-arg=-latomic" cargo test --lib guest_clock::tests
RUSTFLAGS="-C link-arg=-latomic" cargo test --lib trainer::tests
RUSTFLAGS="-C link-arg=-latomic" cargo test --lib trainer::classify::tests
RUSTFLAGS="-C link-arg=-latomic" cargo test --lib trainer_ui::tests
```

The tests cover numeric bounds, float encoding, truncated AUTO matches, empty
refinement, preview without writing, filtering/counts, more than 32 matches,
confirmation/cancellation/expiry, stale plans without partial writes, unique
widget IDs, independent DUMP/bulk actions, and the latched mode switch. They do
not replace testing against real games on the target device. Classification tests
also cover keyword boundaries/conflicts, read-only allocation-bounded inspection,
behavioural hints, incremental batches, frozen/edit suppression, full-set paging,
category-scoped bulk plans and invalidation after category changes.
