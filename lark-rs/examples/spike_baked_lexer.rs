//! SPIKE (throwaway, `docs/notes/parser-optimization-research-2026-07.md`
//! experiment 1 — the top-ranked lever, B1: "directly-executable / baked DFA
//! lexer"). Not a gate, not wired into any feature flag: this example exists only
//! to produce the before/after number and is safe to delete once the finding is
//! written up (`docs/notes/spike-parser-optimization-2026-07-03.md`).
//!
//! ## Hypothesis
//!
//! RE/flex's `--fast`/`--full` modes bake a DFA into hand-generated goto/switch
//! code (or a static opcode table) instead of interpreting a generic transition
//! table at match time, reportedly up to ~7x over a table-interpreted lexer
//! (Pfahler 1990, not independently verified — see the research note). lark-rs's
//! default lexer (`DfaScanner`, `src/lexer/dfa.rs`) drives a `regex-automata`
//! `dense::DFA` through the generic `Automaton` trait: every byte does an
//! equivalence-class lookup, then a transition-table index, through a trait call.
//! Would a **hand-written, grammar-specific** byte-at-a-time scanner (the
//! "--fast" goto/switch style — a `match` on the current byte, which rustc
//! compiles to a jump table with no table-indirection or class-remapping) beat it
//! on real throughput?
//!
//! ## Method
//!
//! A hand-rolled scanner for exactly the JSON grammar's terminal set (the same
//! grammar `benches/lex_backends.rs` and `examples/child_vec_alloc.rs` use), doing
//! direct byte comparisons — no `regex-automata`, no table, no capture groups.
//! Correctness is checked (not just assumed) against the real `DfaScanner` by
//! requiring the baked scanner to consume the **entire** generated input with no
//! gap (`pos == len` at the end) on every input size tested.
//!
//! Per BENCH.md discipline: reused-lexer throughput (the workload lark-rs's own
//! headline use targets — build once, lex many) is reported separately from
//! one-shot build cost (baking makes build cost ~0, trivially — the interesting
//! number is whether the *reused* throughput actually moves).
//!
//! ```text
//! cargo run --release --example spike_baked_lexer
//! ```

use std::hint::black_box;
use std::time::{Duration, Instant};

use lark_rs::{basic_lexer_conf, load_grammar, lower, BasicLexer, Lexer, LexerBackend};

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

fn build_dfa_lexer() -> BasicLexer {
    let g = load_grammar(JSON_GRAMMAR, &["start".to_string()], true, false)
        .expect("benchmark grammar must load");
    let cg = lower(&g);
    let conf = basic_lexer_conf(&cg, 0).with_backend(LexerBackend::Dfa);
    BasicLexer::new(&conf).expect("benchmark lexer must build")
}

// ─── The baked scanner: a hand-written goto/switch-style byte matcher ─────────
//
// Directly implements the JSON grammar's terminal set (see common.lark for
// SIGNED_NUMBER/ESCAPED_STRING's exact shape) as plain Rust byte comparisons: no
// table, no equivalence classes, no capture groups, no trait dispatch — the
// "--fast" style RE/flex bakes. This is deliberately grammar-specific (the
// research note: "a quick-and-dirty ... table for ONE grammar is enough to get a
// directional number — you don't need a general emitter").

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
enum Tok {
    LBracket,
    RBracket,
    LBrace,
    RBrace,
    Comma,
    Colon,
    True,
    False,
    Null,
    Str,
    Num,
    Ws,
}

#[inline]
fn match_keyword(b: &[u8], pos: usize, kw: &[u8], tok: Tok) -> Option<(Tok, usize)> {
    let end = pos + kw.len();
    if b.len() >= end && &b[pos..end] == kw {
        Some((tok, end))
    } else {
        None
    }
}

/// ESCAPED_STRING: `"` ([^"\\\n] | \\.)* `"` — see `common.lark`.
#[inline]
fn scan_string(b: &[u8], start: usize) -> Option<(Tok, usize)> {
    let mut i = start + 1;
    loop {
        let c = *b.get(i)?;
        match c {
            b'"' => return Some((Tok::Str, i + 1)),
            b'\n' => return None,
            b'\\' if i + 1 < b.len() => i += 2,
            _ => i += 1,
        }
    }
}

