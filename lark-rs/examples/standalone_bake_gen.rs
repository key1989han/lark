//! THROWAWAY SPIKE (standalone-bake spike 2026-07-03, issue #620): generate the
//! **variant copies of `generate_standalone` output** the spike bench drives, and
//! print the **L5c footprint sweep**.
//!
//! The 2026-07-03 in-process spike (`docs/notes/spike-parser-optimizations-2026-07-03.md`)
//! proved the flat-table bake at the scanner level; #620 claims the levers pay on
//! the *standalone / one-shot* surface. This example turns each #620 phase into a
//! spliced copy of a real generated parser (never touching `tests/standalone/`):
//!
//! * `<g>_stock.rs`    — `generate_standalone` output verbatim (+ a lex-only
//!   accessor), the L5-value baseline. Scanner: `regex`-crate combined
//!   alternation compiled at `Parser::new`.
//! * `<g>_lean.rs`     — the **L5a analog**: the same `regex-automata` dense DFA
//!   the in-process engine uses, built at `Parser::new` *at runtime* from the
//!   baked pattern list, driven by the prior spike's lean anchored loop. No
//!   rodata cost; pays determinization at load. (NB #620's L5a assumes the
//!   standalone runtime is already DFA-driven — it is not; this variant is what
//!   L5a actually means on this surface.)
//! * `<g>_baked.rs`    — **L5b**: the BFS-flattened `u32[state × 256]` tables of
//!   the in-process dense DFA baked as `static` data, driven by the spike's
//!   interpreter loop. No scanner build at all; pays rodata + compile time.
//! * `<g>_interned.rs` — the **L5d (M2) analog**: `Token::type_` / `Tree::data`
//!   as `&'static str` straight out of the baked symbol table (the standalone
//!   surface gets label interning *for free* — no interner needed). Scanner
//!   stock, so the delta isolates the output path.
//!
//! Grammars: the canonical JSON bench grammar + every wild-bank project that
//! qualifies (LALR + standalone-hostable + single plain dense scanner + bakeable
//! DFA — the prior spike's SKIP taxonomy, re-checked here on the standalone
//! surface). Copies land in `examples/standalone_spike/gen/`; correctness gates
//! and timing live in `examples/standalone_bake_bench.rs`.
//!
//! ```text
//! cargo run --release --features baked-dfa-spike --example standalone_bake_gen
//! ```

use std::collections::{BTreeSet, HashMap};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use lark_rs::grammar::load_grammar_with_base;
use lark_rs::{
    basic_lexer_conf, generate_standalone, lower, BasicLexer, Lark, LarkOptions, LexerBackend,
    LexerType, ParserAlgorithm,
};
use regex_automata::dfa::{dense, Automaton};
use regex_automata::{Anchored, Input};

// Byte-identical to `examples/baked_dfa_lex.rs` / `benches/lex_backends.rs`.
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

// ─── Flattening (same BFS bake as the 2026-07-03 spike, incl. the start-state
//     context-sensitivity probe and the delayed-match/EOI handling) ───────────

struct Flat {
    trans: Vec<u32>,
    match_pat: Vec<i32>,
    eoi_pat: Vec<i32>,
    n_states: usize,
}

fn bake_flat(dfa: &dense::DFA<Vec<u32>>) -> Option<Flat> {
    let start_sid = dfa
        .start_state_forward(&Input::new("").anchored(Anchored::Yes))
        .ok()?;
    // Context-sensitivity probe: a look-behind-conditioned start state (e.g.
    // pyquil) falsifies a single baked start state.
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
    Some(Flat {
        trans,
        match_pat,
        eoi_pat,
        n_states: n,
    })
}

// ─── L5c: byte-class compression of the flat table ────────────────────────────

struct FlatClasses {
    class_of: [u8; 256],
    n_classes: usize,
    trans: Vec<u32>,
}

