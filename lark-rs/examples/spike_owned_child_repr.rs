//! SPIKE (throwaway, `docs/notes/parser-optimization-research-2026-07.md`
//! experiments 2 + 3). Not wired into any feature flag and not touching the
//! engine at all: every variant below is a plain [`lark_rs::OutputBuilder`]
//! driven through the existing public [`Lark::parse_into`] seam (#232, C7) — the
//! *value-parametric* API `parse_span`/`parse_tape` already use internally. No
//! src/ file changes were needed for this spike; delete this example once the
//! finding is written up.
//!
//! ## Hypotheses under test
//!
//! `examples/child_vec_alloc.rs` already isolated the per-node child-`Vec` cost
//! for the **zero-copy** backends (`SpanTree` vs `TapeTree`: both borrow labels
//! and token values, so the only difference is the child-list representation —
//! exactly 1.000 allocs/internal-node, ~96% of `parse_span`'s remaining
//! allocations). Two questions that spike left open:
//!
//! 1. **(experiment 3, M2)** How much does interning `Tree::data: String` to a
//!    `Copy` `u32` save on the **default, fully-owned** tree — where (unlike
//!    `SpanTree`) the label is a fresh heap `String` per node today
//!    (`ctx.callback_name(rule).to_string()`)?
//! 2. **(experiment 2, M5)** Does the flat-`kids[]`-arena win generalize from the
//!    zero-copy tree to the **owned** tree (owned `Token` values, as `parse()`
//!    produces)? And does a `SmallVec`-inline child list (unmeasured in the
//!    child_vec_alloc spike) capture part of that win more cheaply?
//!
//! ## The ladder
//!
//! Every variant keeps owned `Token` values (so nothing here re-measures
//! span-borrowing, M6 — that is `SpanTree`'s question, already answered) and
//! shares the identical LALR engine + tree-shaping logic (`shape_reduction`);
//! only the `OutputBuilder::Value` type changes. `A0`/`A1` are a methodology
//! sanity check (a custom builder reproducing `parse()`'s own shape); `B` isolates
//! label interning alone; `D`/`E` isolate the child-list change *on top of* `B`
//! (so the ladder never conflates two axes in one step):
//!
//! * **A0** — `parser.parse()`, the real default (`String` label, `Vec<Child>`
//!   built via `std::mem::take` — i.e. *stealing* the engine's already-populated
//!   scratch buffer as the permanent `Tree.children`, no copy).
//! * **A1** — a custom builder reproducing A0's representation exactly
//!   (`ctx.callback_name(rule).to_string()` + steal). Should match A0's per-node
//!   allocation count; if it doesn't, the harness itself is suspect.
//! * **B** — swap the label for `rule: u32` (interned), keep the steal. Isolates
//!   M2 alone.
//! * **D** — B's `u32` label, but the child list is `SmallVec<[V; 4]>` built by
//!   `children.drain(..).collect()` — **not** `SmallVec::from_vec` (which would
//!   keep an existing heap `Vec` heap-allocated forever; only building fresh from
//!   an iterator gets the inline optimization for ≤4 children). Isolates a
//!   partial M5 (small-arity nodes skip the heap; wide `array`/`object` nodes
//!   still spill).
//! * **E** — B's `u32` label, full M5: every node's children live in one shared
//!   `kids: Vec<u32>` (exactly `TapeTree`'s structure, but with **owned** `Token`
//!   leaves instead of borrowed spans) — no per-node child container at all.
//!
//! **Why D/E must not steal.** `shape_reduction` recycles its per-parse
//! `ReduceScratch::values` buffer *only* if the builder leaves it in place
//! (`values.clear(); scratch.values = values;` runs unconditionally after
//! `reduce()` returns) — `std::mem::take`ing it (A1/B's steal) hands the buffer to
//! the tree permanently, forcing a fresh allocation next reduction. D copies via
//! `drain().collect()`; E copies via `extend_from_slice` — both leave the
//! original `Vec` behind with its capacity intact, so the *scratch* buffer stops
//! costing anything after an initial warmup, and only the *permanent* storage
//! (SmallVec-inline-or-spill for D, one shared growing array for E) is now the
//! interesting cost.
//!
//! ## Method
//!
//! The same counting-global-allocator technique as `child_vec_alloc.rs`, one
//! parse per variant, reset-just-before/read-just-after. Each builder also counts
//! the nodes it actually materializes (`reduce()` calls — the engine's `expand1`
//! fast path never reaches a builder, so this is directly comparable across
//! variants and cross-checked against `parse()`'s real internal-node count).
//!
//! ```text
//! cargo run --release --example spike_owned_child_repr [records]
//! ```

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};

