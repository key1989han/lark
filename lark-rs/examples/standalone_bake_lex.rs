//! THROWAWAY SPIKE (L5 standalone-bake spike, 2026-07-03): the scanner-representation
//! throughput ladder **on the standalone surface** for issue #620. Unlike the
//! 2026-07-03 in-process spike (`baked_dfa_lex.rs`, baseline = the DFA *seam*), the
//! baseline here is the scanner the standalone runtime ACTUALLY ships:
//! `runtime::Scanner` — a `regex`-crate combined alternation compiled at load
//! (`Regex::new` + `captures_at`). That is the "before" #620's L5b improves.
//!
//! Variants, all driving the SAME terminal set / plan (differential-gated to emit the
//! byte-identical `(id, end)` stream before timing):
//!
//!   * **rx**       — regex-crate combined alternation = the stock standalone scanner
//!                    (`runtime::Scanner`, reproduced here from the same `scanner_plan`).
//!   * **seam**     — in-process `BasicLexer::match_at` (regex-automata DFA), for context.
//!   * **raw**      — hand `Automaton`-trait loop over the dense DFA (L5a leaner loop,
//!                    but note: for standalone this is ALREADY a representation change,
//!                    since stock standalone is `rx`, not a DFA).
//!   * **table**    — flat baked `u32[state*256]` interpreter (L5b, RE/flex `--full`).
//!   * **table-uc** — same, unchecked indexing (codegen ceiling).
//!   * **classed**  — byte-class-compressed `u32[state*classes]` + 256-byte class map
//!                    (L5c footprint variant — one extra indirection per byte).
//!
//! One-shot column = the build each rep pays: `rx` = `Regex::new`; `raw`/`table`/
//! `classed` = dense-DFA determinization (`BasicLexer::new`) + (for the baked ones) the
//! flatten; a *shipped* baked table pays neither at load (the table is `static`).
//!
//! ```text
//! cargo run --release --features baked-dfa-spike --example standalone_bake_lex
//! ```

use std::collections::HashMap;
use std::hint::black_box;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use lark_rs::grammar::load_grammar_with_base;
use lark_rs::grammar::terminal::TerminalDef;
use lark_rs::lexer::{scanner_plan, UnlessEntry};
use lark_rs::{basic_lexer_conf, load_grammar, lower, BasicLexer, Lexer, LexerBackend, SymbolId};
use regex::Regex;
use regex_automata::dfa::{dense, Automaton};
use regex_automata::{Anchored, Input};

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

// ─── the stock standalone scanner (regex crate) — mirror of runtime::Scanner ───

/// Reproduces `standalone::runtime::Scanner` exactly (built from the same
/// `scanner_plan`), so its token stream is the standalone runtime's by construction.
struct RxScanner {
    re: Regex,
    groups: Vec<(u32, usize)>,
    unless: HashMap<u32, (HashMap<String, u32>, Vec<(Regex, u32)>)>,
}

impl RxScanner {
    /// Build from a plan; returns the scanner and the `Regex::new` build cost (ns).
    fn build(
        global_prefix: &str,
        groups: &[(u32, String)],
        unless_in: &[(u32, Vec<(String, bool, u32)>)],
    ) -> (RxScanner, u128) {
        let t0 = Instant::now();
        let mut parts: Vec<String> = Vec::with_capacity(groups.len());
        for (id, rx) in groups {
            parts.push(format!("(?P<g{}>{})", id, rx));
        }
        let pattern = format!("{}{}", global_prefix, parts.join("|"));
        let re = Regex::new(&pattern).expect("baked scanner regex is valid");
        let build_ns = t0.elapsed().as_nanos();
        let name_to_idx: HashMap<String, usize> = re
            .capture_names()
            .enumerate()
            .filter_map(|(i, n)| n.map(|n| (n.to_string(), i)))
            .collect();
        let groups = groups
            .iter()
            .map(|(id, _)| (*id, name_to_idx[&format!("g{}", id)]))
            .collect();
        let mut unless: HashMap<u32, (HashMap<String, u32>, Vec<(Regex, u32)>)> = HashMap::new();
        for (re_id, entries) in unless_in {
            let slot = unless.entry(*re_id).or_default();
            for (value, ci, kw_id) in entries {
                if *ci {
                    let src = format!("^(?i:{})$", regex::escape(value));
                    let re = Regex::new(&src).expect("baked unless regex is valid");
                    slot.1.push((re, *kw_id));
                } else {
                    slot.0.entry(value.clone()).or_insert(*kw_id);
                }
            }
        }
        (RxScanner { re, groups, unless }, build_ns)
    }