/// Column-dedup the 256-wide table: two bytes are equivalent iff their columns
/// are identical across all states. Grammar-truth compression, independent of
/// regex-automata's internal class computation.
fn compress_classes(flat: &Flat) -> FlatClasses {
    let n = flat.n_states;
    let mut class_of = [0u8; 256];
    let mut seen: HashMap<Vec<u32>, u8> = HashMap::new();
    let mut reps: Vec<u8> = Vec::new();
    for b in 0..256usize {
        let col: Vec<u32> = (0..n).map(|s| flat.trans[(s << 8) + b]).collect();
        let next = seen.len() as u8;
        let c = *seen.entry(col).or_insert_with(|| {
            reps.push(b as u8);
            next
        });
        class_of[b] = c;
    }
    let n_classes = reps.len();
    let mut trans = vec![0u32; n * n_classes];
    for s in 0..n {
        for (c, rep) in reps.iter().enumerate() {
            trans[s * n_classes + c] = flat.trans[(s << 8) + *rep as usize];
        }
    }
    FlatClasses {
        class_of,
        n_classes,
        trans,
    }
}

// ─── The two interpreter loops (giveback measurement) ─────────────────────────

fn flat_match_at(t: &Flat, bytes: &[u8], pos: usize) -> (i32, usize) {
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

fn class_match_at(t: &Flat, c: &FlatClasses, bytes: &[u8], pos: usize) -> (i32, usize) {
    let len = bytes.len();
    let nc = c.n_classes;
    let mut st = 1usize;
    let mut bp: i32 = -1;
    let mut be = pos;
    let mut i = pos;
    while i < len {
        st = c.trans[st * nc + c.class_of[bytes[i] as usize] as usize] as usize;
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

/// Whole-corpus tokenization checksum through a `(pattern, end)` matcher —
/// deterministic identity signal between the two interpreters.
fn drive<F: FnMut(&[u8], usize) -> (i32, usize)>(text: &str, mut match_at: F) -> (u64, u64) {
    let bytes = text.as_bytes();
    let (mut h, mut n, mut pos) = (0u64, 0u64, 0usize);
    while pos < text.len() {
        let (pat, end) = match_at(bytes, pos);
        if pat >= 0 && end > pos {
            h = h
                .wrapping_mul(0x100000001b3)
                .wrapping_add(((pat as u64) << 32) ^ end as u64);
            n += 1;
            pos = end;
        } else {
            h = h.wrapping_mul(0x100000001b3).wrapping_add(pos as u64);
            pos += 1;
            while pos < text.len() && !text.is_char_boundary(pos) {
                pos += 1;
            }
        }
    }
    (h, n)
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

// ─── Variant splicing ─────────────────────────────────────────────────────────

/// Insert `code` just before the closing `}` of the generated `mod parser`.
fn splice_before_mod_close(src: &str, code: &str) -> String {
    let marker = "}\n\n#[allow(unused_imports)]\npub use parser::{";
    let idx = src.rfind(marker).expect("generated tail marker not found");
    format!("{}{}{}", &src[..idx], code, &src[idx..])
}

/// Replace the runtime's `struct Scanner` + `impl Scanner` region (everything
/// from `struct Scanner {` up to `fn lex(`) with a variant scanner.
fn replace_scanner(src: &str, new_scanner: &str) -> String {
    let start = src
        .find("struct Scanner {")
        .expect("scanner struct not found");
    let end = src.find("\nfn lex(").expect("fn lex not found");
    assert!(start < end, "scanner region markers out of order");
    format!("{}{}{}", &src[..start], new_scanner, &src[end..])
}

fn replace_exactly_once(src: String, from: &str, to: &str) -> String {
    let count = src.matches(from).count();
    assert_eq!(
        count, 1,
        "interned splice pattern {from:?} matched {count} times (runtime drifted?)"
    );
    src.replacen(from, to, 1)
}

const ACCESSOR: &str = r#"
// ── SPIKE-ONLY accessor (standalone-bake spike 2026-07-03, THROWAWAY) ──
impl Parser {
    /// Lex only — the scan + token-materialization half of `parse`, exposed so
    /// the spike bench can separate scanning from the LALR drive.
    pub fn spike_lex(&self, text: &str) -> Result<Vec<Token>, String> {
        lex(self.data, &self.scanner, text)
    }
}
"#;

/// The `unless` retype helpers shared by the lean/baked variant scanners (the
/// same tables the stock scanner builds inside `Scanner::new`).
const UNLESS_HELPERS: &str = r#"
/// The `unless` keyword-retype tables (exact + case-insensitive), identical to
/// the stock scanner's.
fn build_unless(data: &GrammarData) -> HashMap<u32, (HashMap<String, u32>, Vec<(Regex, u32)>)> {
    let mut unless: HashMap<u32, (HashMap<String, u32>, Vec<(Regex, u32)>)> = HashMap::new();
    for (re_id, entries) in data.unless {
        let slot = unless.entry(*re_id).or_default();
        for (value, ci, kw_id) in *entries {
            if *ci {
                let src = format!("^(?i:{})$", regex::escape(value));
                let re = Regex::new(&src).expect("baked unless regex is valid");
                slot.1.push((re, *kw_id));
            } else {
                slot.0.entry(value.to_string()).or_insert(*kw_id);
            }
        }
    }
    unless
}

fn retype(
    unless: &HashMap<u32, (HashMap<String, u32>, Vec<(Regex, u32)>)>,
    id: u32,
    value: &str,
) -> u32 {
    match unless.get(&id) {
        Some((exact, ci)) => exact
            .get(value)
            .copied()
            .or_else(|| {
                ci.iter()
                    .find(|(re, _)| re.is_match(value))
                    .map(|(_, k)| *k)
            })
            .unwrap_or(id),
        None => id,
    }
}
"#;

const LEAN_SCANNER: &str = r#"struct Scanner {
    // SPIKE variant (L5a analog): the same `regex-automata` dense DFA the
    // in-process engine builds, constructed at `Parser::new` *at runtime* from
    // the baked pattern list and driven by the prior spike's lean anchored loop
    // (no per-position `Input` re-construction inside the byte walk, no
    // prefilter, no capture groups). No rodata cost; pays determinization at
    // load; requires `regex-automata` as a dependency of the generated parser.
    dfa: regex_automata::dfa::dense::DFA<Vec<u32>>,
    /// local PatternID → terminal id, in alternation order.
    pattern_ids: Vec<u32>,
    unless: HashMap<u32, (HashMap<String, u32>, Vec<(Regex, u32)>)>,
}

impl Scanner {
    fn new(data: &GrammarData) -> Scanner {
        use regex_automata::dfa::dense;
        use regex_automata::nfa::thompson;
        let srcs: Vec<String> = data
            .scan_groups
            .iter()
            .map(|(_, rx)| format!("{}{}", data.global_prefix, rx))
            .collect();
        let refs: Vec<&str> = srcs.iter().map(|s| s.as_str()).collect();
        let nfa = thompson::NFA::compiler()
            .configure(thompson::Config::new().which_captures(thompson::WhichCaptures::None))
            .build_many(&refs)
            .expect("baked scanner NFA builds");
        let dfa = dense::Builder::new()
            .configure(
                dense::Config::new()
                    .match_kind(regex_automata::MatchKind::LeftmostFirst)
                    .start_kind(regex_automata::dfa::StartKind::Anchored),
            )
            .build_from_nfa(&nfa)
            .expect("baked scanner DFA determinizes");
        let pattern_ids = data.scan_groups.iter().map(|(id, _)| *id).collect();
        Scanner {
            dfa,
            pattern_ids,
            unless: build_unless(data),
        }
    }

    /// Match a single token starting exactly at `pos`. `None` = nothing here.
    fn match_at<'t>(&self, text: &'t str, pos: usize) -> Option<(u32, &'t str)> {
        use regex_automata::dfa::Automaton;
        let bytes = text.as_bytes();
        let input = regex_automata::Input::new(text)
            .span(pos..bytes.len())
            .anchored(regex_automata::Anchored::Yes);
        let mut state = self.dfa.start_state_forward(&input).ok()?;
        let mut best: i32 = -1;
        let mut end = pos;
        let mut i = pos;
        while i < bytes.len() {
            state = self.dfa.next_state(state, bytes[i]);
            if self.dfa.is_special_state(state) {
                if self.dfa.is_match_state(state) {
                    if i > pos {
                        best = self.dfa.match_pattern(state, 0).as_usize() as i32;
                        end = i;
                    }
                } else if self.dfa.is_dead_state(state) {
                    return self.accept(text, pos, best, end);
                }
            }
            i += 1;
        }
        let eoi = self.dfa.next_eoi_state(state);
        if self.dfa.is_match_state(eoi) && bytes.len() > pos {
            best = self.dfa.match_pattern(eoi, 0).as_usize() as i32;
            end = bytes.len();
        }
        self.accept(text, pos, best, end)
    }

    fn accept<'t>(
        &self,
        text: &'t str,
        pos: usize,
        best: i32,
        end: usize,
    ) -> Option<(u32, &'t str)> {
        if best < 0 || end == pos {
            return None;
        }
        let value = &text[pos..end];
        let id = self.pattern_ids[best as usize];
        Some((retype(&self.unless, id, value), value))
    }
}
"#;

const BAKED_SCANNER: &str = r#"struct Scanner {
    // SPIKE variant (L5b): the BFS-flattened `u32[state × 256]` transition table
    // of the in-process dense DFA, baked as `static` data at generation time and
    // driven by the spike's interpreter loop (safe indexing). `Parser::new`
    // builds nothing but the `unless` tables — the scanner itself is rodata.
    unless: HashMap<u32, (HashMap<String, u32>, Vec<(Regex, u32)>)>,
}

