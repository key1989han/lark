//! Deterministic work counters for the profile-first perf discipline (#54/#55/#56).
//!
//! `BENCH.md` is emphatic that the wall-clock bench is a *recorded trend, not a
//! gate* — shared-runner timing is too noisy to enforce, and a flaky red perf
//! gate gets muted. A super-linearity **regression test** therefore cannot key on
//! time. The noise-free analog these counters provide is an instrumented
//! copy/scan counter: assert *flat per-byte scaling* of a deterministic count and
//! you have something that can actually gate (`tests/test_earley_scaling.rs`).
//!
//! The counters are compiled to genuine no-ops unless the `perf-counters` feature
//! is on, so the normal build and the hot parse path are untouched (the increments
//! sit inside the Earley completer scan and the explicit forest walk). Enable them
//! with `--features perf-counters`; the scaling test self-gates on the same `cfg`.
//!
//! Three counters, mapping onto the two candidate culprits tracked in #56:
//!
//! * [`completer_scan_steps`] (Arm 1) — every item the Earley completer examines
//!   when it looks up an origin column's waiters (`predict_and_complete`). With the
//!   per-column `waiting` index this is the *named* "completer rescans the origin
//!   column" cost; the index makes it flat per byte on realistic shapes (the fix).
//! * [`explicit_prefix_copies`] (Arm 2, the *named* suspect) — every
//!   [`NodeValue`](crate::parsers) copied by `expand_packed`'s cartesian-product
//!   loop (`l = list.clone()`). This counter **disproves** the issue's guess: it
//!   stays *linear* even on a transparent left-recursive helper.
//! * [`explicit_node_children`] (Arm 2, the *real* cost) — every materialized
//!   derivation-value child built per symbol node in `symbol_derivations`. This is
//!   where the explicit walk is genuinely O(n²) on a transparent left-recursive
//!   helper (Inlines of size 1,2,…,n); the streaming fix that would flatten it is a
//!   tracked follow-up (the explicit analog of #55's resolve-mode fix).
//!
//! Two more counters back the **cyclic explicit-Earley re-assembly** gate (#518).
//! `ambiguity='explicit'` over a *cyclic* (nullable+recursive) grammar disables the
//! per-symbol `deriv_memo`/`memo` for cycle nodes and governs them via the
//! per-packed-node `packed_cache` (Python's `_cache` model, #348). `packed_cache`
//! bounds re-*descent*, but a cyclic symbol node's derivation list is still
//! re-`assemble`d on each reach (its `deriv_memo` is never written). Cyclic
//! ambiguous grammars have an inherently exponential *distinct-derivation* count
//! (the true answer, not an artifact — `1,1,2,8,48,352` for `z: | "b" z | z z`), so
//! the gate keys on *per-materialized-derivation* work, never raw total work
//! (BENCH.md / §2.5):
//!
//! * [`explicit_assemble_children`] — every child *slot* the explicit walk feeds to
//!   `TreeOutputBuilder::assemble` while re-building a packed node's derivation
//!   values (`DerivsNext`). This is the re-assembly work the cyclic path repeats on
//!   each reach of a cycle node. On its own it grows with the (exponential)
//!   derivation count, so it is only meaningful *per derivation*.
//! * [`explicit_derivations`] — every materialized derivation value the walk admits
//!   into a node's deduped list (`DerivsNext`'s `push_deduped`). The denominator: the
//!   count of materialized output derivations. A flat `assemble_children /
//!   derivations` envelope says each materialized derivation costs a bounded amount
//!   of re-assembly regardless of how many times its cyclic node is reached; a future
//!   super-linearity in the re-assembly path (e.g. dropping `packed_cache`, or
//!   re-assembling a memoizable subtree per reach) makes the ratio climb and trips the
//!   gate (`tests/test_earley_scaling.rs`, the cyclic arm).
//!
//! A fourth counter backs the CYK scaling gate (#87):
//!
//! * [`cyk_table_steps`] — every `(split, left-nt, right-nt)` combination the CYK
//!   DP examines while filling its triangular table. CYK is inherently
//!   `O(n³ · |grammar|)`; asserting this count scales cubically (flat per `n³`)
//!   over a size sweep catches an accidental complexity regression in the CNF
//!   conversion or the DP, mirroring the Earley methodology
//!   (`tests/test_cyk_scaling.rs`).
//!
//! A sixth counter backs the **dense-DFA build-cost** gate (`docs/LEXER_DFA_PLAN.md`,
//! the determinization-blowup risk):
//!
//! * [`dense_build_bytes`] — the determinized heap size (`dense::DFA::memory_usage`,
//!   a deterministic proxy for the state count × stride) of each `dense::DFA` the
//!   lookaround-lowering `DfaScanner` builds (the combined base engines plus each
//!   guard body). Determinization is the cost the **L5 bake** pays (`to_bytes` needs a
//!   fully-determinized dense DFA), and a lowering that blows it up — parity
//!   duplication, spliced branches, an interacting union of per-state contextual
//!   scanners — would show as a super-linear climb in this count. Asserting it stays
//!   flat per terminal (and per guard width) over a size sweep
//!   (`tests/test_lexer_dfa_build_scaling.rs`) is the deterministic gate, the codegen-
//!   time analog of the Earley/CYK scaling gates (paid at standalone generation, not
//!   every runtime load).
//!
//! A seventh counter backs the **grammar-build cross-product** gate (#404, H6-7):
//!
//! * [`expansion_alts`] — every intermediate alternative the EBNF rule-body
//!   compiler materializes in `compile_expansion`'s per-position cartesian fold
//!   (the running `acc` product, summed across fold steps). A chain of `k`
//!   duplicate-arm inline groups (`(X|X) (X|X) … (X|X)`) that folds *without*
//!   per-step dedup materializes the full `m^k` product before collapsing — a
//!   `2^k` build blowup. Folding with `concat_alts_dedup` bounds the running set
//!   to the *distinct* alternatives at each prefix length, so this count stays
//!   flat in `k`. Asserting that (`tests/test_grammar_build_scaling.rs`) is the
//!   deterministic gate — the grammar-build analog of the Earley/CYK/lexer
//!   scaling gates, paid at load, never wall-clock.
//!
//! An eighth counter backs the **LALR parse-table density** gate (#367, H5-9):
//!
//! * [`parse_table_action_cells`] — every ACTION cell the in-process `ParseTable`
//!   *stores* (summed over all states). A grammar whose state and terminal counts
//!   both grow with size (`start: r0 | … | rn` / `ri: Ai Bi Ci`) has a dense
//!   `action[state][terminal]` matrix of `O(states × terminals) = O(n²)` cells,
//!   while the semantic content — the `Some` actions, matching Python Lark's
//!   sparse dict-of-dicts — is `O(n)`. Counting the cells the representation
//!   actually stores makes the *dense* allocation `O(n²)` and the *sparse*
//!   per-state `(terminal id, action)` rows `O(filled)`. Asserting the count
//!   stays `O(filled)` (not `O(states × terminals)`) over the size sweep
//!   (`tests/test_lalr_table_scaling.rs`) is the deterministic gate — the
//!   parse-table analog of the Earley/CYK/lexer scaling gates, paid at build,
//!   never wall-clock.
//!
//! A ninth group of counters backs the **output-shape** gate (semantic-output C5,
//! #230). They make the "the fast path builds the right *shape* of output" claim
//! falsifiable without wall-clock (ADR-0007): they count what the shared
//! [`TreeOutputBuilder`](crate::parsers) materializes as it shapes each reduction,
//! so a future tree-bypassing semantic backend (C7/C8) can be held to *building
//! fewer nodes* by the same deterministic discipline.
//!
//! * [`tree_nodes_built`] — every `Tree` node the builder materializes
//!   (`TreeOutputBuilder::build_node`). The default tree output has
//!   `tree_nodes_built > 0`; a future zero-tree span backend (C8) must drive it to
//!   `0` for the same parse — the eventual gate the C5 infrastructure is laid for.
//! * [`token_value_string_bytes`] — the total byte length of every token *value*
//!   the builder materializes into the output (`TreeOutputBuilder::build_token`).
//!   The default builder copies each kept token's value string, so this scales with
//!   the kept-token payload; the span backend (C8) that keeps offsets instead of
//!   owned strings drives it to `0` (the `token_value_string_bytes == 0` gate #230
//!   defers to C8).
//! * [`lexer_token_value_bytes`] — the total byte length of every token *value* the
//!   **lexer** materializes into an owned `Token.value: String` (the `value.to_string()`
//!   sites in `lexer/mod.rs`). This is the *upstream* allocation, distinct from
//!   [`token_value_string_bytes`] (which counts the *output* copy): the C8 output
//!   backend drove the output counter to 0 while the lexer kept allocating, so the
//!   two must be separable to prove "the pipeline allocated no token strings" as a
//!   counter result (C8.1 #582, ADR-0007). The span-emitting lexer path (`parse_span`)
//!   builds value-less tokens and drives this to `0`; the default `parse()` /
//!   `parse_into` owned path keeps it `> 0`. Gated in `tests/test_span_tree.rs`.
//! * [`semantic_reduce_calls`] — one per `TreeOutputBuilder::assemble` call, i.e.
//!   one per rule reduction the engine shapes into a value through `assemble`. The
//!   LALR and CYK reducers (and the Earley *explicit* walk's `DerivsNext`) all route
//!   each reduction through `assemble`, so for a known **LALR/CYK** input this equals
//!   the parser's user-rule reduction count (the augmented `$root` accept does *not*
//!   route through `assemble`, so it is not counted). The Earley *resolve* walk
//!   shapes via `TreeOutputBuilder::shape`/`shape_with_container` directly (it never
//!   calls `assemble`), so this counter does **not** track Earley-resolve reductions
//!   — unlike [`tree_nodes_built`]/[`token_value_string_bytes`], which live in
//!   `build_node`/`build_token` and are engine-agnostic. It is the denominator that
//!   makes the per-reduction output-build cost a flat envelope on the LALR/CYK paths
//!   (`tests/test_output_counters.rs`, an LALR gate).
//! * [`child_vec_allocs`] — one **fresh** child-buffer allocation charged per
//!   `shape_reduction` materialization on the `parse_into` path (#583/C8.2). It
//!   tracks the reduction's owned `kept` buffer as a per-buffer unit, **not** a raw
//!   allocator count. Since the child-buffer reuse pass (perf spike 2026-07-02) a
//!   reduction usually *recycles* instead of allocating — stealing a transparent
//!   `Inline` splice buffer, taking the banked `ReduceScratch` buffer, or (for an
//!   arity-1 `expand1` collapse) needing no buffer at all — and none of those charge
//!   the counter. The counter therefore reports the **reuse ratio** the #583 gate
//!   doc anticipated: `child_vec_allocs / semantic_reduce_calls` sits *below* 1
//!   (on the gate's list grammar it is exactly 1 fresh buffer per parse,
//!   independent of input size — `tests/test_child_vec_scaling.rs` pins that
//!   closed form). Rising back toward one-per-reduction means the recycling was
//!   lost; rising past it means a per-child blowup. It lives on the
//!   value-parametric `shape_reduction` seam, so it is engine-scoped to the LALR
//!   `run_into` / `parse_into` path (Earley/CYK stay on the concrete
//!   `assemble`/`shape` path and do not increment it), exactly like
//!   [`semantic_reduce_calls`]'s LALR/CYK scoping.

