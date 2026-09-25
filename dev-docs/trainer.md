# Cheat Engine overlay: memory editing and game speed

This is HyperHLE's built-in trainer panel, not a connection to the external
Cheat Engine desktop application. Quick Options calls it `Cheat Engine`, the
floating button reads `CE`, and `--trainer` / `--no-trainer` remain compatible.
The Quick Options switch starts off unless `--trainer` was supplied on the
command line, and always passes an explicit launch flag for its selected state,
overriding app options.

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
It does not follow arbitrary pointer chains, execute guest code, or probe
addresses with writes.
Inspection is incremental (at most 1,024 hits per refresh); large searches take
several refreshes to classify. Unknown also includes not-yet-inspected results.

### Deeper analysis: typed fields and multi-event patterns

For registered Objective-C objects the trainer now checks runtime field
metadata: the allocation base must be a known object, the exact field offset
must match, and the declared scalar type and size must match the search result.
Inherited fields are checked too (bounded to 16 classes / 256 fields per lookup).
Pointers, object references, aggregates and unknown encodings are excluded.
Names such as `_coins`, `playerAmmo`, `currentHP` and `healthPoints` can therefore
identify a field even when no readable string is next to its numeric value.
This stronger hint takes precedence over unrelated nearby words. Tapping a
result shows the actual field name when available. No guest messages are sent.

Plain repeated `-1` changes are now **ambiguous**, not automatically Ammo.
An Ammo pattern requires at least two observed cycles of three or more unit
decrements followed by a refill to the same small integer capacity. A Timer
pattern needs six fractional float decreases at a roughly consistent rate,
using elapsed host time and accounting for unchanged samples between changes.
Irregular jumps reset the pattern. Matching nearby words can corroborate the
patterns, but patterns alone are still low-confidence hints, not identification.
Trainer edits reset histories and frozen ranges/aliases do not provide evidence.

### MARK: narrow addresses using an actual game action

This often works better than guessing labels, particularly in C/C++ games:

1. Search for the current displayed number; use **GROUP: All** initially.
2. Tap **MARK** to snapshot the complete stored result set.
3. Return to the game (or open WATCH) and spend currency, fire, or take damage.
4. Open CE and tap **DOWN**, **UP**, **CHANGED**, or **SAME** to retain only
   results whose values decreased, increased, changed, or stayed the same.
5. Repeat with another action. Each successful comparison advances the baseline
   to the surviving results' current values; ordinary live refresh never moves it.

The comparison filters the search, not just the visible page/category, and
writes nothing. Numeric comparisons honour signed integers and float values.
No MARK means no filtering; an exhausted comparison never restarts a search.
New searches, value refinements, reset, trainer edits/hack reloads and bulk
preview invalidate the experiment (MARK again). Freed/unreadable addresses
are rejected; allocation reuse with identical boundaries remains undetectable.

### WATCH: independent live-change window

Tap **WATCH** in CE to toggle a separate window beside the main editor. Both
windows fit side by side; closing the main editor leaves WATCH over the game
with larger controls. It monitors **all stored search hits**, independent of GROUP, not arbitrary
unsearched memory. Each row shows `address`, concrete `type`, `before -> after`,
and seconds since observation. New changes light up; old rows retain their age
instead of pretending to be happening now. Touches outside both windows still
reach the game.

**Edit directly in WATCH:** tap a changed row. Its address/type is pinned below
the feed, with a live **NOW** value, independent **NEW** input, numeric keypad
(`CLR` clears, `DEL` deletes a character), and **SET**. Enter a number and tap
SET: only that concrete address/type is targeted, never the main editor's
selection or all feed rows. The feed keeps updating while typing; new events
cannot redirect the pinned target or replace your input. The row under a held
touch is kept stable until release. **DONE** collapses the inline editor.

Before writing, the engine rechecks membership in the current search, concrete
type, numeric range, readable live allocation and overlap with frozen patches.
Frozen addresses require UNFRZ in the main editor first. A normal in-game value
change does not block an explicit SET. The result/error appears in WATCH, and
NOW is reread immediately. New searches/refinements/comparisons/reset clear the
pinned edit. A successful edit invalidates MARK and resets affected observation
histories. SET is still a manual memory edit, not a proof the address is safe:
aliases share bytes, reused allocations cannot always be detected, and the game
may overwrite the value on its next update.

- **PAUSE / RESUME** holds/releases the displayed feed, not the game. The
  pinned NOW value keeps refreshing even while the history feed is paused.
- **OLDER / NEWER** browses recent entries and automatically holds the feed.
- **CLEAR** clears history; **X** hides the window. CE still opens the editor.
- The feed retains 64 distinct address/type pairs, coalescing repeated changes.
  It shows five per page, counts all changed hits per sample, and processes at
  most 256 feed events per sample with a rotating traversal when overloaded.
  The header says `(sampled)` when the feed cannot retain every changed hit.