impl Scanner {
    fn new(data: &GrammarData) -> Scanner {
        for (k, (id, _)) in data.scan_groups.iter().enumerate() {
            assert_eq!(
                BAKED_PATTERN_IDS[k], *id,
                "baked table pattern order drifted from DATA"
            );
        }
        Scanner {
            unless: build_unless(data),
        }
    }

    /// Match a single token starting exactly at `pos`. `None` = nothing here.
    fn match_at<'t>(&self, text: &'t str, pos: usize) -> Option<(u32, &'t str)> {
        let bytes = text.as_bytes();
        let len = bytes.len();
        let mut st = 1usize;
        let mut best: i32 = -1;
        let mut end = pos;
        let mut i = pos;
        while i < len {
            st = BAKED_TRANS[(st << 8) + bytes[i] as usize] as usize;
            let m = BAKED_MATCH[st];
            if m >= 0 {
                if i > pos {
                    best = m;
                    end = i;
                }
            } else if st == 0 {
                break;
            }
            i += 1;
        }
        if i >= len {
            let m = BAKED_EOI[st];
            if m >= 0 && len > pos {
                best = m;
                end = len;
            }
        }
        if best < 0 || end == pos {
            return None;
        }
        let value = &text[pos..end];
        let id = BAKED_PATTERN_IDS[best as usize];
        Some((retype(&self.unless, id, value), value))
    }
}
"#;

