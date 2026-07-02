//! Deterministic **child-buffer reuse** gate (semantic-output C8.2, #583).
//!
//! Originally the honest close-out of #233's *"bounded child-buffer reuse"* line:
//! the counter proved the *bounded* half (one fresh buffer per reduction, never
//! super-linear) while documenting that nothing was reused. The reuse half landed
//! in the 2026-07-02 perf spike (`docs/notes/perf-spike-2026-07-02.md`):
//! `shape_reduction` now recycles its buffers — stealing a transparent `Inline`
//! splice wholesale, banking the drained node buffer in a per-parse
//! `ReduceScratch`, and skipping the buffer entirely for arity-1 `expand1`
//! collapses — so [`lark_rs::perf::child_vec_allocs`] charges only **fresh**
//! materializations (a per-buffer unit, not a raw allocator count — see the
//! `perf.rs` doc). This gate pins the reuse state deterministically (ADR-0007: a
//! work counter, never wall-clock — BENCH.md):
//!
//! 1. **Exactly one fresh buffer per parse** on the list grammar: it has no
//!    transparent rule, so no buffer ever leaves onto the value stack — after the
//!    first materialization every reduction recycles the scratch. Rising above 1
//!    means the recycling was lost (back toward one-per-reduction, or worse,
//!    per-child).
//! 2. **Flat (O(1)) across a size sweep** — strictly stronger than the old
//!    flat-per-node envelope — while `semantic_reduce_calls` keeps the grammar's
//!    `2n+1` closed form, proving the reductions themselves still happen.
//!
//! (History: before the reuse pass this gate asserted `child_vec_allocs == 2n+1
//! == semantic_reduce_calls` — the bounded-but-not-reused ratio-1 state — and its
//! doc reserved exactly this transition for the #242/#243 reuse work.)
//!
//! Like every other scaling gate the counter only exists under
//! `--features perf-counters` (zero overhead otherwise), so `cargo test --all` runs
//! the trivial placeholder and CI runs the real gate with:
//!
//! ```bash
//! cargo test --features perf-counters --test test_child_vec_scaling
//! ```

#[cfg(feature = "perf-counters")]
use lark_rs::{perf, Lark, LarkOptions, LexerType, ParserAlgorithm};

/// A list grammar with a **known, closed-form reduction count** — the same shape
/// `test_output_counters.rs` uses. For an input of `n` items (`"a a a …"`, `n ≥ 1`)
/// under LALR the user-rule reductions are:
///
/// * `item: "a"`        → `n`;
/// * `list: item`       → `1`;
/// * `list: list item`  → `n - 1`;
/// * `start: list`      → `1`.
///
/// Total = `2n + 1`. Each reduction routes through `shape_reduction` (the
/// value-parametric `parse_into`/`parse()` path); with the scratch recycling only
/// the *first* materializes a fresh child buffer, so `child_vec_allocs == 1` while
/// `semantic_reduce_calls == 2n + 1`. The augmented `$root_start → start`
/// accept does **not** route through `shape_reduction`, so it is not counted.
#[cfg(feature = "perf-counters")]
const LIST_GRAMMAR: &str = r#"
start: list
list: list item | item
item: ITEM
ITEM: "a"
%ignore " "
"#;

/// Parse `n` space-separated `a` items on the default (tree) `parse()` path — which
/// drives the value-parametric `run_into` + `shape_reduction` seam — and return
/// `(child_vec_allocs, semantic_reduce_calls)` for that single parse.
#[cfg(feature = "perf-counters")]
fn parse_items(parser: &Lark, n: usize) -> (u64, u64) {
    assert!(n >= 1, "grammar needs at least one item");
    let input = vec!["a"; n].join(" ");
    perf::reset();
    parser
        .parse(&input)
        .unwrap_or_else(|e| panic!("list parse of {n} items must succeed: {e}"));
    (perf::child_vec_allocs(), perf::semantic_reduce_calls())
}