    fn match_at<'t>(&self, text: &'t str, pos: usize) -> Option<(u32, &'t str)> {
        let caps = self.re.captures_at(text, pos)?;
        let m0 = caps.get(0)?;
        if m0.start() != pos || m0.end() == pos {
            return None;
        }
        let value = m0.as_str();
        for (id, idx) in &self.groups {
            if caps.get(*idx).is_some() {
                let ty = self
                    .unless
                    .get(id)
                    .and_then(|(exact, ci)| {
                        exact.get(value).copied().or_else(|| {
                            ci.iter()
                                .find(|(re, _)| re.is_match(value))
                                .map(|(_, k)| *k)
                        })
                    })
                    .unwrap_or(*id);
                return Some((ty, value));
            }
        }
        None
    }
}

// ─── DFA flatten (same as baked_dfa_lex::bake) + byte-class compression ───────

struct Baked {
    trans: Vec<u32>,
    match_pat: Vec<i32>,
    eoi_pat: Vec<i32>,
    n_states: usize,
    // byte-class-compressed form
    class_of: [u8; 256],
    n_bclasses: usize,
    ctrans: Vec<u32>, // [state * n_bclasses + class]
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
    // Byte-class compression: bytes in one dense-DFA equivalence class share every
    // transition, so collapse the 256-wide row to one column per class.
    let bc = dfa.byte_classes();
    let mut class_of = [0u8; 256];
    let mut n_bclasses = 0usize;
    for b in 0..=255u8 {
        let cl = bc.get(b);
        class_of[b as usize] = cl;
        n_bclasses = n_bclasses.max(cl as usize + 1);
    }
    let mut ctrans = vec![0u32; n * n_bclasses];
    for st in 0..n {
        for b in 0..256usize {
            let cl = class_of[b] as usize;
            ctrans[st * n_bclasses + cl] = trans[st * 256 + b];
        }
    }
    Some(Baked {
        trans,
        match_pat,
        eoi_pat,
        n_states: n,
        class_of,
        n_bclasses,
        ctrans,
    })
}