fn emit_u32_slice(out: &mut String, name: &str, vals: &[u32]) {
    let _ = write!(out, "static {name}: &[u32] = &[");
    for (i, v) in vals.iter().enumerate() {
        if i % 16 == 0 {
            out.push_str("\n    ");
        } else {
            out.push(' ');
        }
        let _ = write!(out, "{v},");
    }
    out.push_str("\n];\n");
}

fn emit_i32_slice(out: &mut String, name: &str, vals: &[i32]) {
    let _ = write!(out, "static {name}: &[i32] = &[");
    for (i, v) in vals.iter().enumerate() {
        if i % 16 == 0 {
            out.push_str("\n    ");
        } else {
            out.push(' ');
        }
        let _ = write!(out, "{v},");
    }
    out.push_str("\n];\n");
}

fn spike_banner(variant: &str) -> String {
    format!(
        "// @generated SPIKE COPY (standalone-bake spike 2026-07-03, THROWAWAY) — \
         variant: {variant}.\n\
         // Regenerate: cargo run --release --features baked-dfa-spike --example \
         standalone_bake_gen\n"
    )
}

/// Build all four variant sources from one `generate_standalone` output.
fn build_variants(stock_src: &str, flat: &Flat, pattern_ids: &[u32]) -> Vec<(String, String)> {
    let mut out = Vec::new();

    // stock: verbatim + accessor.
    let stock = splice_before_mod_close(stock_src, ACCESSOR);
    out.push(("stock".to_string(), spike_banner("stock") + &stock));

    // lean (L5a analog): runtime dense DFA + lean loop.
    let lean_scanner = format!("{LEAN_SCANNER}{UNLESS_HELPERS}");
    let lean = splice_before_mod_close(&replace_scanner(stock_src, &lean_scanner), ACCESSOR);
    out.push(("lean".to_string(), spike_banner("lean") + &lean));

    // baked (L5b): static flat tables + interpreter loop.
    let mut tables = String::new();
    tables.push_str("\n// ── baked flat-table scanner data (L5b) ──\n");
    emit_u32_slice(&mut tables, "BAKED_TRANS", &flat.trans);
    emit_i32_slice(&mut tables, "BAKED_MATCH", &flat.match_pat);
    emit_i32_slice(&mut tables, "BAKED_EOI", &flat.eoi_pat);
    emit_u32_slice(&mut tables, "BAKED_PATTERN_IDS", pattern_ids);
    let baked_scanner = format!("{BAKED_SCANNER}{UNLESS_HELPERS}{tables}");
    let baked = splice_before_mod_close(&replace_scanner(stock_src, &baked_scanner), ACCESSOR);
    out.push(("baked".to_string(), spike_banner("baked") + &baked));

    // interned (L5d/M2 analog): `&'static str` token type / tree label.
    let mut interned = stock_src.to_string();
    interned = replace_exactly_once(
        interned,
        "    pub type_: String,",
        "    pub type_: &'static str,",
    );
    interned = replace_exactly_once(
        interned,
        "    pub data: String,",
        "    pub data: &'static str,",
    );
    interned = replace_exactly_once(
        interned,
        "            data: String,",
        "            data: &'static str,",
    );
    interned = replace_exactly_once(
        interned,
        "type_: data.name_of(id).to_string(),",
        "type_: data.name_of(id),",
    );
    interned = replace_exactly_once(
        interned,
        "type_: data.name_of(0).to_string(),",
        "type_: data.name_of(0),",
    );
    interned = replace_exactly_once(
        interned,
        "data: rule.tree_name.to_string(),",
        "data: rule.tree_name,",
    );
    // `t.data.clone()` on a `&'static str` is a no-op clone (a warning); drop it.
    interned = replace_exactly_once(interned, "data: t.data.clone(),", "data: t.data,");
    let interned = splice_before_mod_close(&interned, ACCESSOR);
    out.push(("interned".to_string(), spike_banner("interned") + &interned));

    out
}