struct Counting;
static ALLOC_COUNT: AtomicU64 = AtomicU64::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
        System.alloc(layout)
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout)
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if new_size > layout.size() {
            ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
        }
        System.realloc(ptr, layout, new_size)
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn snapshot() -> u64 {
    ALLOC_COUNT.swap(0, Ordering::Relaxed)
}

use lark_rs::tree::{Child, Tree};
use lark_rs::{
    Lark, LarkOptions, LexerType, Meta, OutputBuilder, OutputContext, ParserAlgorithm, Token,
};
use smallvec::SmallVec;
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

/// Count `Tree` nodes in the real owned parse tree — the denominator every
/// variant's own `nodes` counter is cross-checked against. Iterative (no
/// recursion) as `child_vec_alloc.rs` does.
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

// ─── Variant A1: String label (ctx.callback_name().to_string()), Vec-steal ────
// Reproduces parser.parse()'s own representation exactly — the methodology
// sanity check.

#[allow(dead_code)]
enum SChild {
    Tree(STree),
    Token(Token),
    None,
}
struct STree {
    #[allow(dead_code)]
    label: String,
    #[allow(dead_code)]
    children: Vec<SChild>,
}

#[derive(Default)]
struct StringStealBuilder {
    nodes: u64,
}
impl<'i> OutputBuilder<'i> for StringStealBuilder {
    type Value = SChild;
    fn token(&mut self, token: Token, _input: &'i str, _ctx: &OutputContext) -> SChild {
        SChild::Token(token)
    }
    fn reduce(
        &mut self,
        rule: usize,
        children: &mut Vec<SChild>,
        _meta: &Meta,
        ctx: &OutputContext,
    ) -> SChild {
        self.nodes += 1;
        let label = ctx.callback_name(rule).to_string();
        let children = std::mem::take(children);
        SChild::Tree(STree { label, children })
    }
    fn placeholder(&mut self, _ctx: &OutputContext) -> SChild {
        SChild::None
    }
}

// ─── Variant B: u32 label (interned), Vec-steal — isolates M2 alone ───────────

#[allow(dead_code)]
enum UChild {
    Tree(UTree),
    Token(Token),
    None,
}
struct UTree {
    #[allow(dead_code)]
    rule: u32,
    #[allow(dead_code)]
    children: Vec<UChild>,
}

#[derive(Default)]
struct U32StealBuilder {
    nodes: u64,
}
impl<'i> OutputBuilder<'i> for U32StealBuilder {
    type Value = UChild;
    fn token(&mut self, token: Token, _input: &'i str, _ctx: &OutputContext) -> UChild {
        UChild::Token(token)
    }
    fn reduce(
        &mut self,
        rule: usize,
        children: &mut Vec<UChild>,
        _meta: &Meta,
        _ctx: &OutputContext,
    ) -> UChild {
        self.nodes += 1;
        let children = std::mem::take(children);
        UChild::Tree(UTree {
            rule: rule as u32,
            children,
        })
    }
    fn placeholder(&mut self, _ctx: &OutputContext) -> UChild {
        UChild::None
    }
}

// ─── Variant D: u32 label + SmallVec<[_; 4]> via drain-collect (no steal) ─────
// Partial M5: nodes with <=4 children (string, pair, ...) never touch the heap
// for their child list; wide array/object nodes still spill to a heap Vec.

#[allow(dead_code)]
enum VChild {
    // Boxed to break the size cycle: `SmallVec<[VChild; 4]>` inlines its elements
    // directly (that's the whole point), so `VChild`'s own size can't depend on
    // itself through an unboxed `VTree`.
    Tree(Box<VTree>),
    Token(Token),
    None,
}
#[allow(dead_code)]
struct VTree {
    #[allow(dead_code)]
    rule: u32,
    #[allow(dead_code)]
    children: SmallVec<[VChild; 4]>,
}

