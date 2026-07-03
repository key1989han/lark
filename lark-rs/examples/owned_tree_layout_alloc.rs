//! THROWAWAY SPIKE (parser-optimization spike 2026-07-03): **what does each
//! memory-layout lever buy the DEFAULT owned-output path?** — the "marginal
//! stacking" open question of
//! `docs/notes/parser-optimization-research-2026-07.md` (Part 3, M1/M2/M5).
//!
//! `child_vec_alloc` answered the child-list question for the *zero-copy* span
//! path. This spike answers it for the *owned* path a `parse()` caller gets: all
//! variants keep fully-owned token values (the lexer's owned `Token` is stored
//! as-is), so the deltas isolate the two per-node representation costs — the
//! label `String` and the child list — without the zero-copy confound:
//!
//! * **parse()** — the default owned `Tree` (label `String` + `Vec<Child>` per
//!   node), the baseline.
//! * **intern** — same shape, label interned to a `Copy` `u32` (the rule index
//!   the builder already receives — M2). Isolates the per-node label `String`.
//! * **tape** — label `String` kept, but nodes appended to one flat `nodes[]`
//!   and every child list to one shared flat `kids[]` (M1/M5, the owned analog
//!   of `TapeTree`). Isolates the per-node child `Vec`.
//! * **intern+tape** — both levers stacked: remaining allocations are the
//!   lexer's owned token strings plus O(log n) arena growth.
//! * **smallvec** — label `String` kept, children in a `SmallVec<[_; 2]>`
//!   (inline up to 2, spill to heap above) — the inline-children third data
//!   point M5 asks for.
//!
//! Each variant reports real heap allocations (counting global allocator),
//! allocs per internal node, and a wall-clock trend. Node counts are asserted
//! equal across variants so the deltas compare identical output shapes.
//!
//! ```text
//! cargo run --release --example owned_tree_layout_alloc [records]
//! ```

// The variant node types deliberately *hold* label/meta/token payloads they never
// read back — the point is paying (or not paying) their allocation, not using them.
#![allow(dead_code, clippy::type_complexity)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::hint::black_box;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use lark_rs::tree::{Child, Tree};
use lark_rs::{Lark, LarkOptions, Meta, OutputBuilder, OutputContext, Token};
use smallvec::SmallVec;

// ─── Counting allocator (same as examples/child_vec_alloc.rs) ────────────────

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

// ─── Workload (same generator as child_vec_alloc.rs) ─────────────────────────

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

// ─── Variant builders ─────────────────────────────────────────────────────────

/// M2 only: the default tree shape with the label interned to the `Copy` rule
/// index the builder already receives — everything else identical to `Tree`.
enum IChild {
    Tree(ITree),
    Token(Token),
    None,
}
struct ITree {
    #[allow(dead_code)]
    label: u32,
    children: Vec<IChild>,
    #[allow(dead_code)]
    meta: Meta,
}
struct InternedLabelBuilder;
impl<'i> OutputBuilder<'i> for InternedLabelBuilder {
    type Value = IChild;
    fn token(&mut self, token: Token, _input: &'i str, _ctx: &OutputContext) -> IChild {
        IChild::Token(token)
    }
    fn reduce(
        &mut self,
        rule: usize,
        children: &mut Vec<IChild>,
        meta: &Meta,
        _ctx: &OutputContext,
    ) -> IChild {
        IChild::Tree(ITree {
            label: rule as u32,
            children: std::mem::take(children),
            meta: meta.clone(),
        })
    }
    fn placeholder(&mut self, _ctx: &OutputContext) -> IChild {
        IChild::None
    }
}
fn count_itree(root: &IChild) -> u64 {
    let mut n = 0;
    let mut stack = vec![root];
    while let Some(c) = stack.pop() {
        if let IChild::Tree(t) = c {
            n += 1;
            stack.extend(t.children.iter());
        }
    }
    n
}

