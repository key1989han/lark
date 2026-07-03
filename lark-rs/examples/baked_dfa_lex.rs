//! THROWAWAY SPIKE (parser-optimization spike 2026-07-03): **baked /
//! directly-executable DFA vs the interpreted `regex-automata` dense DFA** — the
//! top unproven lever of `docs/notes/parser-optimization-research-2026-07.md`
//! (finding B1, RE/flex `--full`/`--fast`).
//!
//! All variants drive the *same* determinized automaton (extracted out of the real
//! `DfaScanner` via the `baked-dfa-spike` accessors), so the deltas isolate pure
//! interpretation overhead — the work (states visited per byte) is identical by
//! construction, and a differential check asserts every variant produces the
//! byte-identical `(terminal, end)` token sequence before anything is timed:
//!
//! * **seam** — `BasicLexer::match_at` (production path: prefilter + `Input`
//!   construction + `try_search_fwd`), the baseline.
//! * **raw** — a hand-rolled `Automaton`-trait search loop over the same dense
//!   DFA (no prefilter, no `Input` per position; still byte-class + table walk).
//! * **table** — a flat baked `[state × 256] → state` opcode table (RE/flex
//!   `--full`), safe indexing.
//! * **table-uc** — same table, unchecked indexing (the codegen-quality ceiling
//!   for a table walk).
//! * **gen** — the committed generated goto/switch scanner
//!   (`examples/baked/json_dfa.rs`, RE/flex `--fast`) — JSON workload only.
//!
//! Reused-parser vs one-shot: the scan throughputs below are the **reused**
//! column (automaton built once); the one-shot column is reported as build cost —
//! `BasicLexer::new` (grammar's scanner build) vs the extra `bake()` flattening a
//! one-shot baked lexer would pay on top.
//!
//! ```text
//! cargo run --release --features baked-dfa-spike --example baked_dfa_lex
//! ```
//!
//! Wall-clock is a trend (BENCH.md): only same-session ratios travel. The
//! deterministic signal here is the differential (identical token streams ⇒
//! identical work ⇒ the delta is pure constant-factor interpretation cost).

use std::collections::HashMap;
use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use lark_rs::grammar::load_grammar_with_base;
use lark_rs::{basic_lexer_conf, load_grammar, lower, BasicLexer, Lexer, LexerBackend, SymbolId};
use regex_automata::dfa::{dense, Automaton};
use regex_automata::{Anchored, Input};

/// The generated goto/switch scanner (RE/flex `--fast` analog) for the JSON
/// grammar. Regenerate with `--example bake_dfa_gen`.
mod baked_json {
    include!("baked/json_dfa.rs");
}

// Byte-identical to `bake_dfa_gen.rs` / `benches/lex_backends.rs`.
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

// ─── Baking (same flattening as bake_dfa_gen.rs; duplicated — spike) ─────────

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

// ─── The per-position matchers under comparison ──────────────────────────────

/// Raw `Automaton`-trait search over the live dense DFA — the doc-loop search
/// `try_search_fwd` performs, minus `Input`/prefilter/enum-dispatch overhead.
/// Returns `(local pattern id or -1, end)` with zero-width matches rejected.
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

/// Flat opcode-table interpreter (RE/flex `--full` analog), safe indexing.
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