#[cfg(feature = "perf-counters")]
mod imp {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    static COMPLETER_SCAN_STEPS: AtomicU64 = AtomicU64::new(0);
    static EXPLICIT_PREFIX_COPIES: AtomicU64 = AtomicU64::new(0);
    static EXPLICIT_NODE_CHILDREN: AtomicU64 = AtomicU64::new(0);
    static EXPLICIT_ASSEMBLE_CHILDREN: AtomicU64 = AtomicU64::new(0);
    static EXPLICIT_DERIVATIONS: AtomicU64 = AtomicU64::new(0);
    static FOREST_NODES: AtomicU64 = AtomicU64::new(0);
    static CYK_TABLE_STEPS: AtomicU64 = AtomicU64::new(0);
    static LEXER_SCAN_STEPS: AtomicU64 = AtomicU64::new(0);
    static DENSE_BUILD_BYTES: AtomicU64 = AtomicU64::new(0);
    static EXPANSION_ALTS: AtomicU64 = AtomicU64::new(0);
    static PARSE_TABLE_ACTION_CELLS: AtomicU64 = AtomicU64::new(0);
    static TREE_NODES_BUILT: AtomicU64 = AtomicU64::new(0);
    static TOKEN_VALUE_STRING_BYTES: AtomicU64 = AtomicU64::new(0);
    static LEXER_TOKEN_VALUE_BYTES: AtomicU64 = AtomicU64::new(0);
    static SEMANTIC_REDUCE_CALLS: AtomicU64 = AtomicU64::new(0);
    static CHILD_VEC_ALLOCS: AtomicU64 = AtomicU64::new(0);
    static LEO_DISABLED: AtomicBool = AtomicBool::new(false);

