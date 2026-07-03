//! THROWAWAY SPIKE (standalone-bake spike 2026-07-03, issue #620): the
//! **standalone-surface** counterpart of `examples/baked_dfa_lex.rs`. The prior
//! spike proved the baked-DFA lever at the *in-process scanner* seam; #620 says the
//! lever pays on the *generated-parser* surface, whose scanner is a **different
//! engine**: `src/standalone/runtime.rs` compiles a `regex`-crate combined
//! alternation at load, NOT the `regex-automata` DFA the in-process backend uses.
//!
//! This example measures the standalone surface's *scanner* economics in-process,
//! so the ratios are deterministic and re-runnable without compiling a crate per
//! variant (that half — one-shot compile time + binary size — is the sibling
//! `scripts/standalone_bake_compile.sh` over real generated crates).
//!
//! Variants (all drive the SAME determinized automaton by construction; a
//! token-stream differential gates every one against the standalone scanner before
//! anything is timed):
//!
//! * **stock**  — `LexerBackend::Regex` `match_at`: the standalone runtime's own
//!   scanner engine (a `regex`-crate combined alternation + capture groups). The
//!   baseline the bake must beat.
//! * **dfa**    — `LexerBackend::Dfa` `match_at`: the in-process default backend
//!   (`regex-automata` DFA). What the standalone would get by swapping engines at
//!   runtime, no baking (adds a `regex-automata` dep).
//! * **raw**    — leaner `Automaton`-trait drive loop over the same dense DFA
//!   (L5a: no per-position `Input`/prefilter, no baking, no size cost).
//! * **table**  — flat baked `u32[state×256]` opcode table (L5b full bake; needs
//!   neither `regex` nor `regex-automata` for scanning).
//! * **table-cls** — byte-class-compressed table (L5c: `k`-wide rows, one extra
//!   indirection through the class map — measures the footprint/throughput trade).
//!
//! ```text
//! cargo run --release --features baked-dfa-spike --example standalone_bake_spike
//! ```

use std::collections::HashMap;
use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use lark_rs::grammar::load_grammar_with_base;
use lark_rs::{basic_lexer_conf, lower, BasicLexer, Lexer, LexerBackend, SymbolId};
use regex_automata::dfa::{dense, Automaton};
use regex_automata::{Anchored, Input};

// Byte-identical to baked_dfa_lex.rs / benches/lex_backends.rs.
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

// A second all-plain synthetic: an arithmetic-ish grammar (identifiers, numbers,
// operators) — a different terminal profile from JSON (no long string terminal).
const EXPR_GRAMMAR: &str = r#"
    ?start: sum
    ?sum: product (("+"|"-") product)*
    ?product: atom (("*"|"/") atom)*
    ?atom: NUMBER            -> number
         | NAME              -> var
         | "(" sum ")"
    NAME: /[a-zA-Z_]\w*/
    NUMBER: /\d+(\.\d+)?/
    %import common.WS
    %ignore WS
"#;

// ─── Baking (identical flattening to baked_dfa_lex.rs — duplicated, spike) ────

struct Baked {
    trans: Vec<u32>,
    match_pat: Vec<i32>,
    eoi_pat: Vec<i32>,
    n_states: usize,
}

fn bake(dfa: &dense::DFA<Vec<u32>>) -> Option<Baked> {
    let start_sid = dfa
        .start_state_forward(&Input::new("").anchored(Anchored::Yes))
        .ok()?;
    for ctx in [b'a', b'0', b'\n', b' ', b'"', b'.'] {
        let hay = [ctx, b'x'];
        let inp = Input::new(&hay[..]).span(1..2).anchored(Anchored::Yes);
        if dfa.start_state_forward(&inp).ok()? != start_sid {
            return None;
        }
    }
    if dfa.is_dead_state(start_sid) || dfa.is_quit_state(start_sid) {
        return None;
    }
    let mut ids: HashMap<_, u32> = HashMap::new();
    let mut order = vec![start_sid];
    ids.insert(start_sid, 1);
    let mut rows: Vec<[u32; 256]> = Vec::new();
    let mut qi = 0;
    while qi < order.len() {
        let sid = order[qi];
        qi += 1;
        let mut row = [0u32; 256];
        for b in 0..=255u8 {
            let n = dfa.next_state(sid, b);
            if dfa.is_quit_state(n) {
                return None;
            }
            if dfa.is_dead_state(n) {
                continue;
            }
            row[b as usize] = *ids.entry(n).or_insert_with(|| {
                order.push(n);
                order.len() as u32
            });
        }
        rows.push(row);
    }
    let n = order.len() + 1;
    let mut trans = vec![0u32; n * 256];
    let mut match_pat = vec![-1i32; n];
    let mut eoi_pat = vec![-1i32; n];
    for (k, sid) in order.iter().enumerate() {
        let c = k + 1;
        trans[c * 256..(c + 1) * 256].copy_from_slice(&rows[k]);
        if dfa.is_match_state(*sid) {
            match_pat[c] = dfa.match_pattern(*sid, 0).as_usize() as i32;
        }
        let e = dfa.next_eoi_state(*sid);
        if dfa.is_match_state(e) {
            eoi_pat[c] = dfa.match_pattern(e, 0).as_usize() as i32;
        }
    }
    Some(Baked {
        trans,
        match_pat,
        eoi_pat,
        n_states: n,
    })
}