/// Same interpreter, unchecked indexing — the ceiling a bounds-check-free
/// codegen'd table walk could reach. Safety: `st` always comes from the table
/// itself (values < n_states by construction) and the byte index is < 256.
fn table_match_at_unchecked(t: &Baked, bytes: &[u8], pos: usize) -> (i32, usize) {
    let len = bytes.len();
    let mut st = 1usize;
    let mut bp: i32 = -1;
    let mut be = pos;
    let mut i = pos;
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

// ─── Tokenization drivers (whole-input scan loops) ───────────────────────────

/// Advance `pos` one char (no-match recovery — identical across variants so the
/// comparison holds even where an input does not fully lex).
fn bump_char(text: &str, pos: usize) -> usize {
    let mut p = pos + 1;
    while p < text.len() && !text.is_char_boundary(p) {
        p += 1;
    }
    p
}

/// Fold one token into the running checksum (deterministic differential signal).
#[inline]
fn fold(h: u64, id: u32, end: usize) -> u64 {
    h.wrapping_mul(0x100000001b3)
        .wrapping_add(((id as u64) << 32) ^ end as u64)
}

/// Production seam: `BasicLexer::match_at` (includes prefilter + retype).
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

/// Shared driver for the pattern-returning variants; maps pattern → terminal id
/// and re-applies the seam's `unless` retype so the stream is seam-identical.
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

// ─── Timing (same estimator as benches/lex_backends.rs) ──────────────────────

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

/// Run the whole comparison for one lexer over one corpus. `gen` is the
/// generated goto/switch matcher, if one exists for this grammar.
fn run_workload(
    name: &str,
    lexer: &BasicLexer,
    texts: &[String],
    gen: Option<fn(&[u8], usize) -> (i32, usize)>,
) {
    let Some((dfa, ids)) = lexer.spike_plain_dense() else {
        println!("SKIP\t{name}\tscanner is not a single dense plain engine (guarded/fence/hybrid)");
        return;
    };
    // Spike: leak the tiny map so every closure below can capture a plain
    // `&'static` copy (a few dozen SymbolIds per workload).
    let ids: &'static [SymbolId] = ids.leak();
    // One-shot column: the extra flattening cost a baked lexer pays at build time.
    let t0 = Instant::now();
    let Some(baked) = bake(dfa) else {
        println!("SKIP\t{name}\tDFA does not bake (context-sensitive start or quit states)");
        return;
    };
    let bake_ns = t0.elapsed().as_nanos();
    let bytes: usize = texts.iter().map(|t| t.len()).sum();
    println!(
        "\n{name}: {bytes} bytes over {} input(s); {} baked states, {} patterns, \
         {} KiB flat table, bake cost {:.2} ms (one-shot column)",
        texts.len(),
        baked.n_states,
        ids.len(),
        baked.n_states * 256 * 4 / 1024,
        bake_ns as f64 / 1e6
    );

    // Differential: every variant must produce the byte-identical token stream.
    let mut streams: Vec<(&str, u64, u64)> = Vec::new();
    let (mut h, mut n) = (0u64, 0u64);
    for t in texts {
        let (hh, nn) = drive_seam(lexer, t);
        h = h.wrapping_add(hh);
        n += nn;
    }
    streams.push(("seam", h, n));
    let variants: Vec<(&str, Box<dyn Fn(&str) -> (u64, u64)>)> = {
        let mut v: Vec<(&str, Box<dyn Fn(&str) -> (u64, u64)>)> = vec![
            (
                "raw",
                Box::new(|t: &str| {
                    drive_patterns(lexer, &ids, t, |b, p| {
                        raw_match_at(dfa, unsafe { std::str::from_utf8_unchecked(b) }, p)
                    })
                }),
            ),
            (
                "table",
                Box::new(|t: &str| {
                    drive_patterns(lexer, &ids, t, |b, p| table_match_at(&baked, b, p))
                }),
            ),
            (
                "table-uc",
                Box::new(|t: &str| {
                    drive_patterns(lexer, &ids, t, |b, p| {
                        table_match_at_unchecked(&baked, b, p)
                    })
                }),
            ),
        ];
        if let Some(g) = gen {
            v.push((
                "gen",
                Box::new(move |t: &str| drive_patterns(lexer, &ids, t, g)),
            ));
        }
        v
    };
    for (vname, f) in &variants {
        let (mut h, mut n) = (0u64, 0u64);
        for t in texts {
            let (hh, nn) = f(t);
            h = h.wrapping_add(hh);
            n += nn;
        }
        streams.push((vname, h, n));
    }
    let (h0, n0) = (streams[0].1, streams[0].2);
    for (vname, h, n) in &streams {
        assert_eq!(
            (*h, *n),
            (h0, n0),
            "{name}/{vname}: token stream diverges from the seam — bake is not faithful"
        );
    }
    println!("  differential OK: all variants emit the identical stream ({n0} tokens)");

    // Timing. Seam first, then each variant + ratio.
    let time_one = |f: &dyn Fn()| {
        let s = measure(|| f());
        (s.median_ns, s.min_ns)
    };
    let seam_f: Box<dyn Fn()> = Box::new(|| {
        for t in texts {
            black_box(drive_seam(lexer, black_box(t)));
        }
    });
    let (seam_med, seam_min) = time_one(&seam_f);
    let mbps = |ns: f64| bytes as f64 / ns * 1e3;
    println!(
        "  {:<9} {:>12.0} ns (min {:>12.0})  {:>8.1} MB/s   1.00x  [BENCH\tbaked_lex\t{name}/seam\t{bytes}\t{:.0}\t{:.0}\t{:.1}]",
        "seam", seam_med, seam_min, mbps(seam_med), seam_med, seam_min, mbps(seam_med)
    );
    for (vname, f) in &variants {
        let ff: Box<dyn Fn()> = Box::new(|| {
            for t in texts {
                black_box(f(black_box(t)));
            }
        });
        let (med, min) = time_one(&ff);
        println!(
            "  {:<9} {:>12.0} ns (min {:>12.0})  {:>8.1} MB/s   {:.2}x  [BENCH\tbaked_lex\t{name}/{vname}\t{bytes}\t{:.0}\t{:.0}\t{:.1}]",
            vname, med, min, mbps(med), seam_med / med, med, min, mbps(med)
        );
    }

    // Context row (Amdahl): the FULL `BasicLexer::lex` — scan + owned-`Token`
    // construction — over the same corpus, so the scan share of lexing (and hence
    // the ceiling any scanner speedup has on end-to-end lexing) is a same-session
    // ratio. Skipped when an input does not fully lex (wild corpora may not).
    if texts.iter().all(|t| lexer.lex(t).is_ok()) {
        let lex_f: Box<dyn Fn()> = Box::new(|| {
            for t in texts {
                black_box(lexer.lex(black_box(t)).unwrap());
            }
        });
        let (med, min) = time_one(&lex_f);
        println!(
            "  {:<9} {:>12.0} ns (min {:>12.0})  {:>8.1} MB/s   {:.2}x  (scan is {:.0}% of full lex)  [BENCH\tbaked_lex\t{name}/lex-full\t{bytes}\t{:.0}\t{:.0}\t{:.1}]",
            "lex-full",
            med,
            min,
            mbps(med),
            seam_med / med,
            100.0 * seam_med / med,
            med,
            min,
            mbps(med)
        );
    }
}

// ─── Wild bank ────────────────────────────────────────────────────────────────

/// Build a `BasicLexer` (Dfa backend) + corpus for one wild project, honoring the
/// project's own upstream options. `None` (with a printed reason) when the project
/// is out of scope for this spike.
fn load_wild(pdir: &Path) -> Option<(String, BasicLexer, Vec<String>)> {
    let meta: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(pdir.join("meta.json")).ok()?).ok()?;
    let name = meta["name"].as_str()?.to_string();
    let o = &meta["lark_options"];
    if o["parser"].as_str() != Some("lalr") {
        println!("SKIP\t{name}\tnon-LALR project (basic-lexing its corpus would mis-segment)");
        return None;
    }
    let mut flags = 0u32;
    if let Some(letters) = o["g_regex_flags"].as_str() {
        use lark_rs::grammar::terminal::flags as tf;
        for ch in letters.chars() {
            flags |= match ch {
                'i' => tf::IGNORECASE,
                'm' => tf::MULTILINE,
                's' => tf::DOTALL,
                'x' => tf::VERBOSE,
                _ => return None,
            };
        }
    }
    let grammar = std::fs::read_to_string(pdir.join(meta["entry_grammar"].as_str()?)).ok()?;
    let g = match load_grammar_with_base(
        &grammar,
        &[o["start"].as_str()?.to_string()],
        o["maybe_placeholders"].as_bool().unwrap_or(true),
        o["keep_all_tokens"].as_bool().unwrap_or(false),
        Some(pdir.join("grammar")),
    ) {
        Ok(g) => g,
        Err(_) => {
            println!("SKIP\t{name}\tgrammar does not build (wild xfail)");
            return None;
        }
    };
    let cg = lower(&g);
    let conf = basic_lexer_conf(&cg, flags).with_backend(LexerBackend::Dfa);
    let lexer = match BasicLexer::new(&conf) {
        Ok(l) => l,
        Err(_) => {
            println!("SKIP\t{name}\tbasic lexer does not build");
            return None;
        }
    };
    let inputs: Vec<String> = meta["inputs"]
        .as_object()?
        .keys()
        .filter_map(|rel| std::fs::read_to_string(pdir.join(rel)).ok())
        .collect();
    Some((name, lexer, inputs))
}