    #[inline]
    pub fn add_completer_scan_steps(n: u64) {
        COMPLETER_SCAN_STEPS.fetch_add(n, Ordering::Relaxed);
    }

    #[inline]
    pub fn add_explicit_prefix_copies(n: u64) {
        EXPLICIT_PREFIX_COPIES.fetch_add(n, Ordering::Relaxed);
    }

    #[inline]
    pub fn add_explicit_node_children(n: u64) {
        EXPLICIT_NODE_CHILDREN.fetch_add(n, Ordering::Relaxed);
    }

    /// Count the child slots fed to one `TreeOutputBuilder::assemble` call in the
    /// explicit walk's per-packed-node re-assembly (`DerivsNext`) — the re-assembly
    /// work a cyclic node repeats on each reach (#518). Only meaningful divided by
    /// [`explicit_derivations`]; on its own it tracks the exponential derivation count.
    #[inline]
    pub fn add_explicit_assemble_children(n: u64) {
        EXPLICIT_ASSEMBLE_CHILDREN.fetch_add(n, Ordering::Relaxed);
    }

    /// Count one materialized derivation value admitted into a node's deduped list
    /// (`DerivsNext`'s `push_deduped`) — the denominator for the #518 per-derivation
    /// re-assembly envelope. Tracks the true (exponential) distinct-derivation count
    /// of a cyclic explicit grammar (`1,1,2,8,48,352` for `z: | "b" z | z z`).
    #[inline]
    pub fn add_explicit_derivation() {
        EXPLICIT_DERIVATIONS.fetch_add(1, Ordering::Relaxed);
    }