/// Byte-class-compressed table (L5c): the flat 256-wide `trans` folded onto the
/// DFA's own equivalence classes. `class[b]` maps a byte to its class column, so
/// each state row is `n_classes` wide instead of 256 — one extra load per byte.
struct BakedCls {
    trans: Vec<u32>,
    class: [u8; 256],
    n_classes: usize,
    match_pat: Vec<i32>,
    eoi_pat: Vec<i32>,
}

fn bake_classes(dfa: &dense::DFA<Vec<u32>>, flat: &Baked) -> BakedCls {
    let bc = dfa.byte_classes();
    let mut class = [0u8; 256];
    let mut n_classes = 0usize;
    for b in 0..=255u8 {
        let c = bc.get(b);
        class[b as usize] = c;
        n_classes = n_classes.max(c as usize + 1);
    }
    let mut trans = vec![0u32; flat.n_states * n_classes];
    for st in 0..flat.n_states {
        for b in 0..256usize {
            let c = class[b] as usize;
            // Every byte in a class maps to the same next state, so last-writer is safe.
            trans[st * n_classes + c] = flat.trans[(st << 8) + b];
        }
    }
    BakedCls {
        trans,
        class,
        n_classes,
        match_pat: flat.match_pat.clone(),
        eoi_pat: flat.eoi_pat.clone(),
    }
}

// ─── Per-position matchers ────────────────────────────────────────────────────

fn raw_match_at(dfa: &dense::DFA<Vec<u32>>, text: &str, pos: usize) -> (i32, usize) {
    let bytes = text.as_bytes();
    let input = Input::new(text)
        .span(pos..bytes.len())
        .anchored(Anchored::Yes);
    let Ok(mut state) = dfa.start_state_forward(&input) else {
        return (-1, pos);
    };
    let mut bp: i32 = -1;
    let mut be = pos;
    let mut i = pos;
    while i < bytes.len() {
        state = dfa.next_state(state, bytes[i]);
        if dfa.is_special_state(state) {
            if dfa.is_match_state(state) {
                if i > pos {
                    bp = dfa.match_pattern(state, 0).as_usize() as i32;
                    be = i;
                }
            } else if dfa.is_dead_state(state) {
                return (bp, be);
            }
        }
        i += 1;
    }
    let e = dfa.next_eoi_state(state);
    if dfa.is_match_state(e) && bytes.len() > pos {
        bp = dfa.match_pattern(e, 0).as_usize() as i32;
        be = bytes.len();
    }
    (bp, be)
}

fn table_match_at(t: &Baked, bytes: &[u8], pos: usize) -> (i32, usize) {
    let len = bytes.len();
    let mut st = 1usize;
    let mut bp: i32 = -1;
    let mut be = pos;
    let mut i = pos;
    while i < len {
        st = t.trans[(st << 8) + bytes[i] as usize] as usize;
        let m = t.match_pat[st];
        if m >= 0 {
            if i > pos {
                bp = m;
                be = i;
            }
        } else if st == 0 {
            return (bp, be);
        }
        i += 1;
    }
    let m = t.eoi_pat[st];
    if m >= 0 && len > pos {
        bp = m;
        be = len;
    }
    (bp, be)
}

fn table_cls_match_at(t: &BakedCls, bytes: &[u8], pos: usize) -> (i32, usize) {
    let len = bytes.len();
    let nc = t.n_classes;
    let mut st = 1usize;
    let mut bp: i32 = -1;
    let mut be = pos;
    let mut i = pos;
    while i < len {
        let c = t.class[bytes[i] as usize] as usize;
        st = t.trans[st * nc + c] as usize;
        let m = t.match_pat[st];
        if m >= 0 {
            if i > pos {
                bp = m;
                be = i;
            }
        } else if st == 0 {
            return (bp, be);
        }
        i += 1;
    }
    let m = t.eoi_pat[st];
    if m >= 0 && len > pos {
        bp = m;
        be = len;
    }
    (bp, be)
}

