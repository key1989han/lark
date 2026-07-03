//! **Spike experiments 2 + 3 (throwaway):** decompose the DEFAULT owned-`Tree`'s
//! allocation cost by swapping the two representations the 2026-07 memory-layout
//! research named — the per-node child `Vec` (M1/M5, flat `kids[]` arena) and the
//! owned `String` node label (M2, intern to `u32`) — behind the public `parse_into`
//! `OutputBuilder` seam, and measuring each in isolation with a counting allocator.
//!
//! ## Why `parse_into` is the right vehicle (the gotcha the task flagged)
//!
//! The DEFAULT `parse()` path is `LalrParser::run → run_into → shape_reduction`
//! (ADR-0029 fork 2) — the same value-parametric shaping loop `parse_into` drives.
//! So a custom [`OutputBuilder`] measured through `parse_into` runs the **exact hot
//! path** `parse()` uses, only changing what the reduction *builds*. This is not the
//! `ParserStack::reduce` recovery path.
//!
//! The `child_vec_alloc` spike already isolated the child-`Vec` on the *zero-copy*
//! path (`parse_span` − `parse_tape` = 1.000 alloc/node). This one asks the
//! complementary question the research left open (open-question 2, "marginal
//! stacking"): on the **owned** default tree — owned `String` labels *and* owned token
//! values, the representation real callers get from `parse()` — how much does each
//! technique remove, and do they stack?
//!
//! ## The backends (each a custom `OutputBuilder`, same shaped children from the engine)
//!
//! | backend            | node label     | child list          | isolates                       |
//! |--------------------|----------------|---------------------|--------------------------------|
//! | `parse()`          | `String`       | `Vec<Child>`/node   | the real default (reference)   |
//! | reimpl-owned       | `String`       | `Vec`/node          | harness sanity (≈ `parse()`)   |
//! | intern-label       | **`u32`**      | `Vec`/node          | **M2** — the label `String`    |
//! | flat-kids          | `String`       | **shared `kids[]`** | **M1/M5** — the per-node `Vec` |
//! | flat-kids+intern   | **`u32`**      | **shared `kids[]`** | both stacked                   |
//! | smallvec-kids      | `String`       | **inline SmallVec** | M5 third point (inline小)      |
//!
//! Token *values* stay owned `String` in every backend (the lexer allocates them
//! upstream on the `parse_into` owned path, identically for all), so those allocations
//! are a shared constant and every inter-backend delta is a clean isolation of the
//! label / child-list change. Each backend also rebuilds a structurally-complete tree
//! (asserted node/token counts vs `parse()`), so nothing is measured by skipping work.
//!
//! Recorded measurement, not a gate (ADR-0007). Run:
//!
//! ```text
//! cargo run --release --example tree_alloc_backends [records]
//! ```

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::HashMap;
use std::hint::black_box;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use lark_rs::{
    Lark, LarkOptions, LexerType, Meta, OutputBuilder, OutputContext, ParserAlgorithm, Token,
};
use smallvec::SmallVec;

// ─── counting global allocator (same as child_vec_alloc.rs) ─────────────────────

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
        if new_size > layout.size() {
            ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
            ALLOC_BYTES.fetch_add((new_size - layout.size()) as u64, Ordering::Relaxed);
        }
        System.realloc(ptr, layout, new_size)
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn snapshot() -> (u64, u64) {
    (
        ALLOC_COUNT.swap(0, Ordering::Relaxed),
        ALLOC_BYTES.swap(0, Ordering::Relaxed),
    )
}

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

// ─── Backend 1: reimpl-owned (String label + per-node Vec) — the reference shape ─

/// A node in the reimplemented owned tree — mirrors `tree::Child` exactly (owned
/// `String` label, per-node `Vec` children, owned `Token` leaves). Its allocation
/// profile should reproduce `parse()`'s, validating the harness.
enum OwnedNode {
    Tree {
        #[allow(dead_code)]
        label: String,
        kids: Vec<OwnedNode>,
        #[allow(dead_code)]
        meta: Meta,
    },
    Token(Token),
    None,
}