/// SIGNED_NUMBER: `["+"|"-"] (FLOAT|INT)`, `FLOAT: INT _EXP | DECIMAL _EXP?`,
/// `DECIMAL: INT "." INT? | "." INT`, `_EXP: ("e"|"E") ["+"|"-"] INT` — see
/// `common.lark`. Greedy throughout, matching the regex engine's `+`/`?`.
#[inline]
fn scan_number(b: &[u8], start: usize) -> Option<(Tok, usize)> {
    let mut i = start;
    if matches!(b.get(i), Some(b'+') | Some(b'-')) {
        i += 1;
    }
    let int_start = i;
    while matches!(b.get(i), Some(c) if c.is_ascii_digit()) {
        i += 1;
    }
    let has_int = i > int_start;
    let mut has_frac = false;

    if has_int && b.get(i) == Some(&b'.') {
        // DECIMAL: INT "." INT?  (fractional part optional once INT matched)
        let mut j = i + 1;
        while matches!(b.get(j), Some(c) if c.is_ascii_digit()) {
            j += 1;
        }
        i = j;
        has_frac = true;
    } else if !has_int && b.get(i) == Some(&b'.') {
        // DECIMAL: "." INT  (fractional part mandatory with no leading int)
        let frac_start = i + 1;
        let mut j = frac_start;
        while matches!(b.get(j), Some(c) if c.is_ascii_digit()) {
            j += 1;
        }
        if j > frac_start {
            i = j;
            has_frac = true;
        } else {
            return None;
        }
    }

    if !has_int && !has_frac {
        return None; // a lone sign, or nothing at all
    }

    // _EXP: ("e"|"E") SIGNED_INT — only consumed if well-formed (digits present).
    if matches!(b.get(i), Some(b'e') | Some(b'E')) {
        let mut k = i + 1;
        if matches!(b.get(k), Some(b'+') | Some(b'-')) {
            k += 1;
        }
        let exp_digits_start = k;
        while matches!(b.get(k), Some(c) if c.is_ascii_digit()) {
            k += 1;
        }
        if k > exp_digits_start {
            i = k;
        }
    }
    Some((Tok::Num, i))
}

/// One token starting exactly at `pos`, or `None` if nothing matches there — the
/// baked analogue of `DfaScanner::match_at`. A `match` on the first byte is
/// exactly the goto/switch dispatch RE/flex's `--fast` mode emits; rustc compiles
/// a dense byte `match` like this to a jump table, not a chain of comparisons.
#[inline]
fn baked_match(b: &[u8], pos: usize) -> Option<(Tok, usize)> {
    match *b.get(pos)? {
        b'[' => Some((Tok::LBracket, pos + 1)),
        b']' => Some((Tok::RBracket, pos + 1)),
        b'{' => Some((Tok::LBrace, pos + 1)),
        b'}' => Some((Tok::RBrace, pos + 1)),
        b',' => Some((Tok::Comma, pos + 1)),
        b':' => Some((Tok::Colon, pos + 1)),
        b' ' | b'\t' | 0x0c | b'\r' | b'\n' => {
            let mut e = pos + 1;
            while matches!(
                b.get(e),
                Some(b' ') | Some(b'\t') | Some(0x0c) | Some(b'\r') | Some(b'\n')
            ) {
                e += 1;
            }
            Some((Tok::Ws, e))
        }
        b't' => match_keyword(b, pos, b"true", Tok::True),
        b'f' => match_keyword(b, pos, b"false", Tok::False),
        b'n' => match_keyword(b, pos, b"null", Tok::Null),
        b'"' => scan_string(b, pos),
        b'0'..=b'9' | b'+' | b'-' | b'.' => scan_number(b, pos),
        _ => None,
    }
}

/// Lex the whole input, returning the token count. Panics (a hard test failure,
/// not a benchmark artifact) if the baked scanner can't consume the entire input —
/// the correctness check that makes the throughput number trustworthy.
fn baked_lex(input: &str) -> usize {
    let b = input.as_bytes();
    let mut pos = 0;
    let mut count = 0usize;
    while pos < b.len() {
        let (_, end) = baked_match(b, pos)
            .unwrap_or_else(|| panic!("baked scanner: no match at byte {pos} ({:?})", input));
        debug_assert!(end > pos, "baked scanner: zero-width match at {pos}");
        pos = end;
        count += 1;
    }
    count
}