// ─── Drivers ──────────────────────────────────────────────────────────────────

fn bump_char(text: &str, pos: usize) -> usize {
    let mut p = pos + 1;
    while p < text.len() && !text.is_char_boundary(p) {
        p += 1;
    }
    p
}

#[inline]
fn fold(h: u64, id: u32, end: usize) -> u64 {
    h.wrapping_mul(0x100000001b3)
        .wrapping_add(((id as u64) << 32) ^ end as u64)
}

/// A `BasicLexer::match_at` seam (works for both the Regex and Dfa backends).
fn drive_seam(lexer: &BasicLexer, text: &str) -> (u64, u64) {
    let (mut h, mut n, mut pos) = (0u64, 0u64, 0usize);
    while pos < text.len() {
        match lexer.match_at(text, pos) {
            Some((id, val)) => {
                let end = pos + val.len();
                h = fold(h, id.index() as u32, end);
                n += 1;
                pos = end;
            }
            None => {
                h = fold(h, u32::MAX, pos);
                pos = bump_char(text, pos);
            }
        }
    }
    (h, n)
}

/// Pattern-returning variants: map pattern → terminal id and re-apply the seam's
/// `unless` retype (via the Dfa-backed lexer) so the stream is seam-identical.
fn drive_patterns<F: FnMut(&[u8], usize) -> (i32, usize)>(
    retyper: &BasicLexer,
    ids: &[SymbolId],
    text: &str,
    mut match_at: F,
) -> (u64, u64) {
    let bytes = text.as_bytes();
    let (mut h, mut n, mut pos) = (0u64, 0u64, 0usize);
    while pos < text.len() {
        let (pat, end) = match_at(bytes, pos);
        if pat >= 0 && end > pos {
            let id = retyper.spike_retype(ids[pat as usize], &text[pos..end]);
            h = fold(h, id.index() as u32, end);
            n += 1;
            pos = end;
        } else {
            h = fold(h, u32::MAX, pos);
            pos = bump_char(text, pos);
        }
    }
    (h, n)
}

// ─── Timing ───────────────────────────────────────────────────────────────────

struct Stat {
    min_ns: f64,
    median_ns: f64,
}

