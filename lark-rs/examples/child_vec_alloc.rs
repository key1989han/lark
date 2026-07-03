//! Isolates the **per-node child-`Vec` allocation cost** — the one number the
//! 2026-07 parser-optimization literature study could not source externally
//! (`docs/notes/parser-optimization-research-2026-07.md`, Part 3 / finding M5:
//! "no external source isolates the per-node child-Vec cost — lark-rs can measure
//! it itself via SpanTree vs TapeTree").
//!
//! ## Why SpanTree − TapeTree isolates it
//!
//! `parse_span` (`SpanTree`) and `parse_tape` (`TapeTree`) are the *same* zero-copy
//! parse in every respect except the child-list representation:
//!
//! * both build **no** owned `Tree` (`tree_nodes_built == 0`),
//! * both copy **no** token value or label (borrowed from `input`/grammar),
//! * both run the identical LALR engine + lexer over the identical bytes,
//! * **the only structural difference:** a `SpanBranch` still owns one
//!   `children: Vec<SpanNode>` **per internal node** (`src/parsers/span_tree.rs`),
//!   whereas `TapeTree` appends every node's children to **one shared flat `kids[]`
//!   arena** grown amortized (`src/parsers/tape.rs`, O(log n) allocator calls).
//!
//! Every other allocation (parser stacks, the token `Vec`, lazy scanners) is shared
//! byte-for-byte, so with a counting global allocator the difference
//!
//! ```text
//!     child_vec_cost  =  allocs(parse_span)  −  allocs(parse_tape)
//! ```
//!
//! is exactly the per-node child-`Vec` traffic that finding M5 flagged and that the
//! flattening literature (Sampson, *Flattening ASTs*) predicts a flat `kids[]` arena
//! removes. Dividing by the internal-node count (each owned `Tree` node carries one
//! `children` `Vec`, so `parse()`'s `Tree`-node count is the denominator) gives the
//! **allocations charged per internal node** — expected ≈ 1.0 and, crucially, *flat*
//! as the input scales (one `Vec` per node, independent of size).
//!
//! ```text
//! cargo run --release --features "span-tree,tape-tree,perf-counters" \
//!   --example child_vec_alloc [records]
//! ```
//!
//! `records` (optional, default 2000) scales the JSON input so you can confirm the
//! per-node cost is flat while total child-`Vec` traffic grows with the node count.
//! This is a **recorded measurement, not a gate** (ADR-0007 keeps wall-clock/real-
//! alloc numbers a trend); the deterministic child-buffer *gate* is
//! `tests/test_child_vec_scaling.rs` on the `parse_into` path.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};

/// Pass-through allocator that counts allocations (and grow-reallocs) so a parse's
/// real heap traffic is measurable. Deterministic for a single-threaded parse. Only
/// this example binary installs it — an example's `#[global_allocator]` does not
/// affect the library, tests, or other binaries.
struct Counting;

static ALLOC_COUNT: AtomicU64 = AtomicU64::new(0);
static ALLOC_BYTES: AtomicU64 = AtomicU64::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
        ALLOC_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        System.alloc(layout)
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout)
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // A grow-realloc is a fresh allocation of the delta.
        if new_size > layout.size() {
            ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
            ALLOC_BYTES.fetch_add((new_size - layout.size()) as u64, Ordering::Relaxed);
        }
        System.realloc(ptr, layout, new_size)
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

#[cfg(all(
    feature = "span-tree",
    feature = "tape-tree",
    feature = "perf-counters"
))]
mod demo {
    use super::{ALLOC_BYTES, ALLOC_COUNT};
    use lark_rs::tree::{Child, Tree};
    use lark_rs::{Lark, LarkOptions, LexerType, ParserAlgorithm};
    use std::sync::atomic::Ordering;
    use std::time::Instant;

    const JSON_GRAMMAR: &str = r#"
        ?start: value
        ?value: object
              | array
              | string
              | SIGNED_NUMBER  -> number
              | "true"         -> true
              | "false"        -> false
              | "null"         -> null
        array  : "[" [value ("," value)*] "]"
        object : "{" [pair ("," pair)*] "}"
        pair   : string ":" value
        string : ESCAPED_STRING
        %import common.ESCAPED_STRING
        %import common.SIGNED_NUMBER
        %import common.WS
        %ignore WS
    "#;