- Sampling remains every ~250 ms of host time. Intermediate writes or changes
  that return to their previous value between samples may be missed. This is
  not a CPU write breakpoint or a complete memory-access trace.
- Trainer writes, frozen aliases, and expired allocations are not reported as
  game-change events. Search/refine/reset and comparisons clear the old feed.

Most games will still have many Unknown results: field names may be stripped,
stored elsewhere, encrypted, or absent. C/C++ objects have no Objective-C
field metadata, and host-only values are not guest-memory scalar fields. Hints can be wrong or become stale;
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
RUSTFLAGS="-C link-arg=-latomic" cargo test --lib trainer::watch::tests
RUSTFLAGS="-C link-arg=-latomic" cargo test --lib objc::properties::trainer_metadata_tests
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

Additional regressions cover exact scalar field identification, pointer/type/
offset rejection, repeated refill cycles, irregular timer rejection, explicit
snapshot baselines, signed/float comparisons, bounded/coalesced activity feeds,
trainer-write suppression, side-by-side window layout, stable inline edit
targets, read-only observation controls, and validated single-address writes.

## Overlay touches no longer break game input

Two fixes target games where swipes or taps randomly stopped registering
(e.g. Subway Surfers "swipe right does nothing sometimes"):

1. **Stray releases are no longer swallowed.** If a finger was pressed in the
   game and only the release landed over the overlay, the release used to be
   eaten by the overlay. The game then believed the finger was still down, and
   its next gestures had no `touchesBegan:` and were ignored. The overlay now
   consumes an Up only when it consumed the matching Down, and a press that
   lands on the panel but no widget swallows the whole gesture (no ghost
   `Moved`/`Ended` without `Began` reaches the game).
2. **Stale touches are healed.** If an Up/Cancel is lost for any other reason
   (backgrounding mid-swipe, host quirk), a repeated Down for the same finger
   used to be converted into a Move — the game again never saw `touchesBegan:`.
   It now receives `touchesCancelled:` for the stale touch and a fresh
   `touchesBegan:` for the new one, like real iOS. Each heal is logged at
   default level as `touch ... [heal N]`, so a misbehaving game can be
   diagnosed from an ordinary log without debug flags.
3. **The first finger survives a second finger.** On views that refuse
   multi-touch, a second simultaneous finger used to silently DELETE the
   tracked first touch (no Ended, no further events), stalling Unity-style
   input state machines. Now the tracked touch is kept and only the newcomer
   is dropped, like real iOS; genuinely stale (10s+ idle) touches are
   cancelled visibly and reclaimed.
4. **Gesture summaries in ordinary logs.** The first touch ends are logged as
   `TOUCH-END #n: view=... moves=... delta=(...)`, and Move/Up events for
   fingers whose Down never arrived are warned as `untracked finger`. These
   lines separate "host lost events" from "game ignored a complete gesture".

## In-app purchase emulation (IAP)

The Quick Options panel has a latched **IN-APP: FREE BUYS** button, a
Lucky Patcher-style switch for StoreKit in-app purchases. It can also be
forced on for a session with the `TOUCHHLE_IAP_EMULATION` environment
variable. It is off by default and resets with the process.

While enabled:

- `+[SKPaymentQueue canMakePayments]` returns true, so purchase buttons appear.
- `SKProductsRequest` answers locally: every requested identifier becomes a
  product with price 0.00 (title/description copied from the identifier).
- `-[SKPaymentQueue addPayment:]` completes immediately as
  `SKPaymentTransactionStatePurchased` with a fresh
  `touchHLE.iap.N` transaction identifier, and the observer receives
  `paymentQueue:updatedTransactions:` normally.
- Purchased/restored transactions answer `[SKPaymentTransaction receipt]`
  with a stable opaque NSData blob (`touchHLE-IAP-receipt/v1:<identifier>`)
  and `transactionDate` with the current time. Games of this era commonly
  gate crediting on a non-nil/non-empty receipt; without this they silently
  grant nothing (observed as "0 currency").
- `SKProduct` answers `isDownloadable` and `contentDownloadable` (false), and
  the products delegate also receives `requestDidFinish:` after the response.
- `restoreCompletedTransactions` re-delivers every identifier bought this
  session as restored transactions.
- With the switch off (the default), StoreKit falls back to the original
  pre-emulation stubs: `canMakePayments` is false, `addPayment:` notifies the
  observer with an empty transaction array, and `SKProductsRequest` fails in
  `initWithProductIdentifiers:`/`start`, so no product list, transaction or
  receipt is ever fabricated. Games see exactly the stock no-store behavior
  they had before the IAP emulation existed.

Scope and honesty: this only affects the guest app inside touchHLE. No App
Store, receipts or Apple servers are contacted, nothing outside the emulator
changes, and server-verified purchases in online games will still fail on the
game's own backend checks.
