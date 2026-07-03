# ADR-0044: The reconstructor is grounded metamorphically, not by the Python oracle

- **Status:** Proposed (pending architect ratification)
- **Date:** 2026-07-01
- **Depends on:** ADR-0026 (behaviour scoped to the oracle), ADR-0030 (oracle
  honesty / no silent skips)

## Context

The architect commissioned a serialization engine: turn a shaped parse tree back
into source text (`lark_rs::reconstruct::Reconstructor`). This is new public API
whose *output* has no byte-level Python oracle:

- Python Lark's counterpart (`lark.reconstruct.Reconstructor`) is explicitly
  experimental, its output text depends on which Earley derivation its matcher
  happens to resolve, and it is not canonical — two correct reconstructors can
  emit different bytes for the same tree. Freezing its bytes as fixtures would
  pin an implementation artifact, exactly what ADR-0017 tells us not to chase.
- ADR-0026 requires behaviour beyond the oracle to carry an architect-approved
  validation story in the escalation order *partial oracle → relative oracle →
  property tests → curated residue*. The architect approved metamorphic
  grounding for this feature explicitly when commissioning it.

What *is* falsifiable is the reconstructor's purpose: the emitted text must be
a sentence of the same grammar that re-parses to the same tree.

## Decision

Reconstruction is grounded by the **metamorphic round-trip property**, enforced
in-repo with no Python in the loop:

> for any grammar G and input x that lark-rs parses,
> `parse(reconstruct(parse(x)))` is structurally equal to `parse(x)`
> (node labels, child shapes, token types and values; positions excluded).

Two enforcement layers, both in CI:

1. **Curated property tests** (`tests/test_reconstruct.rs`): one grammar per
   tree-shaping feature the reconstructor must invert (filtered punctuation,
   transparent rules, expand1 collapse, aliases, `!`/`keep_all_tokens`, EBNF
   helpers, templates, `%ignore`, `term_subs`), the typed refusals, and
   small-stack robustness.
2. **A whole-bank sweep** (`tests/test_reconstruct_bank.rs`): every accepted
   case of the LALR compliance bank is round-tripped, gated by an XFAIL ledger
   (`reconstruct_xfail.json`, regenerated via `LARK_RECONSTRUCT_WRITE_XFAIL=1`)
   under the usual burndown discipline — the ledger only shrinks, and the build
   fails on regressions. Refusals and residue are typed and listed, never
   silently skipped (ADR-0030 spirit).

The *matching algorithm* still mirrors Python's `TreeMatcher` design (recons
rules over a tree-children alphabet, Earley matching, discarded-terminal
write-back, `term_subs`, `insert_spaces`), so the architecture stays
recognizable next to the reference implementation — but its behaviour is
allowed to exceed Python's where the metamorphic gate proves the improvement.
The deliberate divergences, each caught-or-motivated by the gate and pinned by
`tests/test_reconstruct.rs` + the bank sweep:

- **Bridge rules for every expand1 origin** (Python only bridges some): in
  Python's scheme an *uncollapsed* `?list: item+` node is unmatchable — its
  only recons rule is a unary helper reference, and no bridge lets a `list`
  reference consume the surviving node.
- **Multi-symbol expand1 rules also get a span-one global copy**: `?r: _x B`
  collapses exactly when it kept one child (`_x` spliced empty), so the rule
  must explain a collapsed *reference* too — constrained to a one-child span
  so it can never swallow multi-child sequences a sibling alternative really
  produced. Python's root-only routing makes such collapsed trees unmatchable.
- **Alias rules are root-only in both directions**: never predicted for an
  inner reference (a collapsed `?a: D` must not write a colliding
  `x: D ";" -> a`'s `";"`), and preferred over global structural rules when
  root-matching a surviving node (which was by definition produced by a
  node-labeling rule).
- **Duplicate-shape alternatives keep the *soundest* variant**: an unwritable
  (non-literal) discarded terminal that is `%ignore`d may be dropped — that is
  provably tree-neutral, the re-parse ignores it — but otherwise the MOST
  discarded write-outs win. Dropping a distinguishing token can flip the
  re-parse to a higher-priority sibling rule (`b.1: "A"+ "B"?` losing its
  `"B"` re-parses as `a.2: "A"+`), and silently dropping a *required*
  non-ignored `_WS` would corrupt the tree where Python fails loudly — so the
  unwritable variant wins and errors typed (`NonLiteralTerminal`) unless
  `term_subs` supplies it, matching Python's `NotImplementedError` posture.
- **Separator insertion is grammar-aware**: the inserted separator is one an
  `%ignore` terminal can actually absorb (`" "`, `"\n"`, `"\t"`, else an
  `%ignore`d fixed-string terminal's own text); a grammar that ignores nothing
  insertable gets exact concatenation. Python inserts `" "` unconditionally,
  which can never re-parse in a grammar that does not ignore whitespace — the
  failure mode of roughly half the bank before this rule. Fusion detection is
  Unicode `XID_Continue` (the `unicode-ident` crate), Python's
  `is_id_continue` semantics — combining marks count.

## Consequences

- **Buys:** a serialization engine whose correctness is checked by CI over
  hundreds of real grammars without any new Python tooling; a reusable pattern
  (metamorphic banks) for future oracle-less features; real bug-discovery
  pressure (both matcher divergences above were caught by the sweep, not by
  inspection).
- **Costs / rules out:** no byte-stability guarantee — the output is *a*
  canonical text, not the input text, and its exact bytes may change as the
  dedup/derivation policy evolves (only the round-trip property is pinned).
  Grammars whose trees genuinely underdetermine the text (a *required*
  discarded regex terminal, e.g. an explicit `_WS` separator rule) need
  `term_subs`, exactly as in Python; they are the typed `recons:` residue in
  the ledger. `maybe_placeholders` grammars are refused up front (typed
  error), matching Python's assert — supporting them is future work that would
  burn down the ledger's `placeholders:` entries.
- **Tripwires:** if a real dependent ever needs byte-stable output, that is a
  new escalate-tier decision (a canonical-formatting contract, not just a
  round-trip). If the reconstructor is wired into a binding (PyO3/WASM/C),
  the public-output-mode rule of ADR-0026 applies to that surface again.