#[derive(Default)]
struct SmallVecBuilder {
    nodes: u64,
}
impl<'i> OutputBuilder<'i> for SmallVecBuilder {
    type Value = VChild;
    fn token(&mut self, token: Token, _input: &'i str, _ctx: &OutputContext) -> VChild {
        VChild::Token(token)
    }
    fn reduce(
        &mut self,
        rule: usize,
        children: &mut Vec<VChild>,
        _meta: &Meta,
        _ctx: &OutputContext,
    ) -> VChild {
        self.nodes += 1;
        // NOT `SmallVec::from_vec(std::mem::take(children))` — that keeps the
        // existing heap Vec heap-allocated forever. Building fresh via
        // drain().collect() is the only way small counts land inline, and it
        // leaves the engine's scratch Vec (`children`) with its capacity intact
        // for `shape_reduction` to recycle on the next reduction (see module docs).
        let kids: SmallVec<[VChild; 4]> = children.drain(..).collect();
        // Boxing here is the price of breaking the recursive-size cycle (see the
        // `VChild` doc comment) — it means this variant pays a Box allocation per
        // node *in addition to* the SmallVec (inline-or-spilled). Measuring that
        // honestly, not assuming it away, is the point of this data point.
        VChild::Tree(Box::new(VTree {
            rule: rule as u32,
            children: kids,
        }))
    }
    fn placeholder(&mut self, _ctx: &OutputContext) -> VChild {
        VChild::None
    }
}

// ─── Variant E: u32 label + flat kids[] arena (no steal) — full M5 ────────────
// Exactly TapeTree's structure (one shared growing `kids: Vec<u32>`, node =
// range into it), but with OWNED `Token` leaves instead of borrowed spans — the
// "what if the DEFAULT owned tree used TapeTree's child-list rep" experiment.

#[allow(dead_code)]
enum FlatEntry {
    Token(Token),
    Node {
        rule: u32,
        kids_start: u32,
        kids_len: u32,
    },
    None,
}

#[derive(Default)]
struct FlatBuilder {
    entries: Vec<FlatEntry>,
    kids: Vec<u32>,
    nodes: u64,
}
impl<'i> OutputBuilder<'i> for FlatBuilder {
    type Value = u32;
    fn token(&mut self, token: Token, _input: &'i str, _ctx: &OutputContext) -> u32 {
        let idx = self.entries.len() as u32;
        self.entries.push(FlatEntry::Token(token));
        idx
    }
    fn reduce(
        &mut self,
        rule: usize,
        children: &mut Vec<u32>,
        _meta: &Meta,
        _ctx: &OutputContext,
    ) -> u32 {
        self.nodes += 1;
        let kids_start = self.kids.len() as u32;
        // Copies (doesn't drain/take) `children` into the shared arena — the
        // engine's scratch Vec is untouched (still holds its elements until the
        // caller's own `values.clear()` right after this call), so its capacity
        // survives for the next reduction, same recycling argument as variant D.
        self.kids.extend_from_slice(children);
        let idx = self.entries.len() as u32;
        self.entries.push(FlatEntry::Node {
            rule: rule as u32,
            kids_start,
            kids_len: children.len() as u32,
        });
        idx
    }
    fn placeholder(&mut self, _ctx: &OutputContext) -> u32 {
        let idx = self.entries.len() as u32;
        self.entries.push(FlatEntry::None);
        idx
    }
}

fn median(mut v: Vec<u128>) -> u128 {
    v.sort_unstable();
    v[v.len() / 2]
}