    /// Count one SPPF node creation. This is the mode-neutral size metric used to
    /// prove the Joop-Leo win (#58): the forest is O(n²) nodes on right recursion
    /// without Leo and O(n) with it — a comparison the scan counter alone cannot
    /// make (Leo zeroes the scan by skipping the cascade, but the question is
    /// whether *total* forest work is now linear).
    #[inline]
    pub fn add_forest_node() {
        FOREST_NODES.fetch_add(1, Ordering::Relaxed);
    }

    /// Count one CYK table-fill combination step: every `(split, left-nt,
    /// right-nt)` triple the DP examines when filling a span cell. This is the
    /// dominant cost of the `O(n³ · |grammar|)` table fill, so asserting it scales
    /// cubically (flat per `n³`) catches an accidental complexity regression in the
    /// CNF conversion or the DP — the CYK analog of the Earley completer-scan gate.
    #[inline]
    pub fn add_cyk_table_steps(n: u64) {
        CYK_TABLE_STEPS.fetch_add(n, Ordering::Relaxed);
    }

    /// Count lexer scan work: per per-position match attempt, the number of input
    /// bytes the (unanchored) regql search had to skip past `pos` before it found a
    /// candidate or gave up, plus one for the attempt itself. On a correctly
    /// *anchored* scanner this is ~1 per attempt (the search only looks at `pos`), so
    /// the total is linear in the token count. On an *unanchored* scanner a terminal
    /// that fails at `pos` makes the engine forward-scan toward the next possible
    /// match — O(remaining input) per position — so a low-rank lookaround terminal
    /// tried at every token boundary makes the total O(n²). Asserting this count
    /// stays flat per byte (`tests/test_lexer_scaling.rs`) is the deterministic gate
    /// for that pathology.
    #[inline]
    pub fn add_lexer_scan_steps(n: u64) {
        LEXER_SCAN_STEPS.fetch_add(n, Ordering::Relaxed);
    }

