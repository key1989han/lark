//! THROWAWAY SPIKE (standalone-bake spike 2026-07-03, issue #620): drive the
//! spliced generated-parser copies (`examples/standalone_spike/gen/`, produced by
//! `standalone_bake_gen`) through correctness gates and the #620 measurements.
//!
//! Per grammar (json + the qualifying wild-bank projects) and per variant
//! (stock / lean / baked / interned — see `standalone_bake_gen.rs` for what each
//! is):
//!
//! * **Gates (deterministic, before any timing):** every variant's token stream
//!   (`type_id`, value, line/column, char spans) must be byte-identical to the
//!   stock copy's over the full workload, and every variant's parse trees must
//!   render byte-identical to the **in-process basic-lexer LALR oracle**'s
//!   (`Display` formats are identical by construction). A diverging variant is a
//!   bug in the bake, not a data point.
//! * **One-shot column:** `Parser::new()` (the load-time scanner build each
//!   variant pays) and `Parser::new()+parse` across an input-size sweep — the
//!   break-even #620 needs. In-process `Lark::new()+parse` is printed for
//!   context (what standalone export saves today).
//! * **Reused column:** whole-corpus parse and lex-only throughput (scan share
//!   of the generated parser), plus counting-allocator allocations per parse
//!   (the L5d deterministic signal: interned = −1 alloc/token −1 alloc/node).
//!
//! Wall-clock is a trend (BENCH.md): only same-session ratios travel.
//!
//! ```text
//! cargo run --release --features baked-dfa-spike --example standalone_bake_bench
//! ```

use std::alloc::{GlobalAlloc, Layout, System};
use std::hint::black_box;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use lark_rs::{Lark, LarkOptions, LexerType, ParserAlgorithm};

// ─── Counting allocator (same as examples/owned_tree_layout_alloc.rs) ─────────

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

fn count_allocs<F: FnOnce()>(f: F) -> (u64, u64) {
    let a0 = ALLOC_COUNT.load(Ordering::Relaxed);
    let b0 = ALLOC_BYTES.load(Ordering::Relaxed);
    f();
    (
        ALLOC_COUNT.load(Ordering::Relaxed) - a0,
        ALLOC_BYTES.load(Ordering::Relaxed) - b0,
    )
}

// ─── The generated variant copies under test ──────────────────────────────────

mod json_stock {
    include!("standalone_spike/gen/json_stock.rs");
}
mod json_lean {
    include!("standalone_spike/gen/json_lean.rs");
}
mod json_baked {
    include!("standalone_spike/gen/json_baked.rs");
}
mod json_interned {
    include!("standalone_spike/gen/json_interned.rs");
}
mod matter_idl_stock {
    include!("standalone_spike/gen/matter_idl_stock.rs");
}
mod matter_idl_lean {
    include!("standalone_spike/gen/matter_idl_lean.rs");
}
mod matter_idl_baked {
    include!("standalone_spike/gen/matter_idl_baked.rs");
}
mod matter_idl_interned {
    include!("standalone_spike/gen/matter_idl_interned.rs");
}
mod poetry_markers_stock {
    include!("standalone_spike/gen/poetry_markers_stock.rs");
}
mod poetry_markers_lean {
    include!("standalone_spike/gen/poetry_markers_lean.rs");
}
mod poetry_markers_baked {
    include!("standalone_spike/gen/poetry_markers_baked.rs");
}
mod poetry_markers_interned {
    include!("standalone_spike/gen/poetry_markers_interned.rs");
}

// ─── Timing (same estimator as the prior spike) ───────────────────────────────