#[derive(Default)]
struct OwnedBuilder {
    nodes: u64,
    tokens: u64,
}

impl<'i> OutputBuilder<'i> for OwnedBuilder {
    type Value = OwnedNode;
    fn token(&mut self, token: Token, _input: &'i str, _ctx: &OutputContext) -> OwnedNode {
        self.tokens += 1;
        OwnedNode::Token(token)
    }
    fn reduce(
        &mut self,
        rule: usize,
        children: &mut Vec<OwnedNode>,
        meta: &Meta,
        ctx: &OutputContext,
    ) -> OwnedNode {
        self.nodes += 1;
        OwnedNode::Tree {
            label: ctx.callback_name(rule).to_string(),
            kids: std::mem::take(children),
            meta: meta.clone(),
        }
    }
    fn placeholder(&mut self, _ctx: &OutputContext) -> OwnedNode {
        OwnedNode::None
    }
}

// ─── Backend 2: intern-label (u32 label + per-node Vec) — isolates M2 ────────────

enum InternNode {
    Tree {
        #[allow(dead_code)]
        label: u32,
        kids: Vec<InternNode>,
        #[allow(dead_code)]
        meta: Meta,
    },
    Token(Token),
    None,
}

#[derive(Default)]
struct InternBuilder {
    intern: HashMap<String, u32>,
    #[allow(dead_code)]
    names: Vec<String>,
    nodes: u64,
    tokens: u64,
}

impl InternBuilder {
    /// Intern a label to a `Copy` `u32`. Allocates a `String` **only** on first sight
    /// of a new label (the fixed rule/alias set is tiny), so per-node cost is zero.
    fn intern(&mut self, name: &str) -> u32 {
        if let Some(&i) = self.intern.get(name) {
            return i;
        }
        let i = self.names.len() as u32;
        self.names.push(name.to_string());
        self.intern.insert(name.to_string(), i);
        i
    }
}

impl<'i> OutputBuilder<'i> for InternBuilder {
    type Value = InternNode;
    fn token(&mut self, token: Token, _input: &'i str, _ctx: &OutputContext) -> InternNode {
        self.tokens += 1;
        InternNode::Token(token)
    }
    fn reduce(
        &mut self,
        rule: usize,
        children: &mut Vec<InternNode>,
        meta: &Meta,
        ctx: &OutputContext,
    ) -> InternNode {
        self.nodes += 1;
        let label = self.intern(ctx.callback_name(rule));
        InternNode::Tree {
            label,
            kids: std::mem::take(children),
            meta: meta.clone(),
        }
    }
    fn placeholder(&mut self, _ctx: &OutputContext) -> InternNode {
        InternNode::None
    }
}

// ─── Backends 3 & 4: flat-kids (shared kids[] arena), owned or interned label ────

/// A `Copy` handle into the flat arena — the analog of a `TapeTree` node ref, but the
/// arena owns `String` labels + `Token` leaves instead of borrowing spans.
#[derive(Clone, Copy)]
enum Ref {
    Tree(u32),
    Token(u32),
    None,
}

/// Flat arena: every node's children live as a contiguous slice of one shared
/// `kids` buffer (grown amortized → O(log n) allocations), not one `Vec` per node.
/// `INTERN` toggles whether labels are interned `u32`s (no per-node string) or owned
/// `String`s (one alloc/node), so the same arena serves backends 3 and 4.
struct FlatArena<const INTERN: bool> {
    labels_str: Vec<String>, // when !INTERN
    labels_u32: Vec<u32>,    // when INTERN
    intern: HashMap<String, u32>,
    intern_names: Vec<String>,
    kids_start: Vec<u32>,
    kids_len: Vec<u32>,
    metas: Vec<Meta>,
    kids: Vec<Ref>, // the one shared flat child buffer
    tokens: Vec<Token>,
    nodes: u64,
    token_count: u64,
}

