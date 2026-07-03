# ADR-0043: bindings `OutputMode` — commit the extension mechanism, `Tree`-only for now

- **Status:** Accepted — ratified by the architect's merge of PR #611 (2026-07-03)
- **Date:** 2026-07-03

Extends **ADR-0029** (public `OutputBuilder` API shape), which fixed the *Rust*
output taxonomy and explicitly deferred the *binding-exposure* half to #244. This
record resolves that deferred half.

## Context

An open Rust trait (`OutputBuilder`, exposed to Rust callers by `parse_into`, C7 /
#232) **cannot cross the PyO3 / WASM / C boundary** — a foreign caller cannot
implement a Rust trait. So the bindings must select from *built-in* backends through
some closed selector, and the shape of that selector is a product/ABI commitment
distinct from the Rust surface.

Two facts frame the decision:

- **Today all three bindings already expose exactly one backend — `Tree`.** The C
  API walks a tree via `lark_tree_*` (#48); PyO3 returns `Tree` / `Token` (`Token`
  IS-A `str`, ADR-0036); WASM returns the tree as a JS object in the oracle-JSON
  shape. So #244 is not "what do we expose first," it is "what is the *extension
  mechanism* when we add a second mode, and do we add one now."
- **No binding consumer is pulling for a fast mode.** `SpanTree` shipped
  (`span-tree` feature) but its value is borrowed `&'i str` spans, which cannot be
  handed safely across FFI; the `TapeTree` backend was built + measured (#243) and
  closed with **no named consumer**; the event backend is internal with the
  embedded transformer as its only consumer (ADR-0039). Per ADR-0026, binding
  surface with no consumer and no validation story is not shipped.

There is no Python oracle for binding ergonomics — this is escalate-tier
(PRINCIPLES §6), decided by the architect, not grounded by a test.

## Decision

**Commit the extension mechanism, not a menu of modes.**

1. **Modes exposed now: `Tree` only.** It is the sole parity-tested mode, the
   committed-stable Rust surface (ADR-0029), and already implemented in every
   binding.

2. **The "closed enum" is load-bearing only for the C ABI.** Shape the selector
   per binding, idiomatically:
   - **C ABI:** an **additive-only** integer enum — `LARK_OUTPUT_TREE = 0`, existing
     values never renumbered, unknown values rejected with a clear error.
   - **PyO3 / WASM:** a **string option** (`output="tree"` / `{output: "tree"}`),
     unknown → clear error. Strings are inherently additive, so there is no
     enum-stability question to answer for the two dynamic bindings, and a string is
     the idiomatic surface in both languages.
   - Any **Rust-facing** selector the bindings share internally is
     `#[non_exhaustive]`.

3. **No callback / transformer escape hatch in the closed enum.** A per-node
   callback across PyO3 / WASM / C is expensive and awkward (FFI overhead per call)
   and has no oracle. The closed selector stays confined to *representation* modes.
   A **PyO3-native `Transformer`** — idiomatic Lark, and the one binding-callback
   idea that *does* have a Python oracle (the embedded-transformer event stream,
   C8b / #242 / ADR-0039) — is a separate, Python-specific future question riding the
   event backend, **not** a cross-binding `OutputMode` variant.

4. **`SpanTree` / `Tape` binding exposure is deferred to a consumer-driven
   trigger.** When a real binding consumer needs a zero-materialization mode, it is
   exposed then — as **language-neutral owned values / byte offsets**, never a
   borrowed Rust `&'i str` (a C consumer can take offsets into its own input buffer;
   PyO3 / WASM copy). That consumer is simultaneously the **named consumer that C8b
   (#242) and C8c (#243) are gated on**, so it is pursued as its own scoped work, not
   folded into this taxonomy decision.

Pre-users (ADR-0025), breaking the public API is still free, so these conventions are
not defensive necessities — they are adopted because the additive C enum and the
string kwarg are zero-cost now and remove a guaranteed future break.

## Consequences

- **Closes #225's last standing decision.** The epic's core (Rust semantic output)
  already shipped; this lets #225 close cleanly with the binding half settled in
  principle.
- **Constraints:** the C ABI is committed to additive-only enum numbering; PyO3 /
  WASM are committed to a string `output` option. Both cheap, both idiomatic.
- **Creates a named trigger.** "A binding fast-output consumer" is now the explicit
  condition that would (a) justify a second `OutputMode` variant and (b) satisfy the
  named-consumer gate on the internal event / tape backends (C8b / C8c) — the natural
  next-epic seed after the perf + refactor work.
- **Enforcement:** taxonomy/policy, so the gate is review, not a test — the first
  binding-mode addition must follow the additive C-enum / string-kwarg / offsets-not-
  borrows conventions above, checkable in `/review-pr`.

## Alternatives considered

- **Expose `Tree` + `SpanTree` now.** Rejected: no consumer pulls for it, and the
  borrowed-span → owned re-projection is unvalidated beyond-oracle surface (ADR-0026).
- **A full numeric closed enum across all three bindings** (`Tree` / `SpanTree` /
  `Tape` / `Event`). Rejected: over-commits the ABI to internal/experimental modes and
  forces the enum-stability problem onto PyO3 / WASM, where a string sidesteps it.
- **Defer #244 wholesale.** Tenable (prio:later, no blocker), but it leaves the
  extension mechanism unspecified, so the first future binding-mode addition eats the
  ABI-shape call under pressure. The minimal commitment here removes that trap for
  near-zero cost.