    /// Read-and-zero the allocation counters (isolates one parse's heap traffic).
    fn snapshot() -> (u64, u64) {
        (
            ALLOC_COUNT.swap(0, Ordering::Relaxed),
            ALLOC_BYTES.swap(0, Ordering::Relaxed),
        )
    }

    fn gen_json(records: usize, fields: usize) -> String {
        let mut s = String::from("[");
        for r in 0..records {
            if r > 0 {
                s.push(',');
            }
            s.push('{');
            for f in 0..fields {
                if f > 0 {
                    s.push(',');
                }
                s.push_str(&format!(
                    "\"key{f}\": {}, \"name{f}\": \"value{r}_{f}\"",
                    r * 10 + f
                ));
            }
            s.push('}');
        }
        s.push(']');
        s
    }

    /// Count `Tree` (internal / branch) nodes in an owned parse tree. Each `Tree`
    /// carries exactly one `children: Vec<Child>`, so this is the number of child
    /// `Vec`s the `SpanTree` path allocates and the denominator for the per-node
    /// cost. Tokens carry no child `Vec` and are not counted. Iterative to avoid
    /// recursion on deep trees.
    fn count_internal_nodes(root: &Tree) -> u64 {
        let mut n: u64 = 0;
        let mut stack: Vec<&Tree> = vec![root];
        while let Some(t) = stack.pop() {
            n += 1;
            for c in &t.children {
                if let Child::Tree(sub) = c {
                    stack.push(sub);
                }
            }
        }
        n
    }

    fn median(mut v: Vec<u128>) -> u128 {
        v.sort_unstable();
        v[v.len() / 2]
    }