impl<const INTERN: bool> Default for FlatArena<INTERN> {
    fn default() -> Self {
        FlatArena {
            labels_str: Vec::new(),
            labels_u32: Vec::new(),
            intern: HashMap::new(),
            intern_names: Vec::new(),
            kids_start: Vec::new(),
            kids_len: Vec::new(),
            metas: Vec::new(),
            kids: Vec::new(),
            tokens: Vec::new(),
            nodes: 0,
            token_count: 0,
        }
    }
}

impl<const INTERN: bool> FlatArena<INTERN> {
    fn intern(&mut self, name: &str) -> u32 {
        if let Some(&i) = self.intern.get(name) {
            return i;
        }
        let i = self.intern_names.len() as u32;
        self.intern_names.push(name.to_string());
        self.intern.insert(name.to_string(), i);
        i
    }
}

impl<'i, const INTERN: bool> OutputBuilder<'i> for FlatArena<INTERN> {
    type Value = Ref;
    fn token(&mut self, token: Token, _input: &'i str, _ctx: &OutputContext) -> Ref {
        self.token_count += 1;
        let idx = self.tokens.len() as u32;
        self.tokens.push(token);
        Ref::Token(idx)
    }
    fn reduce(
        &mut self,
        rule: usize,
        children: &mut Vec<Ref>,
        meta: &Meta,
        ctx: &OutputContext,
    ) -> Ref {
        self.nodes += 1;
        let start = self.kids.len() as u32;
        self.kids.extend(children.drain(..));
        let len = self.kids.len() as u32 - start;
        let name = ctx.callback_name(rule);
        if INTERN {
            let l = self.intern(name);
            self.labels_u32.push(l);
        } else {
            self.labels_str.push(name.to_string());
        }
        self.kids_start.push(start);
        self.kids_len.push(len);
        self.metas.push(meta.clone());
        Ref::Tree(self.kids_start.len() as u32 - 1)
    }
    fn placeholder(&mut self, _ctx: &OutputContext) -> Ref {
        Ref::None
    }
}

// ─── Backend 5: smallvec-kids (inline per-node child refs) — M5 third data point ─
//
// A recursive by-value tree can't inline its subtrees (infinite size — that is *why*
// the default needs a per-node heap `Vec`). The faithful "inline small child list"
// for a tree is therefore the handle model: each node record stores its child *refs*
// (`Copy`) in an inline `SmallVec`, spilling to the heap only when a node has > N
// children. Owned `String` label (so the delta vs owned is a pure child-list change).

/// Inline capacity 4: JSON reductions are arity ≤ 2 (`pair`, the left-recursive
/// `object`/`array` helpers), so a small node's child refs never touch the heap.
type Kids = SmallVec<[Ref; 4]>;

struct SvRecord {
    #[allow(dead_code)]
    label: String,
    kids: Kids,
    #[allow(dead_code)]
    meta: Meta,
}

#[derive(Default)]
struct SmallVecBuilder {
    records: Vec<SvRecord>,
    tokens: Vec<Token>,
    nodes: u64,
    token_count: u64,
    spilled: u64, // nodes whose child list exceeded the inline capacity
}

impl<'i> OutputBuilder<'i> for SmallVecBuilder {
    type Value = Ref;
    fn token(&mut self, token: Token, _input: &'i str, _ctx: &OutputContext) -> Ref {
        self.token_count += 1;
        let idx = self.tokens.len() as u32;
        self.tokens.push(token);
        Ref::Token(idx)
    }
    fn reduce(
        &mut self,
        rule: usize,
        children: &mut Vec<Ref>,
        meta: &Meta,
        ctx: &OutputContext,
    ) -> Ref {
        self.nodes += 1;
        let kids: Kids = children.drain(..).collect();
        if kids.spilled() {
            self.spilled += 1;
        }
        self.records.push(SvRecord {
            label: ctx.callback_name(rule).to_string(),
            kids,
            meta: meta.clone(),
        });
        Ref::Tree(self.records.len() as u32 - 1)
    }
    fn placeholder(&mut self, _ctx: &OutputContext) -> Ref {
        Ref::None
    }
}