fn main() {
    let records: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(2000);

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

    let owned = parser.parse(&input).unwrap();
    let internal_nodes = owned.as_tree().map(count_internal_nodes).unwrap_or(0);
    drop(owned);
    println!(
        "input: {nbytes} bytes ({records} records x 8 fields), {internal_nodes} internal nodes\n"
    );

    // Warm up lazy scanner OnceCells etc. — not measured.
    for _ in 0..3 {
        let _ = parser.parse(&input).unwrap();
        let _ = parser.parse_into(&input, &mut StringStealBuilder::default());
        let _ = parser.parse_into(&input, &mut U32StealBuilder::default());
        let _ = parser.parse_into(&input, &mut SmallVecBuilder::default());
        let _ = parser.parse_into(&input, &mut FlatBuilder::default());
    }

    println!(
        "{:<28}{:>10}{:>12}{:>14}{:>14}",
        "variant", "allocs", "nodes", "allocs/node", "allocs/byte"
    );

    fn print_row(name: &str, allocs: u64, nodes: u64, nbytes: usize) {
        println!(
            "{:<28}{:>10}{:>12}{:>14.3}{:>14.4}",
            name,
            allocs,
            nodes,
            allocs as f64 / nodes.max(1) as f64,
            allocs as f64 / nbytes as f64
        );
    }

    let _ = snapshot();
    let r = parser.parse(&input).unwrap();
    let a0 = snapshot();
    std::hint::black_box(&r);
    drop(r);
    print_row("A0 parse() [default]", a0, internal_nodes, nbytes);

    let mut b1 = StringStealBuilder::default();
    let _ = snapshot();
    let r = parser.parse_into(&input, &mut b1).unwrap();
    let a1 = snapshot();
    std::hint::black_box(&r);
    drop(r);
    print_row("A1 String label, Vec-steal", a1, b1.nodes, nbytes);
    assert_eq!(
        b1.nodes, internal_nodes,
        "A1's node count must match parse()'s real internal-node count \
         (methodology sanity check)"
    );

    let mut b2 = U32StealBuilder::default();
    let _ = snapshot();
    let r = parser.parse_into(&input, &mut b2).unwrap();
    let allocs_b = snapshot();
    std::hint::black_box(&r);
    drop(r);
    print_row("B  u32 label, Vec-steal", allocs_b, b2.nodes, nbytes);

    let mut b3 = SmallVecBuilder::default();
    let _ = snapshot();
    let r = parser.parse_into(&input, &mut b3).unwrap();
    let allocs_d = snapshot();
    std::hint::black_box(&r);
    drop(r);
    print_row("D  u32 label, SmallVec<4>", allocs_d, b3.nodes, nbytes);

    let mut b4 = FlatBuilder::default();
    let _ = snapshot();
    let r = parser.parse_into(&input, &mut b4).unwrap();
    let allocs_e = snapshot();
    std::hint::black_box(r);
    print_row("E  u32 label, flat kids[]", allocs_e, b4.nodes, nbytes);

    println!();
    println!("A0 (real default) allocs: {a0}");
    println!(
        "B  vs A0: {:+} allocs ({:+.1}% of A0) — the label-interning delta (M2 alone)",
        allocs_b as i64 - a0 as i64,
        100.0 * (allocs_b as i64 - a0 as i64) as f64 / a0 as f64
    );
    println!(
        "D  vs B : {:+} allocs ({:+.1}% of B) — SmallVec<4> on top of interning",
        allocs_d as i64 - allocs_b as i64,
        100.0 * (allocs_d as i64 - allocs_b as i64) as f64 / allocs_b as f64
    );
    println!(
        "E  vs B : {:+} allocs ({:+.1}% of B) — flat kids[] on top of interning (full M5)",
        allocs_e as i64 - allocs_b as i64,
        100.0 * (allocs_e as i64 - allocs_b as i64) as f64 / allocs_b as f64
    );

    // ── Wall-clock (trend only, noisy — ADR-0007). ───────────────────────────
    let iters = 100;
    let time = |f: &mut dyn FnMut()| {
        let mut ts = Vec::with_capacity(iters);
        for _ in 0..iters {
            let t = Instant::now();
            f();
            ts.push(t.elapsed().as_nanos());
        }
        median(ts)
    };
    let mbps = |ns: u128| nbytes as f64 / (ns as f64 / 1e3);
    println!("\n── Wall-clock median over {iters} iters (trend, noisy) ──");
    let mut sv = SmallVecBuilder::default();
    let mut fl = FlatBuilder::default();
    let mut u32b = U32StealBuilder::default();
    let t_a0 = time(&mut || {
        std::hint::black_box(parser.parse(&input).unwrap());
    });
    let t_b = time(&mut || {
        std::hint::black_box(parser.parse_into(&input, &mut u32b).unwrap());
    });
    let t_d = time(&mut || {
        std::hint::black_box(parser.parse_into(&input, &mut sv).unwrap());
    });
    let t_e = time(&mut || {
        // FlatBuilder's `entries`/`kids` are real per-parse state (unlike the
        // other builders' trivial `nodes` counter) — clear them each iteration or
        // they accumulate across all 100 timed calls, growing without bound and
        // measuring "append to an ever-larger arena" instead of "one parse".
        fl.entries.clear();
        fl.kids.clear();
        std::hint::black_box(parser.parse_into(&input, &mut fl).unwrap());
    });
    for (name, t) in [
        ("A0 parse()", t_a0),
        ("B  u32 label", t_b),
        ("D  SmallVec<4>", t_d),
        ("E  flat kids[]", t_e),
    ] {
        println!(
            "{name:<16} {:>8.3} ms  {:>7.2} MB/s",
            t as f64 / 1e6,
            mbps(t)
        );
    }
}