    pub fn run(records: usize) {
        let parser = Lark::new(
            JSON_GRAMMAR,
            LarkOptions {
                parser: ParserAlgorithm::Lalr,
                lexer: LexerType::Contextual,
                ..Default::default()
            },
        )
        .expect("json grammar builds");

        let input = gen_json(records, 8);
        let nbytes = input.len();

        // Internal-node count (the denominator) — computed off the measured path.
        // The `?start: value` root collapses to the top `value` node; count from
        // whichever `Tree` the parse rooted at (a bare Token/None root has 0).
        let owned_tree = parser.parse(&input).unwrap();
        let internal_nodes = owned_tree.as_tree().map(count_internal_nodes).unwrap_or(0);
        drop(owned_tree);
        println!(
            "input: {nbytes} bytes ({records} records × 8 fields), {internal_nodes} internal nodes\n"
        );

        // Warm up caches / lazy scanners (not measured).
        for _ in 0..3 {
            let _ = parser.parse(&input).unwrap();
            let _ = parser.parse_span(&input).unwrap();
            let _ = parser.parse_tape(&input).unwrap();
        }

        // ── Real allocations: reset right before ONE parse, read after it returns
        //    (the result is still alive, so its allocations are counted; freeing is
        //    dealloc, which we don't count). Deterministic. ─────────────────────────
        lark_rs::perf::reset();
        let _ = snapshot();
        let owned = parser.parse(&input).unwrap();
        let (owned_allocs, owned_bytes) = snapshot();
        std::hint::black_box(&owned);
        drop(owned);

        lark_rs::perf::reset();
        let _ = snapshot();
        let span = parser.parse_span(&input).unwrap();
        let (span_allocs, span_bytes) = snapshot();
        let span_nodes = lark_rs::perf::tree_nodes_built();
        let span_out_bytes = lark_rs::perf::token_value_string_bytes();
        std::hint::black_box(&span);
        drop(span);

        lark_rs::perf::reset();
        let _ = snapshot();
        let tape = parser.parse_tape(&input).unwrap();
        let (tape_allocs, tape_bytes) = snapshot();
        let tape_nodes = lark_rs::perf::tree_nodes_built();
        let tape_out_bytes = lark_rs::perf::token_value_string_bytes();
        std::hint::black_box(&tape);
        drop(tape);

        // ── The isolation: span and tape differ ONLY in the child-list rep. ───────
        let child_vec_cost = span_allocs.saturating_sub(tape_allocs);

        let per_byte = |n: u64| n as f64 / nbytes as f64;
        let per_node = |n: u64| n as f64 / internal_nodes as f64;

        println!("── Real heap allocations (counting allocator, one parse) ──");
        println!(
            "{:<14}{:>12}{:>14}{:>16}",
            "path", "allocs", "allocs/byte", "bytes"
        );
        for (name, a, b) in [
            ("parse()", owned_allocs, owned_bytes),
            ("parse_span()", span_allocs, span_bytes),
            ("parse_tape()", tape_allocs, tape_bytes),
        ] {
            println!("{:<14}{:>12}{:>14.3}{:>16}", name, a, per_byte(a), b);
        }
        println!();

        println!("── Isolated per-node child-Vec cost (span − tape) ──");
        println!("  span still owns one child Vec per internal node; tape uses a flat kids[].");
        println!("  Both are otherwise byte-identical zero-copy parses, so the delta is");
        println!("  exactly the per-node child-Vec allocation traffic (finding M5).\n");
        println!("  child-Vec allocations total : {child_vec_cost}");
        println!(
            "  per internal node           : {:.3}   ({internal_nodes} nodes)",
            per_node(child_vec_cost)
        );
        println!(
            "  as allocs/byte              : {:.3}   (of parse_span's {:.3})",
            per_byte(child_vec_cost),
            per_byte(span_allocs)
        );
        if span_allocs > 0 {
            println!(
                "  share of parse_span allocs  : {:.1}%",
                100.0 * child_vec_cost as f64 / span_allocs as f64
            );
        }
        println!(
            "  → a flat kids[] arena (tape) removes {:.1}% of parse_span's allocations",
            100.0 * (span_allocs.saturating_sub(tape_allocs)) as f64 / span_allocs.max(1) as f64
        );
        println!();

        // ── Rigor check: both paths are genuinely the same zero-copy parse. ───────
        println!("── Zero-copy invariant (both paths, the reason the delta is clean) ──");
        println!(
            "  tree_nodes_built  span={span_nodes} tape={tape_nodes}  (both 0 ⇒ no owned Tree)"
        );
        println!(
            "  output_tok_bytes  span={span_out_bytes} tape={tape_out_bytes}  (both 0 ⇒ no copied values)"
        );
        if span_nodes != 0 || tape_nodes != 0 || span_out_bytes != 0 || tape_out_bytes != 0 {
            println!(
                "  ⚠ a counter is non-zero — the span/tape paths are NOT both pure zero-copy;\n    \
                 the delta above is no longer a clean child-Vec isolation. Investigate."
            );
        }
        println!();

        // ── Wall-clock (trend only, noisy — ADR-0007). ───────────────────────────
        let iters = 200;
        let time = |f: &dyn Fn()| {
            let mut ts = Vec::with_capacity(iters);
            for _ in 0..iters {
                let t = Instant::now();
                f();
                ts.push(t.elapsed().as_nanos());
            }
            median(ts)
        };
        let span_med = time(&|| {
            std::hint::black_box(parser.parse_span(&input).unwrap());
        });
        let tape_med = time(&|| {
            std::hint::black_box(parser.parse_tape(&input).unwrap());
        });
        let mbps = |ns: u128| nbytes as f64 / (ns as f64 / 1e3);
        println!("── Wall-clock median over {iters} iters (trend, noisy) ──");
        println!(
            "parse_span() {:>8.3} ms  {:>6.2} MB/s",
            span_med as f64 / 1e6,
            mbps(span_med)
        );
        println!(
            "parse_tape() {:>8.3} ms  {:>6.2} MB/s",
            tape_med as f64 / 1e6,
            mbps(tape_med)
        );
        println!(
            "→ tape is {:.2}× the speed of span (child-Vec removal, wall-clock trend)",
            span_med as f64 / tape_med as f64
        );
    }
}

#[cfg(all(
    feature = "span-tree",
    feature = "tape-tree",
    feature = "perf-counters"
))]
fn main() {
    let records = std::env::args()
        .nth(1)
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(2000);
    demo::run(records);
}

#[cfg(not(all(
    feature = "span-tree",
    feature = "tape-tree",
    feature = "perf-counters"
)))]
fn main() {
    eprintln!(
        "child_vec_alloc needs `span-tree` (parse_span), `tape-tree` (parse_tape), and \
         `perf-counters` (the allocation-counter + zero-copy readout):\n\n    cargo run \
         --release --features \"span-tree,tape-tree,perf-counters\" --example child_vec_alloc \
         [records]"
    );
}