    /// Count the determinized heap size (`dense::DFA::memory_usage`) of one
    /// `dense::DFA` built while constructing a lookaround-lowering `DfaScanner` (a
    /// combined base engine or a guard body). Summing this across a scanner build is its
    /// determinization cost — the work the L5 bake pays — so asserting it stays flat per
    /// terminal/width catches a lowering that blows up dense-DFA construction
    /// (`tests/test_lexer_dfa_build_scaling.rs`).
    #[inline]
    pub fn add_dense_build_bytes(n: u64) {
        DENSE_BUILD_BYTES.fetch_add(n, Ordering::Relaxed);
    }

    /// Count intermediate alternatives materialized by `compile_expansion`'s
    /// per-position cartesian fold — the size of the running `acc` product after
    /// each fold step, summed over the rule body. A duplicate-arm inline-group
    /// chain (`(X|X) (X|X) …`) folded without per-step dedup makes this `2^k`; the
    /// deduping fold keeps it flat in `k` (#404, H6-7). The grammar-build analog of
    /// the Earley/CYK/lexer scaling counters; gated by
    /// `tests/test_grammar_build_scaling.rs`.
    #[inline]
    pub fn add_expansion_alts(n: u64) {
        EXPANSION_ALTS.fetch_add(n, Ordering::Relaxed);
    }

    /// Count the ACTION cells the in-process `ParseTable` *stores* — `n` is the
    /// total entry count across all of `action`'s per-state rows. With the dense
    /// `vec![vec![None; n_terminals]; n_states]` matrix this is `states ×
    /// terminals` (`O(n²)` on the #367 size sweep); with the sparse per-state
    /// `(terminal id, action)` rows it is the *filled* count (`O(n)`). Asserting
    /// the per-`filled` ratio stays flat (`tests/test_lalr_table_scaling.rs`)
    /// separates the two regimes — the parse-table analog of the Earley/CYK/lexer
    /// scaling counters.
    #[inline]
    pub fn add_parse_table_action_cells(n: u64) {
        PARSE_TABLE_ACTION_CELLS.fetch_add(n, Ordering::Relaxed);
    }

    /// Count one `Tree` node materialized by `TreeOutputBuilder::build_node` — the
    /// output-shape size metric (semantic-output C5, #230). The default tree output
    /// has this `> 0`; a future zero-tree span backend (C8) must drive it to `0`.
    #[inline]
    pub fn add_tree_node_built() {
        TREE_NODES_BUILT.fetch_add(1, Ordering::Relaxed);
    }

    /// Count the byte length of one token value materialized into the output by
    /// `TreeOutputBuilder::build_token` (semantic-output C5, #230). Scales with the
    /// kept-token payload of the parse; the span backend (C8) drives it to `0`.
    #[inline]
    pub fn add_token_value_string_bytes(n: u64) {
        TOKEN_VALUE_STRING_BYTES.fetch_add(n, Ordering::Relaxed);
    }

    /// Count the byte length of one token value the **lexer** materializes into an
    /// owned `Token.value: String` (the `value.to_string()` sites in `lexer/mod.rs`) —
    /// the upstream allocation, distinct from the output copy `token_value_string_bytes`
    /// counts (C8.1 #582). The span-emitting lexer path builds value-less tokens and
    /// leaves this at `0`.
    #[inline]
    pub fn add_lexer_token_value_bytes(n: u64) {
        LEXER_TOKEN_VALUE_BYTES.fetch_add(n, Ordering::Relaxed);
    }

