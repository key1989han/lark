# ADR-0046: Standalone bake (L5) — byte-class DFA tables as static data + interned output in generated parsers; refuse-with-override for contextual-load-bearing grammars

- **Status:** Accepted — ratified by the architect (2026-07-04)
- **Date:** 2026-07-03

## Context

`docs/LEXER_DFA_PLAN.md` reserved an L5 "bake" phase: ship the scanner as
static data inside `generate_standalone` / `include_lark!` parsers. The epic
(#620) was originally priced against the in-process contextual lexer's cost
model — ×47–108 per-state scanners, several MB of rodata, compile-time cost.
The 2026-07-03 standalone-bake spikes (three independent harnesses,
consolidated in `docs/notes/spike-standalone-bake-2026-07-03.md`) showed that
model does not apply to the standalone surface:

- The standalone runtime is a **`regex`-crate basic lexer — one scanner per
  grammar**, so the bake multiplier is ×1: 14–235 KiB flat per grammar,
  14–105 KiB byte-class-compressed.
- Baking a static table lets the generated parser **drop the `regex`
  dependency entirely**: stripped binary 4.1–4.5× *smaller*, cold compile
  4–12× *faster* (the removed dep + its LTO IR; the table itself is free for
  rustc). The two costed risks invert into gains.
- Measured value is **bimodal**: ~1.3–1.8× on one large document (reused
  parse), up to ~27× one-shot on small inputs (the per-invocation `Regex::new`
  is erased) — the many-small-inputs CLI/embedded pattern is the standout.
- Correctness is gateable: every spike variant was token-stream-differential
  gated and FNV-tree-digest-identical to the in-process basic-lexer oracle,
  including case-insensitive keyword (`unless`) grammars via a regex-free
  `eq_ignore_ascii_case` retype.
- Post-bake the generated parser is **output-bound** (LR drive + owned tree);
  retyping the generated runtime's `Token::type_`/`Tree::data` to
  `&'static str` (names are already `'static` in the baked data) removes
  exactly (tokens + shifted tokens + tree nodes) allocs/parse, 1.19× over
  baked, digest-identical.
- Scope findings: only grammars whose plain scanner DFA-flattens can bake, and
  a grammar can generate yet be contextual-load-bearing — the basic-lexer
  standalone then fails on its real corpus at runtime (poetry_pep508: 0/14).
  Today `generate_standalone` ships such parsers silently.

## Decision

1. **Bake data, not code**: emit the scanner as a **byte-class-compressed**
   `u32[state × classes]` static table + 256-byte class map (the ~5%
   throughput giveback buys a 1.5–4.9× smaller table and stays dependency
   free); flat-256 is an opt-in for hot grammars. `regex-automata::to_bytes`
   embedding is **rejected** — it keeps the crate linked and forfeits the
   size/compile inversion. Rust source codegen (goto/switch) is rejected by
   measurement (no computed goto; loses to the flat table).
2. **The bake must leave `regex` fully droppable**: the case-insensitive
   `unless` retype is re-expressed with `eq_ignore_ascii_case` (a non-ASCII
   ci keyword takes a Unicode-fold compare, still regex-free).
3. **Interned output in the generated runtime**: `Token::type_` / `Tree::data`
   become `&'static str` referencing the baked data (the standalone
   counterpart of ADR-0042's default-path label decision).
4. **Per-scanner graceful degradation, refuse-with-override at the front
   door**: a grammar whose scanner does not flatten keeps the current regex
   path — generation never fails on bake-ineligibility alone. But when a
   grammar is **contextual-load-bearing** (the basic-lexer standalone would
   fail on inputs the in-process contextual parser accepts),
   `generate_standalone` **refuses by default**, naming the load-bearing
   terminals, with an explicit override flag — a warning is too easy to miss
   in codegen workflows, and silently shipping a runtime-failing parser is the
   worst outcome.
5. **Explicit non-claims / splits**: the *capability* bake (baking the guarded
   engine so grammars the `regex` runtime rejects become standalone-able) is a
   separate epic with its own ADR; "lalrpop speed band" is **not** claimed —
   baked reused throughput is in-process `parse()` parity, and that tier would
   additionally need LR-code specialization + typed-AST output (unscoped); a
   tape output for the generated runtime rides the ADR-0045/#619 line and is
   bounded by the standalone `tok+` O(n²) (#624) until that is fixed.

## Consequences

- **Buys:** dependency-free generated parsers that are ~4× smaller and ~4–12×
  faster to compile, 25–27× faster one-shot on small inputs, modestly faster
  (1.3–1.8×) on large documents — and an existing footgun (silently shipping
  contextual-load-bearing grammars) is closed.
- **Costs:** the baked drive loop is a second copy of scanner semantics under
  the ADR-0008 one-runtime rule — recurring maintenance; the per-grammar
  token-stream differential + oracle tree digest make drift loud and are the
  required gate for every generated grammar (alongside `test_standalone.rs`'s
  round-trip/freshness net). The beneficiary set on today's wild bank is
  narrow (grammars that both flatten and basic-lex); the one-shot economics
  and the dependency drop are the justification, not breadth.
- **Rules out:** `to_bytes` embedding; scanner source codegen; treating rodata
  or compile time as blocking risks for this surface.
- **Tripwire:** if a future standalone surface gains the contextual lexer, the
  ×1 multiplier assumption (and this ADR's footprint math) must be re-measured
  before extending the bake there.
- Resolves the #620 scope decisions. Blast radius: generated-parser public
  surface → escalate-tier implementation; the refusal policy (decision 4) is a
  behavioural change to `generate_standalone`'s contract.
