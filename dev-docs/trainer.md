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

## SET ALL checks

The entire batch is checked before the first write. It is rejected if:

- there are more than **32** results (no arbitrary subset is edited);
- the UI type differs from a result's type — search/refine with the intended
  type instead of widening existing hits;
- the new value does not fit any one of the result types;
- a result is unaligned, two results overlap, or an address is no longer fully
  inside a live allocation;
- an address is unreadable or its value changed since the last result update.

A rejection is shown in the panel's status line. No writes are made on a failed
preflight. A successful write updates the values shown in the list immediately.
These checks reduce accidental corruption, but a live allocation and a matching
value do **not** prove that an address represents currency. Allocation reuse
with the same value cannot be detected by these checks either.

If the batch is rejected as too broad, choose the known storage type (often
`I32`, but this depends on the game) and refine the search. Observing a legitimate
value change is still the most useful way to narrow ambiguous matches. For a
known address, use single `SET` or a per-game saved hack; these are not subject
to the bulk count/alignment guard and still require care.

## Regression tests

```sh
RUSTFLAGS="-C link-arg=-latomic" cargo test --lib trainer::tests
```

The tests cover numeric bounds, float encoding, truncated AUTO matches, empty
refinement, bulk preflight failures without partial writes, and successful
mixed-type writes that preserve neighbouring bytes. They do not replace testing
against real games on the target device.