    /// Count one `TreeOutputBuilder::assemble` call — one rule reduction shaped into
    /// a value (semantic-output C5, #230). For a known **LALR/CYK** input this equals
    /// the parser's user-rule reduction count (the augmented `$root` accept does not
    /// route through `assemble`). The Earley resolve walk shapes via `shape`, not
    /// `assemble`, so it does not increment this — see the module-level doc.
    #[inline]
    pub fn add_semantic_reduce_call() {
        SEMANTIC_REDUCE_CALLS.fetch_add(1, Ordering::Relaxed);
    }

    /// Charge one **fresh** child-buffer allocation on the `parse_into` shaping
    /// path (`shape_reduction`, #583/C8.2) — a per-buffer unit, not a raw allocator
    /// count. Recycled buffers (`ReduceScratch`, `Inline` steals) and the `expand1`
    /// fast path charge nothing, so the counter reports the reuse ratio (below one
    /// per reduction since the 2026-07-02 reuse pass). Gated in
    /// `tests/test_child_vec_scaling.rs`.
    #[inline]
    pub fn add_child_vec_alloc() {
        CHILD_VEC_ALLOCS.fetch_add(1, Ordering::Relaxed);
    }

    /// Zero every counter. Call before the workload you want to measure.
    pub fn reset() {
        COMPLETER_SCAN_STEPS.store(0, Ordering::Relaxed);
        EXPLICIT_PREFIX_COPIES.store(0, Ordering::Relaxed);
        EXPLICIT_NODE_CHILDREN.store(0, Ordering::Relaxed);
        EXPLICIT_ASSEMBLE_CHILDREN.store(0, Ordering::Relaxed);
        EXPLICIT_DERIVATIONS.store(0, Ordering::Relaxed);
        FOREST_NODES.store(0, Ordering::Relaxed);
        CYK_TABLE_STEPS.store(0, Ordering::Relaxed);
        LEXER_SCAN_STEPS.store(0, Ordering::Relaxed);
        DENSE_BUILD_BYTES.store(0, Ordering::Relaxed);
        EXPANSION_ALTS.store(0, Ordering::Relaxed);
        PARSE_TABLE_ACTION_CELLS.store(0, Ordering::Relaxed);
        TREE_NODES_BUILT.store(0, Ordering::Relaxed);
        TOKEN_VALUE_STRING_BYTES.store(0, Ordering::Relaxed);
        LEXER_TOKEN_VALUE_BYTES.store(0, Ordering::Relaxed);
        SEMANTIC_REDUCE_CALLS.store(0, Ordering::Relaxed);
        CHILD_VEC_ALLOCS.store(0, Ordering::Relaxed);
    }

    pub fn completer_scan_steps() -> u64 {
        COMPLETER_SCAN_STEPS.load(Ordering::Relaxed)
    }

    pub fn explicit_prefix_copies() -> u64 {
        EXPLICIT_PREFIX_COPIES.load(Ordering::Relaxed)
    }

    pub fn explicit_node_children() -> u64 {
        EXPLICIT_NODE_CHILDREN.load(Ordering::Relaxed)
    }

    pub fn explicit_assemble_children() -> u64 {
        EXPLICIT_ASSEMBLE_CHILDREN.load(Ordering::Relaxed)
    }

    pub fn explicit_derivations() -> u64 {
        EXPLICIT_DERIVATIONS.load(Ordering::Relaxed)
    }

    pub fn forest_nodes() -> u64 {
        FOREST_NODES.load(Ordering::Relaxed)
    }

    pub fn cyk_table_steps() -> u64 {
        CYK_TABLE_STEPS.load(Ordering::Relaxed)
    }

    pub fn lexer_scan_steps() -> u64 {
        LEXER_SCAN_STEPS.load(Ordering::Relaxed)
    }

    pub fn dense_build_bytes() -> u64 {
        DENSE_BUILD_BYTES.load(Ordering::Relaxed)
    }

    pub fn expansion_alts() -> u64 {
        EXPANSION_ALTS.load(Ordering::Relaxed)
    }

    pub fn parse_table_action_cells() -> u64 {
        PARSE_TABLE_ACTION_CELLS.load(Ordering::Relaxed)
    }