/// The whole net is ONE test: the `perf` counters are process-global atomics, so a
/// second `#[test]` racing in parallel would corrupt the reads (same rationale as
/// the Earley/CYK/lexer/output-shape scaling gates).
#[cfg(feature = "perf-counters")]
#[test]
fn child_vec_allocs_are_bounded() {
    assert!(
        perf::ENABLED,
        "test built with the perf-counters feature but counters report disabled"
    );

    let parser = Lark::new(
        LIST_GRAMMAR,
        LarkOptions {
            parser: ParserAlgorithm::Lalr,
            lexer: LexerType::Contextual,
            start: vec!["start".to_string()],
            ..Default::default()
        },
    )
    .expect("list grammar must build under LALR");

    // ── The counter is actually wired. ────────────────────────────────────────
    let (allocs, reduces) = parse_items(&parser, 4);
    eprintln!("n=4: child_vec_allocs={allocs}, semantic_reduce_calls={reduces}");
    assert!(
        allocs > 0,
        "child_vec_allocs recorded zero — the counter is not wired into \
         shape_reduction (or this input never reduces)"
    );

    // ── Assertion 1: reuse ratio < 1 — the #242/#243 reuse state. ──────────────
    // Since the child-buffer reuse pass (perf spike 2026-07-02: `ReduceScratch`
    // recycling + `Inline` steal + `expand1` fast path), `child_vec_allocs` charges
    // only *fresh* buffer allocations (scratch/steal recycles charge nothing).
    // LIST_GRAMMAR has no transparent rule, so no buffer ever leaves onto the value
    // stack: after the first reduction materializes the one scratch buffer, every
    // later reduction recycles it — the closed form is exactly **1 fresh buffer per
    // parse**, independent of n. Rising above it means the recycling was lost (a
    // regression back toward one-buffer-per-reduction, or worse, per-child).
    assert_eq!(
        allocs, 1,
        "child_vec_allocs must be exactly 1 on the list grammar (the single fresh \
         scratch materialization; every later reduction recycles it — the \
         #242/#243 reuse state). allocs>1 means recycling regressed; \
         semantic_reduce_calls={reduces} for scale"
    );

    // ── Assertion 2: flat (O(1)) across the size sweep, no per-node/per-child
    // blowup. The reuse makes fresh-buffer work *constant* in output shape here,
    // strictly stronger than the old flat-per-node envelope (BENCH.md).
    let sweep = [1usize, 2, 4, 8, 16, 32];
    let mut rows: Vec<(usize, u64, u64)> = Vec::new();
    for &n in &sweep {
        let (allocs, reduces) = parse_items(&parser, n);
        rows.push((n, allocs, reduces));
    }
    eprintln!("child-vec sweep (n, allocs, reduces) = {rows:?}");

    for &(n, allocs, reduces) in &rows {
        assert_eq!(
            allocs, 1,
            "fresh child-buffer count must stay exactly 1 across the sweep \
             (n={n}, reduces={reduces}); growth means the scratch recycling was lost"
        );
        assert_eq!(
            reduces,
            (2 * n + 1) as u64,
            "the reduction count itself must keep the 2n+1 closed form (n={n})"
        );
    }

    // ── Assertion 3: the trailing-placeholder path is materialization-visible. ──
    // `maybe_placeholders` inserts a `None` per absent optional; `item: "(" [A] ")"`
    // with the optional absent makes every `item` reduction's kept children *only*
    // placeholders (the `"("`/`")"` punctuation is filtered). That exercises the
    // trailing-placeholder push in `shape_reduction`, which used to grow the child
    // buffer *without* routing through the recycle/counter site — a placeholder-first
    // reduction allocated silently, so `child_vec_allocs` under-counted (the review
    // finding). The counter must now see it (> 0), and the scratch recycling must
    // keep the fresh-buffer count *bounded* across the sweep, not one-per-item.
    let placeholder = Lark::new(
        PLACEHOLDER_GRAMMAR,
        LarkOptions {
            parser: ParserAlgorithm::Lalr,
            lexer: LexerType::Contextual,
            start: vec!["start".to_string()],
            maybe_placeholders: true,
            ..Default::default()
        },
    )
    .expect("placeholder grammar must build under LALR");
    let parse_ph = |n: usize| -> (u64, u64) {
        let input = vec!["()"; n].join(" ");
        perf::reset();
        placeholder
            .parse(&input)
            .expect("placeholder parse must succeed");
        (perf::child_vec_allocs(), perf::semantic_reduce_calls())
    };
    let mut ph_rows = Vec::new();
    for &n in &[1usize, 2, 4, 8, 16, 32] {
        ph_rows.push((n, parse_ph(n)));
    }
    eprintln!("placeholder sweep (n, (allocs, reduces)) = {ph_rows:?}");
    let (allocs1, reduces1) = ph_rows[0].1;
    assert!(
        allocs1 > 0,
        "a placeholder-producing reduction must charge child_vec_allocs; 0 means the \
         trailing-placeholder push bypassed the materialize/recycle site"
    );
    let (max_allocs, reduces_big) = ph_rows.last().unwrap().1;
    assert!(
        max_allocs <= allocs1 + 2,
        "fresh child-buffer count must stay bounded across the placeholder sweep \
         (scratch reuse): {max_allocs} at n=32 vs {allocs1} at n=1 — a per-item climb \
         means the placeholder path stopped recycling"
    );
    assert!(
        reduces_big > reduces1,
        "reduction count must grow with item count (sanity: the sweep parses more)"
    );
}

/// A **placeholder-producing** grammar: `maybe_placeholders` inserts a `None` for
/// each absent optional, and every `item` is `"(" [A] ")"` with the optional
/// absent — so each `item` reduction's kept children are *only* placeholders (the
/// `"("`/`")"` punctuation is filtered), exercising the trailing-placeholder
/// materialization path in `shape_reduction`.
#[cfg(feature = "perf-counters")]
const PLACEHOLDER_GRAMMAR: &str = r#"
start: item+
item: "(" [A] ")"
A: "a"
%ignore " "
"#;

/// Without the `perf-counters` feature the counter is a no-op, so the gate cannot
/// run. Keep a visible placeholder documenting how to run it (mirrors the other
/// scaling gates), so `cargo test --all` stays fast and the file is never silently
/// empty.
#[cfg(not(feature = "perf-counters"))]
#[test]
fn child_vec_scaling_requires_perf_counters_feature() {
    assert!(!lark_rs::perf::ENABLED);
}