fn measure<F: FnMut()>(mut f: F) -> f64 {
    let mut iters = 1usize;
    loop {
        let t = Instant::now();
        for _ in 0..iters {
            f();
        }
        if t.elapsed() >= Duration::from_millis(1) || iters >= 1 << 22 {
            break;
        }
        iters = (iters * 2).max(1);
    }
    let mut samples: Vec<f64> = Vec::new();
    let overall = Instant::now();
    while samples.len() < 30 && overall.elapsed() < Duration::from_millis(1000) {
        let t = Instant::now();
        for _ in 0..iters {
            f();
        }
        samples.push(t.elapsed().as_nanos() as f64 / iters as f64);
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    samples[samples.len() / 2]
}

// ─── Per-variant runner ───────────────────────────────────────────────────────

type TokenRec = (u32, String, usize, usize, usize, usize);

struct VariantResult {
    label: &'static str,
    /// `Parser::new()` — the per-process scanner-build cost.
    new_ns: f64,
    /// Whole-corpus reused parse / lex-only (scan+token) time.
    parse_ns: f64,
    lex_ns: f64,
    /// Counting-allocator totals for one whole-corpus parse pass.
    allocs: u64,
    alloc_bytes: u64,
    /// Deterministic differential signals.
    stream: Vec<TokenRec>,
    trees: String,
    /// `Parser::new()+parse` per one-shot input.
    oneshot_ns: Vec<f64>,
}

macro_rules! variant_fn {
    ($fname:ident, $m:ident, $label:expr) => {
        fn $fname(corpus: &[String], oneshot_inputs: &[String]) -> VariantResult {
            use $m::parser::Parser;
            let new_ns = measure(|| {
                black_box(Parser::new());
            });
            let parser = Parser::new();
            let mut stream: Vec<TokenRec> = Vec::new();
            for t in corpus {
                for tok in parser.spike_lex(t).expect("variant lexes corpus") {
                    stream.push((
                        tok.type_id,
                        tok.value.clone(),
                        tok.line,
                        tok.column,
                        tok.start_pos,
                        tok.end_pos,
                    ));
                }
            }
            let mut trees = String::new();
            for t in corpus {
                let tree = parser.parse(t).expect("variant parses corpus");
                trees.push_str(&tree.to_string());
                trees.push('\n');
            }
            let (allocs, alloc_bytes) = count_allocs(|| {
                for t in corpus {
                    black_box(parser.parse(black_box(t)).unwrap());
                }
            });
            let parse_ns = measure(|| {
                for t in corpus {
                    black_box(parser.parse(black_box(t)).unwrap());
                }
            });
            let lex_ns = measure(|| {
                for t in corpus {
                    black_box(parser.spike_lex(black_box(t)).unwrap());
                }
            });
            let oneshot_ns: Vec<f64> = oneshot_inputs
                .iter()
                .map(|t| {
                    measure(|| {
                        let p = Parser::new();
                        black_box(p.parse(black_box(t)).unwrap());
                    })
                })
                .collect();
            VariantResult {
                label: $label,
                new_ns,
                parse_ns,
                lex_ns,
                allocs,
                alloc_bytes,
                stream,
                trees,
                oneshot_ns,
            }
        }
    };
}

variant_fn!(json_stock_run, json_stock, "stock");
variant_fn!(json_lean_run, json_lean, "lean");
variant_fn!(json_baked_run, json_baked, "baked");
variant_fn!(json_interned_run, json_interned, "interned");
variant_fn!(matter_stock_run, matter_idl_stock, "stock");
variant_fn!(matter_lean_run, matter_idl_lean, "lean");
variant_fn!(matter_baked_run, matter_idl_baked, "baked");
variant_fn!(matter_interned_run, matter_idl_interned, "interned");
variant_fn!(markers_stock_run, poetry_markers_stock, "stock");
variant_fn!(markers_lean_run, poetry_markers_lean, "lean");
variant_fn!(markers_baked_run, poetry_markers_baked, "baked");
variant_fn!(markers_interned_run, poetry_markers_interned, "interned");

// ─── Workloads ────────────────────────────────────────────────────────────────

// Byte-identical to `standalone_bake_gen.rs` / the prior spike.
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

/// Grammar source + upstream options + corpus for a wild project (mirrors
/// `standalone_bake_gen::wild_case`).
fn wild_case(dir: &str) -> (String, LarkOptions, Vec<String>) {
    let pdir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/wild")
        .join(dir);
    let meta: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(pdir.join("meta.json")).unwrap()).unwrap();
    let o = &meta["lark_options"];
    let mut flags = 0u32;
    if let Some(letters) = o["g_regex_flags"].as_str() {
        use lark_rs::grammar::terminal::flags as tf;
        for ch in letters.chars() {
            flags |= match ch {
                'i' => tf::IGNORECASE,
                'm' => tf::MULTILINE,
                's' => tf::DOTALL,
                'x' => tf::VERBOSE,
                _ => unreachable!("unknown g_regex_flags letter"),
            };
        }
    }
    let grammar_src =
        std::fs::read_to_string(pdir.join(meta["entry_grammar"].as_str().unwrap())).unwrap();
    let options = LarkOptions {
        start: vec![o["start"].as_str().unwrap().to_string()],
        parser: ParserAlgorithm::Lalr,
        maybe_placeholders: o["maybe_placeholders"].as_bool().unwrap_or(true),
        keep_all_tokens: o["keep_all_tokens"].as_bool().unwrap_or(false),
        g_regex_flags: flags,
        base_path: Some(pdir.join("grammar")),
        ..Default::default()
    };
    let corpus: Vec<String> = meta["inputs"]
        .as_object()
        .unwrap()
        .keys()
        .filter_map(|rel| std::fs::read_to_string(pdir.join(rel)).ok())
        .collect();
    (grammar_src, options, corpus)
}