fn raw_match_at(dfa: &dense::DFA<Vec<u32>>, bytes: &[u8], pos: usize) -> (i32, usize) {
    let input = Input::new(bytes)
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
    let (mut bp, mut be, mut i) = (-1i32, pos, pos);
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

fn table_match_at_unchecked(t: &Baked, bytes: &[u8], pos: usize) -> (i32, usize) {
    let len = bytes.len();
    let mut st = 1usize;
    let (mut bp, mut be, mut i) = (-1i32, pos, pos);
    while i < len {
        unsafe {
            st = *t
                .trans
                .get_unchecked((st << 8) + *bytes.get_unchecked(i) as usize)
                as usize;
            let m = *t.match_pat.get_unchecked(st);
            if m >= 0 {
                if i > pos {
                    bp = m;
                    be = i;
                }
            } else if st == 0 {
                return (bp, be);
            }
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

/// Byte-class-compressed interpreter: one extra indirection (`class_of[byte]`) per byte.
fn classed_match_at(t: &Baked, bytes: &[u8], pos: usize) -> (i32, usize) {
    let len = bytes.len();
    let nc = t.n_bclasses;
    let mut st = 1usize;
    let (mut bp, mut be, mut i) = (-1i32, pos, pos);
    while i < len {
        let cl = t.class_of[bytes[i] as usize] as usize;
        st = t.ctrans[st * nc + cl] as usize;
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

// ─── drivers ─────────────────────────────────────────────────────────────────

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

fn drive_rx(scanner: &RxScanner, text: &str) -> (u64, u64) {
    let (mut h, mut n, mut pos) = (0u64, 0u64, 0usize);
    while pos < text.len() {
        match scanner.match_at(text, pos) {
            Some((id, val)) => {
                let end = pos + val.len();
                h = fold(h, id, end);
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

fn drive_patterns<F: FnMut(&[u8], usize) -> (i32, usize)>(
    lexer: &BasicLexer,
    ids: &[SymbolId],
    text: &str,
    mut match_at: F,
) -> (u64, u64) {
    let bytes = text.as_bytes();
    let (mut h, mut n, mut pos) = (0u64, 0u64, 0usize);
    while pos < text.len() {
        let (pat, end) = match_at(bytes, pos);
        if pat >= 0 && end > pos {
            let id = lexer.spike_retype(ids[pat as usize], &text[pos..end]);
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

// ─── timing ──────────────────────────────────────────────────────────────────

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

// ─── workloads ───────────────────────────────────────────────────────────────

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

struct PlanBits {
    global_prefix: String,
    groups: Vec<(u32, String)>,
    unless: Vec<(u32, Vec<(String, bool, u32)>)>,
}

/// Re-derive the standalone plan (same call chain as `standalone::bake`) so the `rx`
/// scanner is byte-identical to what a generated parser ships.
fn plan_bits(
    grammar: &str,
    starts: &[String],
    base: Option<PathBuf>,
    flags: u32,
) -> Option<PlanBits> {
    let g = load_grammar_with_base(grammar, starts, true, false, base).ok()?;
    let cg = lower(&g);
    let conf = basic_lexer_conf(&cg, flags);
    let term_refs: Vec<(SymbolId, &TerminalDef)> =
        conf.terminals.iter().map(|(id, t)| (*id, t)).collect();
    let plan = scanner_plan(&term_refs, conf.global_flags).ok()?;
    let groups: Vec<(u32, String)> = plan
        .groups
        .iter()
        .map(|(id, rx)| (id.0, rx.clone()))
        .collect();
    let unless: Vec<(u32, Vec<(String, bool, u32)>)> = plan
        .unless
        .iter()
        .map(|(id, es): (&SymbolId, &Vec<UnlessEntry>)| {
            (
                id.0,
                es.iter()
                    .map(|e| (e.value.clone(), e.ci, e.keyword.0))
                    .collect(),
            )
        })
        .collect();
    Some(PlanBits {
        global_prefix: plan.global_prefix.clone(),
        groups,
        unless,
    })
}

fn run_workload(
    name: &str,
    grammar: &str,
    starts: &[String],
    base: Option<PathBuf>,
    texts: &[String],
) {
    // in-process DFA basic lexer (seam + dense engine)
    let g = match load_grammar_with_base(grammar, starts, true, false, base.clone()) {
        Ok(g) => g,
        Err(_) => {
            println!("SKIP\t{name}\tgrammar does not build");
            return;
        }
    };
    let cg = lower(&g);
    let conf = basic_lexer_conf(&cg, 0).with_backend(LexerBackend::Dfa);
    let t0 = Instant::now();
    let lexer = match BasicLexer::new(&conf) {
        Ok(l) => l,
        Err(_) => {
            println!("SKIP\t{name}\tbasic lexer does not build");
            return;
        }
    };
    let dfa_build_ns = t0.elapsed().as_nanos();
    let Some((dfa, ids)) = lexer.spike_plain_dense() else {
        println!("SKIP\t{name}\tnot a single dense plain engine");
        return;
    };
    let ids: &'static [SymbolId] = ids.leak();

    // rx scanner (stock standalone) — same plan.
    let Some(pb) = plan_bits(grammar, starts, base.clone(), 0) else {
        println!("SKIP\t{name}\tplan does not build");
        return;
    };
    let (rx, rx_build_ns) = RxScanner::build(&pb.global_prefix, &pb.groups, &pb.unless);

    // bake
    let t0 = Instant::now();
    let Some(baked) = bake(dfa) else {
        println!("SKIP\t{name}\tDFA does not flatten");
        return;
    };
    let bake_ns = t0.elapsed().as_nanos();

    let bytes: usize = texts.iter().map(|t| t.len()).sum();
    let flat_kib = baked.n_states * 256 * 4 / 1024;
    let classed_kib = (baked.n_states * baked.n_bclasses * 4 + 256) / 1024;
    println!(
        "\n{name}: {bytes} B over {} input(s); {} states, {} byte-classes; \
         flat {flat_kib} KiB / classed {classed_kib} KiB",
        texts.len(),
        baked.n_states,
        baked.n_bclasses
    );
    println!(
        "  one-shot build: rx Regex::new {:.2} ms | dense-DFA determinize {:.2} ms | \
         + bake flatten {:.2} ms | shipped table load ~0",
        rx_build_ns as f64 / 1e6,
        dfa_build_ns as f64 / 1e6,
        bake_ns as f64 / 1e6
    );

    // Differential: every rep emits the byte-identical stream.
    let mut streams: Vec<(&str, u64, u64)> = Vec::new();
    let acc = |f: &dyn Fn(&str) -> (u64, u64)| {
        let (mut h, mut n) = (0u64, 0u64);
        for t in texts {
            let (hh, nn) = f(t);
            h = h.wrapping_add(hh);
            n += nn;
        }
        (h, n)
    };
    let (h, n) = acc(&|t| drive_rx(&rx, t));
    streams.push(("rx", h, n));
    let (h, n) = acc(&|t| drive_seam(&lexer, t));
    streams.push(("seam", h, n));
    let (h, n) = acc(&|t| drive_patterns(&lexer, ids, t, |b, p| raw_match_at(dfa, b, p)));
    streams.push(("raw", h, n));
    let (h, n) = acc(&|t| drive_patterns(&lexer, ids, t, |b, p| table_match_at(&baked, b, p)));
    streams.push(("table", h, n));
    let (h, n) = acc(&|t| {
        drive_patterns(&lexer, ids, t, |b, p| {
            table_match_at_unchecked(&baked, b, p)
        })
    });
    streams.push(("table-uc", h, n));
    let (h, n) = acc(&|t| drive_patterns(&lexer, ids, t, |b, p| classed_match_at(&baked, b, p)));
    streams.push(("classed", h, n));

    let (h0, n0) = (streams[0].1, streams[0].2);
    for (vn, h, n) in &streams {
        assert_eq!(
            (*h, *n),
            (h0, n0),
            "{name}/{vn}: token stream diverges from rx — not faithful"
        );
    }
    println!("  differential OK: all reps emit the identical stream ({n0} tokens)");

    // Timing. Baseline = rx (the stock standalone scanner).
    let time = |f: &dyn Fn()| {
        let s = measure(|| f());
        (s.median_ns, s.min_ns)
    };
    let mbps = |ns: f64| bytes as f64 / ns * 1e3;
    let rx_f: Box<dyn Fn()> = Box::new(|| {
        for t in texts {
            black_box(drive_rx(&rx, black_box(t)));
        }
    });
    let (rx_med, rx_min) = time(&rx_f);
    println!(
        "  {:<9} {:>12.0} ns (min {:>12.0})  {:>8.1} MB/s   1.00x (baseline: stock standalone)",
        "rx",
        rx_med,
        rx_min,
        mbps(rx_med)
    );
    let variants: Vec<(&str, Box<dyn Fn(&str) -> (u64, u64)>)> = vec![
        ("seam", Box::new(|t: &str| drive_seam(&lexer, t))),
        (
            "raw",
            Box::new(|t: &str| drive_patterns(&lexer, ids, t, |b, p| raw_match_at(dfa, b, p))),
        ),
        (
            "table",
            Box::new(|t: &str| drive_patterns(&lexer, ids, t, |b, p| table_match_at(&baked, b, p))),
        ),
        (
            "table-uc",
            Box::new(|t: &str| {
                drive_patterns(&lexer, ids, t, |b, p| {
                    table_match_at_unchecked(&baked, b, p)
                })
            }),
        ),
        (
            "classed",
            Box::new(|t: &str| {
                drive_patterns(&lexer, ids, t, |b, p| classed_match_at(&baked, b, p))
            }),
        ),
    ];
    for (vn, f) in &variants {
        let ff: Box<dyn Fn()> = Box::new(|| {
            for t in texts {
                black_box(f(black_box(t)));
            }
        });
        let (med, min) = time(&ff);
        println!(
            "  {:<9} {:>12.0} ns (min {:>12.0})  {:>8.1} MB/s   {:.2}x vs rx",
            vn,
            med,
            min,
            mbps(med),
            rx_med / med
        );
    }

    // Amdahl: full BasicLexer::lex (scan + owned Token) — scan share of lexing.
    if texts.iter().all(|t| lexer.lex(t).is_ok()) {
        let lex_f: Box<dyn Fn()> = Box::new(|| {
            for t in texts {
                black_box(lexer.lex(black_box(t)).unwrap());
            }
        });
        let (med, _min) = time(&lex_f);
        println!(
            "  {:<9} {:>12.0} ns                {:>8.1} MB/s   (rx scan is {:.0}% of full lex)",
            "lex-full",
            med,
            mbps(med),
            100.0 * rx_med / med
        );
    }
}

fn main() {
    println!("# L5 standalone scanner-representation ladder (issue #620)");
    println!("# baseline = rx (regex-crate combined alternation = the stock standalone scanner)");

    // JSON synthetic size sweep.
    let starts = vec!["start".to_string()];
    let _ = load_grammar(JSON_GRAMMAR, &starts, true, false).expect("json loads");
    for (tag, records) in [("json_56k", 200), ("json_594k", 2000)] {
        run_workload(tag, JSON_GRAMMAR, &starts, None, &[gen_json(records, 8)]);
    }

    // Wild bank: the qualifying standalone-bakeable grammars (from the probe).
    let wild = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/wild");
    for proj in ["matter_idl", "poetry_markers", "poetry_pep508"] {
        let pdir = wild.join(proj);
        let Ok(meta) = serde_json::from_str::<serde_json::Value>(
            &std::fs::read_to_string(pdir.join("meta.json")).unwrap_or_default(),
        ) else {
            continue;
        };
        let start = meta["lark_options"]["start"]
            .as_str()
            .unwrap_or("start")
            .to_string();
        let entry = meta["entry_grammar"].as_str().unwrap_or("");
        let gpath = pdir.join(entry);
        let grammar = std::fs::read_to_string(&gpath).unwrap_or_default();
        let base = gpath.parent().map(|p| p.to_path_buf());
        let inputs: Vec<String> = meta["inputs"]
            .as_object()
            .map(|o| {
                o.keys()
                    .filter_map(|rel| std::fs::read_to_string(pdir.join(rel)).ok())
                    .collect()
            })
            .unwrap_or_default();
        run_workload(proj, &grammar, &[start], base, &inputs);
    }
}