fn measure<F: FnMut()>(mut f: F) -> Stat {
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
    while samples.len() < 50 && overall.elapsed() < Duration::from_millis(1500) {
        let t = Instant::now();
        for _ in 0..iters {
            f();
        }
        samples.push(t.elapsed().as_nanos() as f64 / iters as f64);
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    Stat {
        min_ns: samples[0],
        median_ns: samples[samples.len() / 2],
    }
}

/// Median wall-clock (ns) of building `BasicLexer::new` for a given backend —
/// the load cost. Re-measured a few times; report the min (least-noise) build.
fn build_cost(conf_backend: LexerBackend, mk: &dyn Fn(LexerBackend) -> Option<BasicLexer>) -> f64 {
    let mut best = f64::INFINITY;
    for _ in 0..9 {
        let t = Instant::now();
        let l = mk(conf_backend);
        let ns = t.elapsed().as_nanos() as f64;
        black_box(&l);
        best = best.min(ns);
    }
    best
}

// ─── Workloads ────────────────────────────────────────────────────────────────

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

fn gen_expr(terms: usize) -> String {
    let mut s = String::new();
    for i in 0..terms {
        if i > 0 {
            s.push_str(" + ");
        }
        s.push_str(&format!("foo_{i} * 12 + (bar{i} - 3.5)"));
    }
    s
}

/// The comparison for one grammar over one corpus. `mk_lexer` builds a fresh
/// `BasicLexer` for a chosen backend (so the load cost is measured per backend).
fn run_grammar(name: &str, grammar_src: &str, base: Option<PathBuf>, texts: &[String]) {
    let g = match load_grammar_with_base(grammar_src, &["start".to_string()], true, false, base) {
        Ok(g) => g,
        Err(_) => {
            println!("\n{name}: SKIP — grammar does not build (wild xfail)");
            return;
        }
    };
    let cg = lower(&g);
    let mk = |backend: LexerBackend| -> Option<BasicLexer> {
        let conf = basic_lexer_conf(&cg, 0).with_backend(backend);
        BasicLexer::new(&conf).ok()
    };

    // Both backends must build; a lookaround/scope refusal here is exactly what the
    // standalone bake would reject too (the SKIP taxonomy).
    let (Some(stock), Some(dfa_lexer)) = (mk(LexerBackend::Regex), mk(LexerBackend::Dfa)) else {
        println!("\n{name}: SKIP — basic lexer refuses to build (lookaround/scope — un-bakeable)");
        return;
    };

    let Some((dfa, ids)) = dfa_lexer.spike_plain_dense() else {
        println!("\n{name}: SKIP — scanner is not a single dense plain engine");
        return;
    };
    let ids: &'static [SymbolId] = ids.leak();

    let Some(baked) = bake(dfa) else {
        println!("\n{name}: SKIP — DFA does not bake (context-sensitive start / quit states)");
        return;
    };
    let baked_cls = bake_classes(dfa, &baked);

    let bytes: usize = texts.iter().map(|t| t.len()).sum();

    // ── Load costs (one-shot column) ──
    let regex_build = build_cost(LexerBackend::Regex, &mk);
    let dfa_build = build_cost(LexerBackend::Dfa, &mk);
    let bake_ns = {
        let mut best = f64::INFINITY;
        for _ in 0..9 {
            let t = Instant::now();
            let b = bake(dfa).unwrap();
            best = best.min(t.elapsed().as_nanos() as f64);
            black_box(&b);
        }
        best
    };

    // ── Footprint (L5c) ──
    let flat_bytes = baked.n_states * 256 * 4;
    let cls_bytes = baked.n_states * baked_cls.n_classes * 4 + 256; // + class map
    let (dfa_ser, _pad) = dfa.to_bytes_native_endian();
    let to_bytes = dfa_ser.len();

    println!(
        "\n══ {name}: {bytes} bytes over {} input(s) ══",
        texts.len()
    );
    println!(
        "  scanners for this grammar (standalone basic lexer): 1  \
         (contextual would be N deduped per-state scanners — standalone uses none)"
    );
    println!(
        "  load    stock(regex) {:.3} ms | dfa-engine {:.3} ms | bake-flatten {:.3} ms",
        regex_build / 1e6,
        dfa_build / 1e6,
        bake_ns / 1e6
    );
    println!(
        "  footprint  baked states {} | flat 256-wide {} KiB | byte-class {}-wide {} KiB | \
         regex-automata to_bytes {} KiB",
        baked.n_states,
        flat_bytes / 1024,
        baked_cls.n_classes,
        cls_bytes / 1024,
        to_bytes / 1024,
    );

    // ── Differential (correctness gate) ──
    let mut streams: Vec<(&str, u64, u64)> = Vec::new();
    let sum = |f: &dyn Fn(&str) -> (u64, u64)| {
        let (mut h, mut n) = (0u64, 0u64);
        for t in texts {
            let (hh, nn) = f(t);
            h = h.wrapping_add(hh);
            n += nn;
        }
        (h, n)
    };
    let (h0, n0) = sum(&|t| drive_seam(&stock, t));
    streams.push(("stock", h0, n0));
    for (vname, hn) in [
        ("dfa", sum(&|t| drive_seam(&dfa_lexer, t))),
        (
            "raw",
            sum(&|t| {
                drive_patterns(&dfa_lexer, ids, t, |b, p| {
                    raw_match_at(dfa, unsafe { std::str::from_utf8_unchecked(b) }, p)
                })
            }),
        ),
        (
            "table",
            sum(&|t| drive_patterns(&dfa_lexer, ids, t, |b, p| table_match_at(&baked, b, p))),
        ),
        (
            "table-cls",
            sum(&|t| {
                drive_patterns(&dfa_lexer, ids, t, |b, p| {
                    table_cls_match_at(&baked_cls, b, p)
                })
            }),
        ),
    ] {
        streams.push((vname, hn.0, hn.1));
    }
    for (vname, h, n) in &streams {
        assert_eq!(
            (*h, *n),
            (h0, n0),
            "{name}/{vname}: token stream diverges from the standalone scanner — bake not faithful"
        );
    }
    println!("  differential OK: all variants emit the standalone scanner's stream ({n0} tokens)");

    // ── Reused scan throughput (ratios vs stock) ──
    let mbps = |ns: f64| bytes as f64 / ns * 1e3;
    let time_seam = |lx: &BasicLexer| {
        measure(|| {
            for t in texts {
                black_box(drive_seam(lx, black_box(t)));
            }
        })
    };
    let s_stock = time_seam(&stock);
    let s_dfa = time_seam(&dfa_lexer);
    let time_pat = |f: &dyn Fn(&str) -> (u64, u64)| {
        measure(|| {
            for t in texts {
                black_box(f(black_box(t)));
            }
        })
    };
    let s_raw = time_pat(&|t| {
        drive_patterns(&dfa_lexer, ids, t, |b, p| {
            raw_match_at(dfa, unsafe { std::str::from_utf8_unchecked(b) }, p)
        })
    });
    let s_table =
        time_pat(&|t| drive_patterns(&dfa_lexer, ids, t, |b, p| table_match_at(&baked, b, p)));
    let s_cls = time_pat(&|t| {
        drive_patterns(&dfa_lexer, ids, t, |b, p| {
            table_cls_match_at(&baked_cls, b, p)
        })
    });

    let base = s_stock.median_ns;
    let row = |label: &str, s: &Stat| {
        println!(
            "  {:<10} {:>10.0} ns (min {:>10.0})  {:>7.1} MB/s  {:>5.2}x  \
             [BENCH\tstandalone_bake\t{name}/{label}\t{bytes}\t{:.0}\t{:.0}\t{:.1}]",
            label,
            s.median_ns,
            s.min_ns,
            mbps(s.median_ns),
            base / s.median_ns,
            s.median_ns,
            s.min_ns,
            mbps(s.median_ns),
        );
    };
    row("stock", &s_stock);
    row("dfa", &s_dfa);
    row("raw", &s_raw);
    row("table", &s_table);
    row("table-cls", &s_cls);

    // ── Scan share of full standalone-style lex (owned Token materialization) ──
    if texts.iter().all(|t| stock.lex(t).is_ok()) {
        let full = measure(|| {
            for t in texts {
                black_box(stock.lex(black_box(t)).unwrap());
            }
        });
        println!(
            "  lex-full   {:>10.0} ns (min {:>10.0})  {:>7.1} MB/s   (stock scan is {:.0}% of full lex)",
            full.median_ns,
            full.min_ns,
            mbps(full.median_ns),
            100.0 * s_stock.median_ns / full.median_ns,
        );
    }
}

// ─── Wild bank ────────────────────────────────────────────────────────────────

fn wild_grammar(pdir: &Path) -> Option<(String, String, PathBuf, Vec<String>)> {
    let meta: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(pdir.join("meta.json")).ok()?).ok()?;
    let name = meta["name"].as_str()?.to_string();
    let o = &meta["lark_options"];
    if o["parser"].as_str() != Some("lalr") {
        return None;
    }
    if o["g_regex_flags"]
        .as_str()
        .map(|s| !s.is_empty())
        .unwrap_or(false)
    {
        return None; // keep the spike to the zero-global-flag path
    }
    let grammar = std::fs::read_to_string(pdir.join(meta["entry_grammar"].as_str()?)).ok()?;
    let inputs: Vec<String> = meta["inputs"]
        .as_object()?
        .keys()
        .filter_map(|rel| std::fs::read_to_string(pdir.join(rel)).ok())
        .collect();
    Some((name, grammar, pdir.join("grammar"), inputs))
}