/// The DFA-side analogue of `baked_lex`: drive `BasicLexer::match_at` (the raw
/// per-position scanner seam, no `Token` construction, no line/col/char-index
/// bookkeeping) in the same loop shape as `baked_lex`. This — not `Lexer::lex`,
/// which additionally builds an owned `Token` (two `String` allocations) and
/// walks every matched value char-by-char for position tracking — is the fair
/// apples-to-apples comparison: both sides do *exactly* scanning, nothing else.
/// (An earlier draft of this spike compared `baked_lex` against `dfa.lex()` and
/// got a misleadingly huge ratio — see the findings note for the correction.)
fn dfa_scan_all(lexer: &BasicLexer, input: &str) -> usize {
    let mut pos = 0;
    let mut count = 0usize;
    while pos < input.len() {
        match lexer.match_at(input, pos) {
            Some((_, value)) => {
                debug_assert!(!value.is_empty(), "dfa scanner: zero-width match at {pos}");
                pos += value.len();
                count += 1;
            }
            None => panic!("dfa scanner: no match at byte {pos}"),
        }
    }
    count
}

// ─── Measurement harness (same shape as benches/lex_backends.rs) ─────────────

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

fn row(name: &str, bytes: usize, stat: &Stat) -> f64 {
    let mb_per_s = bytes as f64 / stat.median_ns * 1e3;
    println!(
        "BENCH\tbaked_lex\t{name}\t{bytes}\t{:.0}\t{:.0}\t{mb_per_s:.1}",
        stat.median_ns, stat.min_ns
    );
    println!(
        "  {name:<12} {bytes:>8} B   {:>10.0} ns/iter (min {:>10.0})   {mb_per_s:>7.1} MB/s",
        stat.median_ns, stat.min_ns
    );
    mb_per_s
}

fn main() {
    println!("# spike: baked/hand-rolled JSON scanner vs the regex-automata DfaScanner");
    println!("# hypothesis: docs/notes/parser-optimization-research-2026-07.md, lever B1");
    println!();

    // ── One-shot: build cost. Baking is a compile-time artifact (a plain Rust
    //    function — zero runtime construction), so this side of the ledger is
    //    trivially ~0 by construction; report the DFA side for the record. ──────
    let build_stat = measure(|| {
        black_box(build_dfa_lexer());
    });
    println!("── One-shot build cost ──");
    println!(
        "  DfaScanner::build   {:>10.0} ns/iter (min {:>10.0})",
        build_stat.median_ns, build_stat.min_ns
    );
    println!("  baked scanner       ~0 ns  (a plain fn — no runtime construction at all)");
    println!();

    // ── Reused-lexer throughput: build once, scan many (lark-rs's headline use).
    //    Scanner-only on BOTH sides (match_at / baked_match) — no Token
    //    construction, no allocation, on either side. This is the fair number. ──
    println!("── Reused-lexer throughput, scanner-only (build once, scan N times) ──");
    let dfa = build_dfa_lexer();
    for (name, records, fields) in [
        ("json_small", 4, 3),
        ("json_medium", 64, 4),
        ("json_large", 512, 5),
    ] {
        let input = gen_json(records, fields);
        let bytes = input.len();

        // Correctness check: both scanners must consume the whole input, and
        // agree on how many tokens (incl. ignored whitespace) that took.
        let baked_tokens = baked_lex(&input);
        let dfa_tokens = dfa_scan_all(&dfa, &input);
        assert_eq!(
            baked_tokens, dfa_tokens,
            "{name}: baked and dfa scanners disagree on token count"
        );
        println!("  {name}: both scanners consumed all {bytes} bytes as {baked_tokens} tokens");

        let dfa_stat = measure(|| {
            black_box(dfa_scan_all(&dfa, black_box(&input)));
        });
        let baked_stat = measure(|| {
            black_box(baked_lex(black_box(&input)));
        });
        let dfa_mbps = row(&format!("{name}_dfa"), bytes, &dfa_stat);
        let baked_mbps = row(&format!("{name}_baked"), bytes, &baked_stat);
        println!(
            "  ratio {name:<12} baked/dfa = {:>5.2}x   ({})\n",
            baked_mbps / dfa_mbps,
            if baked_mbps > dfa_mbps {
                "baked faster"
            } else {
                "dfa faster"
            }
        );
    }

    // ── For context only, NOT compared to baked: the full `Lexer::lex()` cost
    //    (Token construction: two String allocs/token + char-by-char position
    //    tracking) on the DFA side — this is what benches/lex_backends.rs times,
    //    and is a different, larger workload than the scanner-only numbers above.
    println!("── For context: full Lexer::lex() cost, DFA side only (not vs baked) ──");
    for (name, records, fields) in [("json_medium", 64, 4), ("json_large", 512, 5)] {
        let input = gen_json(records, fields);
        let bytes = input.len();
        let stat = measure(|| {
            black_box(dfa.lex(black_box(&input)).expect("must lex"));
        });
        row(&format!("{name}_dfa_full_lex"), bytes, &stat);
    }
}