/// M1+M5: the owned flat tape — one `nodes[]` arena, one shared `kids[]` child
/// arena. `LABEL_STRINGS` keeps/drops the per-node owned label `String`, so the
/// same builder covers "tape" (true) and "intern+tape" (false).
enum TNode {
    Token(Token),
    None,
    Node {
        #[allow(dead_code)]
        label: Option<String>, // None when LABEL_STRINGS is false (u32 rule id suffices)
        #[allow(dead_code)]
        rule: u32,
        #[allow(dead_code)]
        meta: Meta,
        #[allow(dead_code)]
        kids: (u32, u32), // (start, len) into the shared kids[] arena
    },
}
struct TapeOwnedBuilder<const LABEL_STRINGS: bool> {
    nodes: Vec<TNode>,
    kids: Vec<u32>,
}
impl<const L: bool> TapeOwnedBuilder<L> {
    fn new() -> Self {
        TapeOwnedBuilder {
            nodes: Vec::new(),
            kids: Vec::new(),
        }
    }
    fn node_count(&self) -> u64 {
        self.nodes
            .iter()
            .filter(|n| matches!(n, TNode::Node { .. }))
            .count() as u64
    }
}
impl<'i, const L: bool> OutputBuilder<'i> for TapeOwnedBuilder<L> {
    type Value = u32;
    fn token(&mut self, token: Token, _input: &'i str, _ctx: &OutputContext) -> u32 {
        self.nodes.push(TNode::Token(token));
        (self.nodes.len() - 1) as u32
    }
    fn reduce(
        &mut self,
        rule: usize,
        children: &mut Vec<u32>,
        meta: &Meta,
        ctx: &OutputContext,
    ) -> u32 {
        let start = self.kids.len() as u32;
        self.kids.append(children);
        self.nodes.push(TNode::Node {
            label: L.then(|| ctx.callback_name(rule).to_string()),
            rule: rule as u32,
            meta: meta.clone(),
            kids: (start, self.kids.len() as u32 - start),
        });
        (self.nodes.len() - 1) as u32
    }
    fn placeholder(&mut self, _ctx: &OutputContext) -> u32 {
        self.nodes.push(TNode::None);
        (self.nodes.len() - 1) as u32
    }
}

/// M5 third data point: default shape with an inline-capacity-2 `SmallVec`
/// child list (label `String` kept, so the delta vs `parse()` is child-list-only).
///
/// Structural catch (a finding in itself): an *inline* child list cannot hold an
/// unboxed recursive node — `SmallVec<[SChild; 2]>` inside `STree` inside `SChild`
/// is an infinite-size type, where the default `Vec<Child>` breaks the cycle with
/// its heap pointer. The recursion must go through some indirection, so the node
/// (and the fat `Token`) get boxed — i.e. SmallVec-inline children trade the
/// per-node child-`Vec` allocation for a per-node `Box` allocation.
enum SChild {
    Tree(Box<STree>),
    Token(Box<Token>), // boxed so the inline SmallVec doesn't balloon the parent
    None,
}
struct STree {
    #[allow(dead_code)]
    label: String,
    children: SmallVec<[SChild; 2]>,
    #[allow(dead_code)]
    meta: Meta,
}
struct SmallVecBuilder;
impl<'i> OutputBuilder<'i> for SmallVecBuilder {
    type Value = SChild;
    fn token(&mut self, token: Token, _input: &'i str, _ctx: &OutputContext) -> SChild {
        SChild::Token(Box::new(token))
    }
    fn reduce(
        &mut self,
        rule: usize,
        children: &mut Vec<SChild>,
        meta: &Meta,
        ctx: &OutputContext,
    ) -> SChild {
        SChild::Tree(Box::new(STree {
            label: ctx.callback_name(rule).to_string(),
            children: children.drain(..).collect(),
            meta: meta.clone(),
        }))
    }
    fn placeholder(&mut self, _ctx: &OutputContext) -> SChild {
        SChild::None
    }
}
fn count_stree(root: &SChild) -> u64 {
    let mut n = 0;
    let mut stack = vec![root];
    while let Some(c) = stack.pop() {
        if let SChild::Tree(t) = c {
            n += 1;
            stack.extend(t.children.iter());
        }
    }
    n
}