fn main() {
    println!("# standalone-surface bake spike (#620): scanner economics of a generated parser");
    println!("# stock=regex combined alt (what ships) | dfa=regex-automata engine | raw=leaner loop (L5a)");
    println!("# table=flat 256-wide bake (L5b) | table-cls=byte-class-compressed bake (L5c)");

    // JSON size sweep.
    for (tag, records) in [("json_56k", 200), ("json_594k", 2000)] {
        run_grammar(tag, JSON_GRAMMAR, None, &[gen_json(records, 8)]);
    }
    // Expr (different terminal profile).
    run_grammar("expr_30k", EXPR_GRAMMAR, None, &[gen_expr(1200)]);

    // Wild bank: every LALR project whose basic-lexer scanner bakes cleanly.
    let wild = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/wild");
    let mut projects: Vec<PathBuf> = std::fs::read_dir(&wild)
        .expect("tests/wild exists")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.join("meta.json").is_file())
        .collect();
    projects.sort();
    println!("\n# wild bank (standalone basic lexer over each project's full terminal set)");
    for pdir in &projects {
        if let Some((name, grammar, base, inputs)) = wild_grammar(pdir) {
            if inputs.is_empty() {
                continue;
            }
            // Guard: only run if the grammar builds a plain dense scanner (the
            // run_grammar SKIP path prints otherwise).
            run_grammar(&name, &grammar, Some(base), &inputs);
        }
    }
}
