# Trainer: search and bulk editing

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

The old **32-address bulk limit is removed**. All stored search results are
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
RUSTFLAGS="-C link-arg=-latomic" cargo test --lib trainer::tests
RUSTFLAGS="-C link-arg=-latomic" cargo test --lib trainer_ui::tests
```

The tests cover numeric bounds, float encoding, truncated AUTO matches, empty
refinement, preview without writing, filtering/counts, more than 32 matches,
confirmation/cancellation/expiry, stale plans without partial writes, unique
widget IDs, independent DUMP/bulk actions, and the latched mode switch. They do
not replace testing against real games on the target device.