fn main() {
    println!("# baked/directly-executable DFA vs interpreted regex-automata dense DFA");
    println!(
        "# variants: seam (production) / raw (hand Automaton loop) / table (flat u32[state*256])"
    );
    println!("#           table-uc (unchecked) / gen (generated goto-switch, JSON only)");

    // JSON synthetic (the canonical bench grammar), size sweep + generated code.
    let g = load_grammar(JSON_GRAMMAR, &["start".to_string()], true, false)
        .expect("json grammar loads");
    let cg = lower(&g);
    let conf = basic_lexer_conf(&cg, 0).with_backend(LexerBackend::Dfa);
    // One-shot column: the scanner build the current lexer pays.
    let t0 = Instant::now();
    let json_lexer = BasicLexer::new(&conf).expect("json lexer builds");
    println!(
        "\njson: BasicLexer::new (scanner build, one-shot column) {:.2} ms",
        t0.elapsed().as_nanos() as f64 / 1e6
    );
    for (tag, records) in [("json_56k", 200), ("json_594k", 2000), ("json_2.4m", 8000)] {
        run_workload(
            tag,
            &json_lexer,
            &[gen_json(records, 8)],
            Some(baked_json::match_at),
        );
    }

    // Wild bank: every LALR project whose scanner is a single dense plain engine.
    let wild = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/wild");
    let mut projects: Vec<PathBuf> = std::fs::read_dir(&wild)
        .expect("tests/wild exists")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.join("meta.json").is_file())
        .collect();
    projects.sort();
    println!("\n# wild bank (BasicLexer over each project's full terminal set)");
    for pdir in &projects {
        if let Some((name, lexer, inputs)) = load_wild(pdir) {
            run_workload(&name, &lexer, &inputs, None);
        }
    }
}