    pub fn tree_nodes_built() -> u64 {
        TREE_NODES_BUILT.load(Ordering::Relaxed)
    }

    pub fn token_value_string_bytes() -> u64 {
        TOKEN_VALUE_STRING_BYTES.load(Ordering::Relaxed)
    }

    pub fn lexer_token_value_bytes() -> u64 {
        LEXER_TOKEN_VALUE_BYTES.load(Ordering::Relaxed)
    }

    pub fn semantic_reduce_calls() -> u64 {
        SEMANTIC_REDUCE_CALLS.load(Ordering::Relaxed)
    }

    pub fn child_vec_allocs() -> u64 {
        CHILD_VEC_ALLOCS.load(Ordering::Relaxed)
    }

    /// Turn the Joop-Leo optimization off (`true`) or on (`false`). Lets a
    /// benchmark/test measure the *same* engine with and without Leo, so the
    /// before/after comparison is apples-to-apples (the "prove it was super-linear
    /// without the fix" half of #58). Production never touches this — the toggle
    /// only exists under `perf-counters`.
    pub fn set_leo_disabled(disabled: bool) {
        LEO_DISABLED.store(disabled, Ordering::Relaxed);
    }

    #[inline]
    pub fn leo_disabled() -> bool {
        LEO_DISABLED.load(Ordering::Relaxed)
    }

    /// Whether the counters are live (the `perf-counters` feature is enabled).
    pub const ENABLED: bool = true;
}

#[cfg(not(feature = "perf-counters"))]
mod imp {
    #[inline]
    pub fn add_completer_scan_steps(_n: u64) {}

    #[inline]
    pub fn add_explicit_prefix_copies(_n: u64) {}

    #[inline]
    pub fn add_explicit_node_children(_n: u64) {}

    #[inline]
    pub fn add_explicit_assemble_children(_n: u64) {}

    #[inline]
    pub fn add_explicit_derivation() {}

    #[inline]
    pub fn add_forest_node() {}

    #[inline]
    pub fn add_cyk_table_steps(_n: u64) {}

    #[inline]
    pub fn add_lexer_scan_steps(_n: u64) {}

    #[inline]
    pub fn add_dense_build_bytes(_n: u64) {}

    #[inline]
    pub fn add_expansion_alts(_n: u64) {}

    #[inline]
    pub fn add_parse_table_action_cells(_n: u64) {}

    #[inline]
    pub fn add_tree_node_built() {}

    #[inline]
    pub fn add_token_value_string_bytes(_n: u64) {}

    #[inline]
    pub fn add_lexer_token_value_bytes(_n: u64) {}

    #[inline]
    pub fn add_semantic_reduce_call() {}

    #[inline]
    pub fn add_child_vec_alloc() {}

    pub fn reset() {}

    pub fn completer_scan_steps() -> u64 {
        0
    }

    pub fn explicit_prefix_copies() -> u64 {
        0
    }

    pub fn explicit_node_children() -> u64 {
        0
    }

    pub fn explicit_assemble_children() -> u64 {
        0
    }

    pub fn explicit_derivations() -> u64 {
        0
    }

    pub fn forest_nodes() -> u64 {
        0
    }

    pub fn cyk_table_steps() -> u64 {
        0
    }

    pub fn lexer_scan_steps() -> u64 {
        0
    }

    pub fn dense_build_bytes() -> u64 {
        0
    }

    pub fn expansion_alts() -> u64 {
        0
    }

    pub fn parse_table_action_cells() -> u64 {
        0
    }

    pub fn tree_nodes_built() -> u64 {
        0
    }

    pub fn token_value_string_bytes() -> u64 {
        0
    }

    pub fn lexer_token_value_bytes() -> u64 {
        0
    }

    pub fn semantic_reduce_calls() -> u64 {
        0
    }

    pub fn child_vec_allocs() -> u64 {
        0
    }

    pub fn set_leo_disabled(_disabled: bool) {}

    #[inline]
    pub fn leo_disabled() -> bool {
        false
    }

    pub const ENABLED: bool = false;
}

pub use imp::*;