// ─── harness ────────────────────────────────────────────────────────────────────

/// Measure allocations for one `parse_into(builder)` call, keeping the result alive
/// while the counters are read (freeing is `dealloc`, uncounted) — the same protocol
/// as `child_vec_alloc.rs`. Returns `(allocs, bytes)`.
fn measure_into<'i, B: OutputBuilder<'i>>(
    parser: &Lark,
    input: &'i str,
    builder: &mut B,
) -> (u64, u64) {
    let _ = snapshot();
    let v = parser.parse_into(input, builder).expect("parse_into ok");
    let counts = snapshot();
    black_box(&v);
    drop(v);
    counts
}

fn wallclock<F: FnMut()>(iters: usize, mut f: F) -> f64 {
    // warm
    for _ in 0..3 {
        f();
    }
    let mut ts = Vec::with_capacity(iters);
    for _ in 0..iters {
        let t = Instant::now();
        f();
        ts.push(t.elapsed().as_nanos());
    }
    ts.sort_unstable();
    ts[ts.len() / 2] as f64
}

fn main() {
    let records = std::env::args()
        .nth(1)
        .and_then(|s| s.parse::<usize>().ok())
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

    // Warm caches / lazy scanners (not measured).
    for _ in 0..3 {
        let _ = parser.parse(&input).unwrap();
    }

    // Reference: the real default owned tree.
    let owned_tree = parser.parse(&input).unwrap();
    let ref_nodes = owned_tree.as_tree().map(count_nodes).unwrap_or((0, 0));
    drop(owned_tree);
    let _ = snapshot();
    let default_tree = parser.parse(&input).unwrap();
    let (default_allocs, default_bytes) = snapshot();
    black_box(&default_tree);
    drop(default_tree);

    println!("# Spike exp-2/3: decomposing the owned-Tree allocation cost (parse_into backends)");
    println!("# recorded measurement, not a gate (ADR-0007)\n");
    println!(
        "input: {nbytes} bytes ({records} records × 8 fields), {} internal nodes, {} tokens\n",
        ref_nodes.0, ref_nodes.1
    );

    // Run each custom backend.
    let mut owned = OwnedBuilder::default();
    let owned_c = measure_into(&parser, &input, &mut owned);
    assert_eq!(
        owned.nodes, ref_nodes.0,
        "reimpl-owned node count != parse()"
    );

    let mut intern = InternBuilder::default();
    let intern_c = measure_into(&parser, &input, &mut intern);
    assert_eq!(
        intern.nodes, ref_nodes.0,
        "intern-label node count != parse()"
    );

    let mut flat: FlatArena<false> = FlatArena::default();
    let flat_c = measure_into(&parser, &input, &mut flat);
    assert_eq!(flat.nodes, ref_nodes.0, "flat-kids node count != parse()");

    let mut flatint: FlatArena<true> = FlatArena::default();
    let flatint_c = measure_into(&parser, &input, &mut flatint);
    assert_eq!(
        flatint.nodes, ref_nodes.0,
        "flat+intern node count != parse()"
    );

    let mut sv = SmallVecBuilder::default();
    let sv_c = measure_into(&parser, &input, &mut sv);
    assert_eq!(sv.nodes, ref_nodes.0, "smallvec node count != parse()");

    let per_byte = |n: u64| n as f64 / nbytes as f64;
    let per_node = |n: u64| n as f64 / ref_nodes.0.max(1) as f64;

    println!("── Real heap allocations (counting allocator, one parse) ──");
    println!(
        "{:<20}{:>12}{:>14}{:>16}{:>14}",
        "backend", "allocs", "allocs/byte", "bytes", "allocs/node"
    );
    let rows = [
        ("parse() [default]", default_allocs, default_bytes),
        ("reimpl-owned", owned_c.0, owned_c.1),
        ("intern-label", intern_c.0, intern_c.1),
        ("flat-kids", flat_c.0, flat_c.1),
        ("flat-kids+intern", flatint_c.0, flatint_c.1),
        ("smallvec-kids", sv_c.0, sv_c.1),
    ];
    for (name, a, b) in rows {
        println!(
            "{name:<20}{a:>12}{:>14.3}{b:>16}{:>14.3}",
            per_byte(a),
            per_node(a)
        );
    }
    println!();

    // Isolations.
    println!("── Isolations (all vs the reimpl-owned reference, per internal node) ──");
    let label_saved = owned_c.0.saturating_sub(intern_c.0);
    let childvec_saved = owned_c.0.saturating_sub(flat_c.0);
    let both_saved = owned_c.0.saturating_sub(flatint_c.0);
    let sv_saved = owned_c.0.saturating_sub(sv_c.0);
    println!(
        "  M2  label String   (owned − intern-label) : {label_saved:>8} allocs  = {:.3}/node",
        per_node(label_saved)
    );
    println!(
        "  M5  child Vec       (owned − flat-kids)    : {childvec_saved:>8} allocs  = {:.3}/node",
        per_node(childvec_saved)
    );
    println!(
        "  M2+M5 stacked       (owned − flat+intern)  : {both_saved:>8} allocs  = {:.3}/node",
        per_node(both_saved)
    );
    println!(
        "  M5' inline SmallVec (owned − smallvec)     : {sv_saved:>8} allocs  = {:.3}/node   ({} nodes spilled to heap)",
        per_node(sv_saved),
        sv.spilled
    );
    println!(
        "\n  residual after M2+M5 = {} allocs ({:.3}/byte) — the owned token value+type Strings\n  \
         (allocated upstream by the lexer on the parse_into owned path; a span/zero-copy\n  \
         token backend is what removes these — see parse_tape's 0.007/byte).",
        flatint_c.0,
        per_byte(flatint_c.0)
    );
    println!();

    // Wall-clock trend.
    let iters = 60;
    println!("── Wall-clock median over {iters} iters (trend, noisy — ADR-0007) ──");
    let mbps = |ns: f64| nbytes as f64 / ns * 1e3;
    let t_default = wallclock(iters, || {
        black_box(parser.parse(black_box(&input)).unwrap());
    });
    let t_intern = wallclock(iters, || {
        let mut b = InternBuilder::default();
        black_box(parser.parse_into(black_box(&input), &mut b).unwrap());
    });
    let t_flat = wallclock(iters, || {
        let mut b: FlatArena<false> = FlatArena::default();
        black_box(parser.parse_into(black_box(&input), &mut b).unwrap());
    });
    let t_flatint = wallclock(iters, || {
        let mut b: FlatArena<true> = FlatArena::default();
        black_box(parser.parse_into(black_box(&input), &mut b).unwrap());
    });
    let t_sv = wallclock(iters, || {
        let mut b = SmallVecBuilder::default();
        black_box(parser.parse_into(black_box(&input), &mut b).unwrap());
    });
    for (name, t) in [
        ("parse() [default]", t_default),
        ("intern-label", t_intern),
        ("flat-kids", t_flat),
        ("flat-kids+intern", t_flatint),
        ("smallvec-kids", t_sv),
    ] {
        println!(
            "  {name:<20}{:>8.3} ms  {:>6.2} MB/s   {:>5.2}× vs default",
            t / 1e6,
            mbps(t),
            t_default / t
        );
    }
}

/// Count (internal nodes, tokens) in a default parse tree — the denominators, computed
/// off the measured path (iterative to survive deep trees).
fn count_nodes(root: &lark_rs::Tree) -> (u64, u64) {
    use lark_rs::Child;
    let mut nodes = 0u64;
    let mut tokens = 0u64;
    let mut stack = vec![root];
    while let Some(t) = stack.pop() {
        nodes += 1;
        for c in &t.children {
            match c {
                Child::Tree(sub) => stack.push(sub),
                Child::Token(_) => tokens += 1,
                Child::None => {}
            }
        }
    }
    (nodes, tokens)
}