// ─── Per-grammar processing ───────────────────────────────────────────────────

struct GrammarCase {
    name: String,
    grammar_src: String,
    options: LarkOptions,
    corpus: Vec<String>,
}

fn process(case: &GrammarCase, out_dir: &Path) {
    let name = &case.name;

    // 1. Standalone generation (the front door #620 targets).
    let t0 = Instant::now();
    let stock_src = match generate_standalone(&case.grammar_src, &case.options) {
        Ok(s) => s,
        Err(e) => {
            println!("SKIP\t{name}\tstandalone generation refused: {e}");
            return;
        }
    };
    let gen_ms = t0.elapsed().as_nanos() as f64 / 1e6;

    // 2. In-process basic lexer (Dfa backend) → the production dense DFA.
    let g = match load_grammar_with_base(
        &case.grammar_src,
        &case.options.start,
        case.options.maybe_placeholders,
        case.options.keep_all_tokens,
        case.options.base_path.clone(),
    ) {
        Ok(g) => g,
        Err(e) => {
            println!("SKIP\t{name}\tgrammar does not load in-process: {e}");
            return;
        }
    };
    let cg = lower(&g);
    let conf = basic_lexer_conf(&cg, case.options.g_regex_flags).with_backend(LexerBackend::Dfa);
    let t0 = Instant::now();
    let lexer = match BasicLexer::new(&conf) {
        Ok(l) => l,
        Err(e) => {
            println!("SKIP\t{name}\tin-process basic lexer does not build: {e}");
            return;
        }
    };
    let lexer_build_ms = t0.elapsed().as_nanos() as f64 / 1e6;
    let Some((dfa, ids)) = lexer.spike_plain_dense() else {
        println!("SKIP\t{name}\tscanner is not a single dense plain engine (guarded/fence/hybrid)");
        return;
    };
    let t0 = Instant::now();
    let Some(flat) = bake_flat(dfa) else {
        println!("SKIP\t{name}\tDFA does not bake (context-sensitive start or quit states)");
        return;
    };
    let bake_ms = t0.elapsed().as_nanos() as f64 / 1e6;
    let pattern_ids: Vec<u32> = ids.iter().map(|s| s.index() as u32).collect();

    // 3. L5c footprint sweep row.
    let classes = compress_classes(&flat);
    let n = flat.n_states;
    let bytes_256_u32 = n * 256 * 4;
    let bytes_256_u16 = n * 256 * 2;
    let bytes_cls_u32 = n * classes.n_classes * 4 + 256;
    let bytes_cls_u16 = n * classes.n_classes * 2 + 256;
    let to_bytes_len = dfa.to_bytes_little_endian().0.len();
    let table = lark_rs::parsers::lalr::build_lalr_table(&cg, false).expect("LALR table builds");
    let ctx_scanners = contextual_scanner_count(&table, &cg);
    println!(
        "\n{name}: {} patterns, {} baked states, {} byte classes; in-process dense DFA \
         memory {} KiB",
        pattern_ids.len(),
        n,
        classes.n_classes,
        dfa.memory_usage() / 1024
    );
    println!(
        "  L5c table bytes: 256×u32 {:>8} | 256×u16 {:>8} | cls×u32 {:>8} | cls×u16 {:>8} | \
         regex-automata to_bytes {:>8}",
        bytes_256_u32, bytes_256_u16, bytes_cls_u32, bytes_cls_u16, to_bytes_len
    );
    println!(
        "  L5c multiplier: standalone runtime lexer = BASIC (1 scanner/grammar); a contextual \
         standalone would need {ctx_scanners} deduped state scanners"
    );
    println!(
        "  one-shot context: generate_standalone {gen_ms:.2} ms, in-process BasicLexer::new \
         {lexer_build_ms:.2} ms, flat bake {bake_ms:.2} ms"
    );

    // Byte-class giveback: same-session flat vs class interpreter over the corpus.
    if !case.corpus.is_empty() {
        let bytes: usize = case.corpus.iter().map(|t| t.len()).sum();
        let (mut h_flat, mut h_cls) = (0u64, 0u64);
        for t in &case.corpus {
            h_flat = h_flat.wrapping_add(drive(t, |b, p| flat_match_at(&flat, b, p)).0);
            h_cls = h_cls.wrapping_add(drive(t, |b, p| class_match_at(&flat, &classes, b, p)).0);
        }
        assert_eq!(
            h_flat, h_cls,
            "{name}: class-compressed interpreter diverges from the flat table"
        );
        let flat_ns = measure(|| {
            for t in &case.corpus {
                std::hint::black_box(drive(t, |b, p| flat_match_at(&flat, b, p)));
            }
        });
        let cls_ns = measure(|| {
            for t in &case.corpus {
                std::hint::black_box(drive(t, |b, p| class_match_at(&flat, &classes, b, p)));
            }
        });
        let mbps = |ns: f64| bytes as f64 / ns * 1e3;
        println!(
            "  L5c giveback (scan-only, same-session): flat {:.1} MB/s, byte-class {:.1} MB/s \
             ({:.2}x of flat)",
            mbps(flat_ns),
            mbps(cls_ns),
            flat_ns / cls_ns
        );
    }

    // 4. Oracle-parse probe: which corpus inputs parse under the in-process
    //    basic-lexer LALR engine (the standalone runtime is basic-only, so this
    //    both defines the bench workload and confirms an oracle exists).
    let mut opts = case.options.clone();
    opts.lexer = LexerType::Basic;
    let oracle = match Lark::new(&case.grammar_src, opts) {
        Ok(l) => l,
        Err(e) => {
            println!("SKIP\t{name}\tin-process basic-lexer engine does not build: {e}");
            return;
        }
    };
    let parseable = case
        .corpus
        .iter()
        .filter(|t| oracle.parse(t).is_ok())
        .count();
    println!(
        "  oracle probe: {parseable}/{} corpus inputs parse under in-process basic-lexer LALR",
        case.corpus.len()
    );
    if parseable == 0 && !case.corpus.is_empty() {
        println!("SKIP\t{name}\tno corpus input parses under the basic lexer (contextual-dependent grammar) — not emitted");
        return;
    }

    // 5. Emit the variant copies.
    for (variant, src) in build_variants(&stock_src, &flat, &pattern_ids) {
        let path = out_dir.join(format!("{name}_{variant}.rs"));
        std::fs::write(&path, &src).expect("write variant copy");
        println!("  wrote {} ({} KiB)", path.display(), src.len() / 1024);
    }
}

