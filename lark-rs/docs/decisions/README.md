# Architecture Decision Records (ADRs)

This folder is the **why** of lark-rs: the load-bearing decisions, each as a
short, dated, append-only record. A decision is a historical fact — once made it
doesn't go stale, so unlike implementation prose these records need almost no
maintenance. When you (the steerer) ask "why did we do it this way," this is the
first place to look.

The records here were **backfilled on 2026-06-13**: ADRs 0001–0009 from the
rationale in [`CLAUDE.md`](../../CLAUDE.md) and [`docs/STATUS.md`](../STATUS.md),
and ADRs 0010–0015 from a sweep of the PR history (#1–161) for decisions —
especially *reversals and abandoned approaches* — that lived only in PR
descriptions. New decisions get a new file from then on.

**Who sets `Status`.** A newly authored ADR is **always `Proposed` (pending
architect ratification)** — an agent never accepts its own decision. Only the
**architect** flips an ADR to `Accepted`, by merging the PR that carries it.
Authoring a new ADR as `Accepted` is a Definition-of-Done failure (see
`.claude/commands/finish-task.md` and `TEMPLATE.md`).

## Index

| # | Decision | Status |
|---|---|---|
| [0001](0001-python-lark-is-the-oracle.md) | Python Lark is the oracle (oracle-first testing) | Accepted |
| [0002](0002-true-lalr1-not-slr.md) | Compute true LALR(1) lookaheads, not SLR FOLLOW | Accepted |
| [0003](0003-intern-symbols-to-ids-with-flags.md) | Lower to integer `SymbolId`s; semantics as flags, not name-prefixes | Accepted |
| [0004](0004-python-re-regex-dialect.md) | Terminal regexes follow the Python `re` dialect | Accepted |
| [0005](0005-lower-lookaround-into-the-dfa.md) | Lower bounded lookaround into the DFA; no backtracking runtime engine | Accepted |
| [0006](0006-dfa-default-lexer-backend.md) | DFA (`regex-automata`) is the default lexer backend | Accepted |
| [0007](0007-deterministic-perf-counters.md) | Gate performance on deterministic work counters, not wall-clock | Accepted |
| [0008](0008-standalone-shares-one-runtime.md) | Standalone parsers share one compiled runtime + the same scanner plan | Accepted |
| [0009](0009-xfail-burndown-discipline.md) | Known gaps are XFAIL allow-lists that only shrink | Accepted |
| [0010](0010-lookaround-strategy-history.md) | Lookaround strategy history — three approaches abandoned before the DFA | Accepted |
| [0011](0011-parsing-is-allocation-bound.md) | Parsing is allocation-bound; tree representation is the deferred headroom | Accepted |
| [0012](0012-differential-fuzzer-active-oracle.md) | Differential fuzzer — turn the static oracle into an active one | Accepted |
| [0013](0013-ebnf-nullable-helper-distribution.md) | EBNF nullable helpers — distribute non-final, share only the recurse core | Accepted |
| [0014](0014-patternstr-vs-patternre-classification.md) | Recover PatternStr/PatternRE structurally via a `string_type` flag | Accepted |
| [0015](0015-one-shared-treebuilder-consolidate-before-features.md) | One shared tree shaper; consolidate seams before adding algorithms | Accepted |
| [0016](0016-tiered-merge-autonomy.md) | Tiered merge autonomy by blast radius | Accepted |
| [0017](0017-oracle-fidelity-is-for-intended-behavior.md) | Oracle fidelity is for *intended* behavior, not implementation artifacts | Accepted |
| [0018](0018-start-sprint-orchestration.md) | `/start-sprint` — whole-backlog autonomy via an integration branch + omnibus PR | Accepted |
| [0019](0019-recovered-tree-is-optional-at-premature-eof.md) | `RecoveredTree.tree` is `Option`, `None` at premature `$END` (no fabricated partial) | Accepted |
| [0020](0020-postlex-recovery-mirrors-resume-parse.md) | LALR recovery over a postlex (Indenter) hook — mirror Python's per-resume `Indenter.process` reset | Accepted |
| [0021](0021-differential-audit-checkpoint.md) | Differential-audit checkpoint in the review discipline — banks are a regression net, not a completeness net | Accepted |
| [0022](0022-kaizen-sweep-omnibus.md) | `/kaizen-sweep` drains the whole kaizen backlog via the omnibus pattern (mirrors ADR-0018) | Accepted |
| [0023](0023-sprint-ledger-durability.md) | Sprint/kaizen ledger durability — reconstruct-at-finalize + a committed append-only residue, not a churned PR body (refines ADR-0018) | Accepted |
| [0024](0024-cyk-empty-rule-rejection-by-provenance.md) | CYK empty-rule rejection keyed on source provenance (generated-helper `anon_kind`), not name spelling | Accepted |
| [0025](0025-no-backward-compat-pre-users.md) | Pre-users: breaking the public API is free — no backward-compatibility constraint until lark-rs has users | Accepted |
| [0026](0026-behaviour-scoped-to-the-oracle.md) | Behaviour is scoped to the Python Lark oracle; beyond-oracle behaviour is escalate + needs a validation story (resolves #211) | Accepted |
| [0027](0027-semantic-output-builders-direction.md) | Semantic output backends — `TreeBuilder` becomes the default impl of an internal `OutputBuilder` seam; LALR parity oracle-backed, fast backends relative-oracle-backed | Accepted |
| [0028](0028-recovery-action-design.md) | `RecoveryAction` enum over direct `&mut InteractiveParser` for `on_error` (insert/delete/stop recovery) | Accepted |
| [0029](0029-output-builder-public-api-shape.md) | Public `OutputBuilder` API shape — per-call `parse_into`, `is_discard` hook, `ctx` metadata, LALR-only; commit `Tree` + Rust `Custom`, `SpanTree` experimental, `Event`/`Tape` internal | Accepted |
| [0030](0030-oracle-generators-fail-loud.md) | Oracle generators fail loud on un-allow-listed contradictions; the oracle suite is honest by construction (no silent skips, no self-referential fields) | Accepted |
| [0031](0031-good-autonomous-fix-site-verification.md) | `good-autonomous` carries a fix-site **verification status** (not a blocking gate): `/triage` checks the stated fix site against a fast repro; verified sites are load-bearing, unverified sites keep the label but carry a "fix site unverified" note (bug-hackathon marks fix sites hypothesised vs verified) | Accepted |
| [0032](0032-closeout-files-retro-flagged-kaizen-items.md) | Sprint/kaizen/task close-out must file every retro-flagged kaizen item (required, checkable) + a dual-arm follow-ups report — closes the §7 lapse #284 made (dropped kaizen follow-ups; recovered as #309–#314) | Accepted |
| [0033](0033-parsetree-none-root.md) | Add `ParseTree::None` — a bare-`None` root parse result (`?start: [A]` on `""`, #289); a source-breaking public-enum addition → escalate-tier | Accepted |
| [0034](0034-priority-width-i64.md) | Terminal/rule priority storage widened `i32` → `i64` (loader clamp + public `RuleOptions`/`TerminalDef` fields + Earley/CYK accumulators) so two distinct very-large priorities no longer saturate and tie (#352); public-API change → escalate-tier | Accepted |
| [0035](0035-freeze-python-re-dialect-parity.md) | Freeze Python `re` terminal-dialect parity work + defer the dialect-mode architecture until a user-needs signal (the `frozen` label; #416 is explicitly not frozen) | Accepted |
| [0036](0036-pyo3-token-is-a-str.md) | PyO3 `Token` IS-A `str`, realised in pure Python under `abi3` (PyO3 can't subclass `str` with `abi3`); engine becomes the `lark_rs._lark_rs` submodule, `Token`/`Meta` are Python types the Rust core constructs (#416); public binding surface → escalate-tier | Accepted |
| [0037](0037-dfa-hybrid-fallback-for-over-budget-terminals.md) | Over-budget terminals fall back to a lazy/hybrid DFA sub-engine instead of the eager dense union (the `.*a.{N}` determinizer blow-up, #349); byte-identical matches, flat build cost, grammar still builds | Accepted |
| [0038](0038-generic-value-placeholder-and-discard.md) | Generic `OutputBuilder::Value` needs a `placeholder(ctx) -> Value` method (`maybe_placeholders` `None`) and a `Discard`-after-placeholders drop order (C7, #232); refines fork-5 so `token()` carries lexer positions + owned value for byte-identical C7; both no-ops for `Value = Child` | Proposed |
| [0039](0039-embedded-transformer-is-the-c8b-consumer.md) | The embedded transformer is C8b's named event-sink consumer (#242): the backend is grounded against the embedded-transformer oracle + the zero-tree counter and stays internal — a public transformer/`OutputMode` surface remains the separate escalate-tier call (C10, #244) | Proposed |
| [0040](0040-token-meta-positions-u32.md) | Source positions stored as `u32` behind `usize` accessors (position fields `pub(crate)`), shrinking `Meta` 104→56 B and `GSlot<Child>` 264→~168 B — a pure-memcpy win shared by every engine and `OutputBuilder` backend; a `wide-positions` feature restores `usize` for the rare >4 GiB input; public-API change → escalate-tier (PR #602) | Accepted |
| [0041](0041-committed-seed-range-grammar-fuzz-regression-tier.md) | The grammar fuzzer's regression tier is a **committed seed range** (`scripts/fuzz-seed-range.sh`, gating nightly job) — a random-grammar corpus stored as one line, with XFAIL-ledger ratchet semantics (widen freely once verified; narrowing needs allow-list-level scrutiny); amends ADR-0012's two-tier taxonomy for `--fuzz-grammars` (#208) | Accepted |
| [0042](0042-intern-tree-token-labels.md) | Intern `Tree.data` / `Token.type_` labels behind `&str` accessors (`Arc<str>` storage) — kills the per-token/per-node name allocation (~1.20× on the default `parse()` path); public accessor change → escalate-tier (resolves #604) | Accepted |
| [0043](0043-bindings-output-mode-taxonomy.md) | Bindings `OutputMode` (extends ADR-0029's deferred binding half) — commit the *extension mechanism* (additive C enum + string kwarg for PyO3/WASM, no FFI callback), expose `Tree` only for now, defer `SpanTree`/`Tape` to a consumer-driven trigger (which is also the C8b/C8c named-consumer gate); resolves #244 | Accepted |
| [0044](0044-reconstructor-metamorphic-grounding.md) | The reconstructor (tree → text, `src/reconstruct.rs`) is grounded by the metamorphic round-trip property `parse(reconstruct(parse(x))) == parse(x)` — curated tests + a whole-LALR-bank sweep under an XFAIL ledger — not by a Python byte oracle (Python's own reconstructor is experimental/non-canonical); architect-commissioned per ADR-0026 | Proposed |
| [0045](0045-owned-tree-arena-backend-sequenced-b1-rerank.md) | Owned-tree M2+M5 land **sequenced**: opt-in arena/interned output backend behind the `OutputBuilder` seam first, default `Tree`/`Child` unchanged (ADR-0042 already covers the default-path label allocations); baked DFA re-ranked from in-process lever to one-shot/standalone (ADR-0046); `Token.type_` field-drop superseded by ADR-0042 (its rejected Option 2) — allocation intent delivered, the 16 B + 2-atomic-ops/token residual vs `type_id` deferred to the M6 token-surface change; resolves #619 | Proposed |
| [0046](0046-standalone-bake-byte-class-tables.md) | Standalone bake (L5): byte-class DFA tables as `static` data in generated parsers (flat-256 opt-in, `to_bytes`/codegen rejected), regex-free ci-`unless` retype so the `regex` dep drops, `&'static str` interned output; contextual-load-bearing grammars get **refuse-with-override**; capability bake + lalrpop-parity split out; resolves #620 scope | Proposed |

## ADRs going forward — the governance audit trail

The backfilled records above are history. New ones are *also* a control surface.
Under the autonomy kit ([`../PRINCIPLES.md`](../PRINCIPLES.md)), an agent records an
ADR when it deviates from a §3 default, or makes an architecture/policy call it
can't fully ground in a test (`PRINCIPLES.md` §3–4). The architect reads these in
arrears, in batch, and promotes any correction into `PRINCIPLES.md` §2/§3 — so the
constitution sharpens over time instead of drifting. That is why the log is
append-only, and why a `Proposed` *policy* ADR (e.g. ADR-0016) is legitimate here:
an ADR is both the record of *why we did it* and the audit trail for *decisions an
agent made without the architect in the loop*.

## ADRs are decision records, not session transcripts

Keep ADRs self-contained and stable: context, decision, consequences, and the
validation gate. The following belong in **PR bodies, issues, or explicitly
non-normative notes** — not in the decision record:

- PR state, branch names, session IDs, or transient merge-tier routing ("Reviewed
  as `escalate`-tier", "architect approves via the omnibus"). Noting a change's
  *durable* blast-radius classification (e.g. "this is a breaking API change,
  `escalate`-tier") is fine in Consequences — it is a standing fact about the
  decision, not a routing instruction for a specific PR.
- Implementation queues, command routing, or task plans
- Model provenance or AI session transcripts
- Sprint retrospective narratives ("Two problems surfaced the first time it ran…")
- Meta-commentary about the ADR-writing process itself ("An earlier draft of this
  ADR…", "This ADR preserves it so the dead ends aren't re-explored")

Issue and PR numbers are fine as **parenthetical citations** for traceability
(e.g. "the `~n`-inlining fix (#176)"), but the ADR must stand on its own without
reading those references. "Worked examples" that illustrate the decision's
application are welcome when they name the principle being applied, not just the
ticket.

## How to add one

1. Copy [`TEMPLATE.md`](TEMPLATE.md) to `NNNN-short-title.md` (next number).
2. Fill in Context / Decision / Consequences. Keep it to a screen.
3. Add a row to the index above.
4. Set Status: `Proposed` → `Accepted`. To reverse a past decision, add a **new**
   ADR that supersedes it (set the old one's status to `Superseded by NNNN`) —
   never delete or rewrite history.

**Renumber-on-rebase (parallel-branch convention).** The "next number" in step 1
is the next free integer *at authoring time* — but `master` moves while a PR is
open, so two parallel branches can independently mint the **same** number and
collide on rebase (a duplicate `NNNN-*.md` filename + an index conflict; this bit
#195/#207). Before rebasing your branch onto `master`, **re-check the highest ADR
number on `master` and renumber your proposed ADR to the next free slot**, with a
`git grep ADR-NNNN` reference sweep (the file, this index row, and any
`CLAUDE.md`/code citation). Your ADR is `Status: Proposed` until merge, so the
integer is not yet load-bearing — moving it is cheap, and doing it *before* the
rebase keeps it routine instead of a surprise conflict.

## The maintenance rule (keep docs from rotting)

A PR that **changes a load-bearing decision** must, in the same PR, either add a
new ADR or mark an existing one superseded. The durable docs
([`ARCHITECTURE.md`](../../ARCHITECTURE.md), [`GLOSSARY.md`](../../GLOSSARY.md),
and these ADRs) are short and reference module paths so drift is easy to spot.
Everything fast-changing is documented by the tests, not by prose.