// ─── Per-grammar bench ────────────────────────────────────────────────────────

type VariantFn = fn(&[String], &[String]) -> VariantResult;

fn bench_grammar(
    name: &str,
    grammar_src: &str,
    mut options: LarkOptions,
    corpus_all: Vec<String>,
    oneshot_hint: Option<Vec<String>>,
    variants: &[VariantFn],
) {
    // The standalone runtime IS the basic lexer, so the oracle is the in-process
    // basic-lexer LALR engine.
    options.lexer = LexerType::Basic;
    let t0 = Instant::now();
    let oracle = Lark::new(grammar_src, options.clone()).expect("in-process oracle builds");
    let inproc_build_ms = t0.elapsed().as_nanos() as f64 / 1e6;

    let n_all = corpus_all.len();
    let corpus: Vec<String> = corpus_all
        .into_iter()
        .filter(|t| oracle.parse(t).is_ok())
        .collect();
    let bytes: usize = corpus.iter().map(|t| t.len()).sum();
    println!(
        "\n== {name}: {} bytes over {} input(s){} ==",
        bytes,
        corpus.len(),
        if corpus.len() < n_all {
            format!(
                " (dropped {} inputs the in-process basic lexer cannot parse)",
                n_all - corpus.len()
            )
        } else {
            String::new()
        }
    );
    assert!(!corpus.is_empty(), "{name}: empty workload");

    // Oracle trees for the tree-identity gate (Display formats are identical
    // between the in-process and standalone tree types by construction).
    let mut oracle_trees = String::new();
    for t in &corpus {
        oracle_trees.push_str(&oracle.parse(t).unwrap().to_string());
        oracle_trees.push('\n');
    }

    // One-shot inputs: the synthetic sweep (json) or smallest+largest corpus input.
    let oneshot_inputs: Vec<String> = oneshot_hint.unwrap_or_else(|| {
        let mut sorted: Vec<&String> = corpus.iter().collect();
        sorted.sort_by_key(|t| t.len());
        let mut v = vec![sorted[0].clone()];
        if sorted.len() > 1 {
            v.push(sorted[sorted.len() - 1].clone());
        }
        v
    });

    // Run every variant, then gate before reporting any number.
    let results: Vec<VariantResult> = variants
        .iter()
        .map(|f| f(&corpus, &oneshot_inputs))
        .collect();
    let stock = &results[0];
    for r in &results {
        assert_eq!(
            r.stream.len(),
            stock.stream.len(),
            "{name}/{}: token count diverges from stock",
            r.label
        );
        assert!(
            r.stream == stock.stream,
            "{name}/{}: token stream diverges from stock — the bake is not faithful",
            r.label
        );
        assert!(
            r.trees == oracle_trees,
            "{name}/{}: parse trees diverge from the in-process oracle",
            r.label
        );
    }
    let nodes = oracle_trees.matches("Tree(").count();
    println!(
        "  gates OK: {} tokens, {} tree nodes — all variants stream-identical to stock and \
         tree-identical to the in-process oracle",
        stock.stream.len(),
        nodes
    );

    // In-process context rows.
    let inproc_parse_ns = measure(|| {
        for t in &corpus {
            black_box(oracle.parse(black_box(t)).unwrap());
        }
    });
    let mbps = |ns: f64| bytes as f64 / ns * 1e3;
    println!(
        "  in-process context: Lark::new {:.2} ms; reused parse {:.1} MB/s",
        inproc_build_ms,
        mbps(inproc_parse_ns)
    );

    // The table.
    println!(
        "  {:<9} {:>12} {:>13} {:>11} {:>10} {:>12} {:>11}",
        "variant", "new (ms)", "parse MB/s", "lex MB/s", "scan %", "allocs", "alloc MB"
    );
    for r in &results {
        println!(
            "  {:<9} {:>12.3} {:>10.1} ({:.2}x) {:>11.1} {:>9.0}% {:>12} {:>11.1}  \
             [BENCH\tsa_bake\t{name}/{}\t{bytes}\t{:.0}\t{:.0}\t{:.0}]",
            r.label,
            r.new_ns / 1e6,
            mbps(r.parse_ns),
            stock.parse_ns / r.parse_ns,
            mbps(r.lex_ns),
            100.0 * r.lex_ns / r.parse_ns,
            r.allocs,
            r.alloc_bytes as f64 / 1e6,
            r.label,
            r.new_ns,
            r.parse_ns,
            r.lex_ns,
        );
    }

    // One-shot sweep: Parser::new()+parse per input size, per variant; in-process
    // Lark::new()+parse for context.
    println!("  one-shot (build + first parse), per input:");
    for (i, t) in oneshot_inputs.iter().enumerate() {
        let inproc = measure(|| {
            let l = Lark::new(grammar_src, options.clone()).unwrap();
            black_box(l.parse(black_box(t)).unwrap());
        });
        let cols: Vec<String> = results
            .iter()
            .map(|r| format!("{} {:>9.3} ms", r.label, r.oneshot_ns[i] / 1e6))
            .collect();
        println!(
            "    {:>9} B: {}  | in-process {:>9.3} ms",
            t.len(),
            cols.join(" | "),
            inproc / 1e6
        );
    }
}