/// Deduped per-state terminal-set count — what a *contextual* standalone bake
/// would multiply the table footprint by (the in-process `ContextualLexer` keys
/// its lazy scanners on exactly this set: state action-row terminals ∪ ignore).
fn contextual_scanner_count(
    table: &lark_rs::parsers::lalr::ParseTable,
    cg: &lark_rs::grammar::CompiledGrammar,
) -> usize {
    let ig: BTreeSet<u32> = cg.ignore.iter().map(|s| s.index() as u32).collect();
    let mut sets: BTreeSet<Vec<u32>> = BTreeSet::new();
    for row in &table.action {
        let mut s: BTreeSet<u32> = row.iter().map(|(t, _)| *t).filter(|t| *t != 0).collect();
        s.extend(ig.iter().copied());
        if s.is_empty() {
            continue;
        }
        sets.insert(s.into_iter().collect());
    }
    sets.len()
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

fn wild_case(pdir: &Path) -> Option<GrammarCase> {
    let meta: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(pdir.join("meta.json")).ok()?).ok()?;
    let name = meta["name"].as_str()?.to_string();
    let o = &meta["lark_options"];
    if o["parser"].as_str() != Some("lalr") {
        println!("SKIP\t{name}\tnon-LALR project (standalone is LALR-only)");
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
    let grammar_src = std::fs::read_to_string(pdir.join(meta["entry_grammar"].as_str()?)).ok()?;
    let options = LarkOptions {
        start: vec![o["start"].as_str()?.to_string()],
        parser: ParserAlgorithm::Lalr,
        maybe_placeholders: o["maybe_placeholders"].as_bool().unwrap_or(true),
        keep_all_tokens: o["keep_all_tokens"].as_bool().unwrap_or(false),
        g_regex_flags: flags,
        base_path: Some(pdir.join("grammar")),
        ..Default::default()
    };
    let corpus: Vec<String> = meta["inputs"]
        .as_object()?
        .keys()
        .filter_map(|rel| std::fs::read_to_string(pdir.join(rel)).ok())
        .collect();
    Some(GrammarCase {
        name,
        grammar_src,
        options,
        corpus,
    })
}

fn main() {
    let out_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/standalone_spike/gen");
    std::fs::create_dir_all(&out_dir).expect("create gen dir");

    println!("# standalone-bake spike: variant generation + L5c footprint sweep (#620)");

    // The canonical JSON bench grammar.
    process(
        &GrammarCase {
            name: "json".to_string(),
            grammar_src: JSON_GRAMMAR.to_string(),
            options: LarkOptions {
                start: vec!["start".to_string()],
                parser: ParserAlgorithm::Lalr,
                maybe_placeholders: true,
                keep_all_tokens: false,
                ..Default::default()
            },
            corpus: vec![gen_json(2000, 8)],
        },
        &out_dir,
    );

    // Every wild-bank project.
    let wild = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/wild");
    let mut projects: Vec<PathBuf> = std::fs::read_dir(&wild)
        .expect("tests/wild exists")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.join("meta.json").is_file())
        .collect();
    projects.sort();
    for pdir in &projects {
        if let Some(case) = wild_case(pdir) {
            process(&case, &out_dir);
        }
    }
}