fn count_internal_nodes(root: &Tree) -> u64 {
    let mut n = 0;
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

fn main() {
    let records = std::env::args()
        .nth(1)
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(2000);

    let parser = Lark::new(JSON_GRAMMAR, LarkOptions::default()).expect("json grammar builds");

    println!("── Owned-output layout matrix: real heap allocations per parse ──");
    println!("   (all variants keep fully-owned token values; deltas isolate the");
    println!("    per-node label String and the per-node child Vec)\n");

    // Size sweep for the allocation counts (one parse per cell, deterministic).
    for sweep in [records / 4, records, records * 4] {
        let input = gen_json(sweep.max(1), 8);
        let nbytes = input.len();

        // Baseline + denominator.
        let owned = parser.parse(&input).unwrap();
        let internal_nodes = owned.as_tree().map(count_internal_nodes).unwrap_or(0);
        drop(owned);

        // Warm lazy scanners so the first measured parse isn't charged for them.
        let _ = parser.parse(&input).unwrap();

        println!(
            "input {nbytes} bytes, {internal_nodes} internal nodes  ({} records)",
            sweep.max(1)
        );
        println!(
            "  {:<12} {:>10} {:>12} {:>12} {:>14}",
            "variant", "allocs", "allocs/byte", "allocs/node", "bytes"
        );

        let per = |a: u64| {
            (
                a as f64 / nbytes as f64,
                a as f64 / internal_nodes.max(1) as f64,
            )
        };
        let row = |name: &str, a: u64, b: u64| {
            let (pb, pn) = per(a);
            println!("  {name:<12} {a:>10} {pb:>12.3} {pn:>12.3} {b:>14}");
        };

        let _ = snapshot();
        let t = parser.parse(&input).unwrap();
        let (a, b) = snapshot();
        let n0 = t.as_tree().map(count_internal_nodes).unwrap_or(0);
        drop(t);
        row("parse()", a, b);

        let mut ib = InternedLabelBuilder;
        let _ = snapshot();
        let v = parser.parse_into(&input, &mut ib).unwrap();
        let (a, b) = snapshot();
        assert_eq!(count_itree(&v), n0, "intern: node count diverges");
        drop(v);
        row("intern", a, b);

        let mut tb = TapeOwnedBuilder::<true>::new();
        let _ = snapshot();
        let root = parser.parse_into(&input, &mut tb).unwrap();
        let (a, b) = snapshot();
        black_box(root);
        assert_eq!(tb.node_count(), n0, "tape: node count diverges");
        drop(tb);
        row("tape", a, b);

        let mut itb = TapeOwnedBuilder::<false>::new();
        let _ = snapshot();
        let root = parser.parse_into(&input, &mut itb).unwrap();
        let (a, b) = snapshot();
        black_box(root);
        assert_eq!(itb.node_count(), n0, "intern+tape: node count diverges");
        drop(itb);
        row("intern+tape", a, b);

        let mut sb = SmallVecBuilder;
        let _ = snapshot();
        let v = parser.parse_into(&input, &mut sb).unwrap();
        let (a, b) = snapshot();
        assert_eq!(count_stree(&v), n0, "smallvec: node count diverges");
        drop(v);
        row("smallvec", a, b);
        println!();
    }

    // Wall-clock trend at the default size only (noisy — ADR-0007).
    let input = gen_json(records, 8);
    let nbytes = input.len();
    let iters = 15;
    let time = |f: &dyn Fn()| {
        let mut ts = Vec::with_capacity(iters);
        for _ in 0..iters {
            let t = Instant::now();
            f();
            ts.push(t.elapsed().as_nanos());
        }
        median(ts)
    };
    println!("── Wall-clock median over {iters} iters at {nbytes} bytes (trend, noisy) ──");
    let base = time(&|| {
        black_box(parser.parse(&input).unwrap());
    });
    let mbps = |ns: u128| nbytes as f64 / (ns as f64 / 1e3);
    println!(
        "  {:<12} {:>10.3} ms {:>8.2} MB/s   1.00x",
        "parse()",
        base as f64 / 1e6,
        mbps(base)
    );
    let variants: Vec<(&str, Box<dyn Fn()>)> = vec![
        (
            "intern",
            Box::new(|| {
                black_box(
                    parser
                        .parse_into(&input, &mut InternedLabelBuilder)
                        .unwrap(),
                );
            }),
        ),
        (
            "tape",
            Box::new(|| {
                let mut b = TapeOwnedBuilder::<true>::new();
                black_box(parser.parse_into(&input, &mut b).unwrap());
                black_box(&b.kids);
            }),
        ),
        (
            "intern+tape",
            Box::new(|| {
                let mut b = TapeOwnedBuilder::<false>::new();
                black_box(parser.parse_into(&input, &mut b).unwrap());
                black_box(&b.kids);
            }),
        ),
        (
            "smallvec",
            Box::new(|| {
                black_box(parser.parse_into(&input, &mut SmallVecBuilder).unwrap());
            }),
        ),
    ];
    for (name, f) in &variants {
        let ns = time(f);
        println!(
            "  {name:<12} {:>10.3} ms {:>8.2} MB/s   {:.2}x",
            ns as f64 / 1e6,
            mbps(ns),
            base as f64 / ns as f64
        );
    }
}