fn main() {
    println!("# standalone-bake spike bench (#620): stock / lean / baked / interned");

    let json_options = LarkOptions {
        start: vec!["start".to_string()],
        parser: ParserAlgorithm::Lalr,
        maybe_placeholders: true,
        keep_all_tokens: false,
        ..Default::default()
    };
    bench_grammar(
        "json",
        JSON_GRAMMAR,
        json_options,
        vec![gen_json(2000, 8)],
        Some(
            [1usize, 10, 100, 1000, 2000]
                .iter()
                .map(|r| gen_json(*r, 8))
                .collect(),
        ),
        &[
            json_stock_run as VariantFn,
            json_lean_run,
            json_baked_run,
            json_interned_run,
        ],
    );

    let (grammar, options, corpus) = wild_case("matter_idl");
    bench_grammar(
        "matter_idl",
        &grammar,
        options,
        corpus,
        None,
        &[
            matter_stock_run as VariantFn,
            matter_lean_run,
            matter_baked_run,
            matter_interned_run,
        ],
    );

    let (grammar, options, corpus) = wild_case("poetry_markers");
    bench_grammar(
        "poetry_markers",
        &grammar,
        options,
        corpus,
        None,
        &[
            markers_stock_run as VariantFn,
            markers_lean_run,
            markers_baked_run,
            markers_interned_run,
        ],
    );
}
