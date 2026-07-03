//! THROWAWAY SPIKE (L5 standalone-bake spike, 2026-07-03): emit the Part-B
//! generated-crate variants for issue #620 — a STOCK standalone parser (regex-crate
//! `Scanner`) and two BAKED copies (flat `u32[state*256]` table and byte-class-
//! compressed table) that splice a static-table interpreter into the *same* generated
//! runtime, dropping the `regex` dependency. Compiling these three crates is how the
//! spike measures binary size, rustc compile time, and one-shot vs reused parse.
//!
//! ```text
//! cargo run --release --features baked-dfa-spike --example standalone_bake_emit -- \
//!     <grammar.lark> <start> <out_dir>
//! ```
//! Writes `<out_dir>/{stock,baked,baked_classed}.rs`. Correctness is gated downstream
//! by the driver script (token stream + tree vs the in-process oracle).

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::PathBuf;

use lark_rs::grammar::load_grammar_with_base;
use lark_rs::grammar::terminal::TerminalDef;
use lark_rs::lexer::scanner_plan;
use lark_rs::{
    basic_lexer_conf, generate_standalone, lower, BasicLexer, LarkOptions, LexerBackend,
    ParserAlgorithm, SymbolId,
};
use regex_automata::dfa::{dense, Automaton};
use regex_automata::{Anchored, Input};

struct Baked {
    trans: Vec<u32>,
    match_pat: Vec<i32>,
    eoi_pat: Vec<i32>,
    n_states: usize,
    pat2id: Vec<u32>,
    class_of: [u8; 256],
    n_bclasses: usize,
    ctrans: Vec<u32>,
}

fn bake(dfa: &dense::DFA<Vec<u32>>, ids: &[SymbolId]) -> Option<Baked> {
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
    let mut idmap: HashMap<_, u32> = HashMap::new();
    let mut order = vec![start_sid];
    idmap.insert(start_sid, 1);
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
            row[b as usize] = *idmap.entry(n).or_insert_with(|| {
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
    let pat2id = ids.iter().map(|s| s.index() as u32).collect();
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
            ctrans[st * n_bclasses + class_of[b] as usize] = trans[st * 256 + b];
        }
    }
    Some(Baked {
        trans,
        match_pat,
        eoi_pat,
        n_states: n,
        pat2id,
        class_of,
        n_bclasses,
        ctrans,
    })
}

fn arr_u32(name: &str, v: &[u32]) -> String {
    let mut s = format!("static {name}: &[u32] = &[");
    for (i, x) in v.iter().enumerate() {
        if i % 32 == 0 {
            s.push_str("\n    ");
        }
        let _ = write!(s, "{x},");
    }
    s.push_str("\n];\n");
    s
}
fn arr_i32(name: &str, v: &[i32]) -> String {
    let mut s = format!("static {name}: &[i32] = &[");
    for (i, x) in v.iter().enumerate() {
        if i % 32 == 0 {
            s.push_str("\n    ");
        }
        let _ = write!(s, "{x},");
    }
    s.push_str("\n];\n");
    s
}
fn arr_u8(name: &str, v: &[u8], n: usize) -> String {
    let mut s = format!("static {name}: [u8; {n}] = [");
    for (i, x) in v.iter().enumerate() {
        if i % 32 == 0 {
            s.push_str("\n    ");
        }
        let _ = write!(s, "{x},");
    }
    s.push_str("\n];\n");
    s
}

/// The regex-free unless map + match_at body shared by both baked variants. `scan_body`
/// is the per-variant inner-loop transition (flat vs class-compressed).
fn baked_scanner_common(scan_fn: &str) -> String {
    format!(
        r#"
struct Scanner {{
    // Regex-free keyword retype: exact values hashed; case-insensitive keywords
    // compared with `eq_ignore_ascii_case` (the bundled ci keywords are ASCII).
    unless: HashMap<u32, (HashMap<String, u32>, Vec<(String, u32)>)>,
}}

impl Scanner {{
    fn new(data: &GrammarData) -> Scanner {{
        let mut unless: HashMap<u32, (HashMap<String, u32>, Vec<(String, u32)>)> = HashMap::new();
        for (re_id, entries) in data.unless {{
            let slot = unless.entry(*re_id).or_default();
            for (value, ci, kw_id) in *entries {{
                if *ci {{
                    slot.1.push((value.to_string(), *kw_id));
                }} else {{
                    slot.0.entry(value.to_string()).or_insert(*kw_id);
                }}
            }}
        }}
        Scanner {{ unless }}
    }}

{scan_fn}

    fn match_at<'t>(&self, text: &'t str, pos: usize) -> Option<(u32, &'t str)> {{
        let (pat, end) = self.scan(text.as_bytes(), pos);
        if pat < 0 || end <= pos {{
            return None;
        }}
        let value = &text[pos..end];
        let id = PAT2ID[pat as usize];
        let ty = self
            .unless
            .get(&id)
            .and_then(|(exact, ci)| {{
                exact.get(value).copied().or_else(|| {{
                    ci.iter()
                        .find(|(kw, _)| value.eq_ignore_ascii_case(kw))
                        .map(|(_, kid)| *kid)
                }})
            }})
            .unwrap_or(id);
        Some((ty, value))
    }}
}}
"#
    )
}

fn splice(stock: &str, scanner_src: &str) -> String {
    let start = stock
        .find("struct Scanner {")
        .expect("Scanner struct present");
    let end = stock
        .find("fn lex(data: &GrammarData")
        .expect("lex fn present");
    let mut out = String::new();
    out.push_str(&stock[..start]);
    out.push_str(scanner_src.trim_start());
    out.push('\n');
    out.push_str(&stock[end..]);
    // Drop the regex dependency entirely (baked scanner + regex-free unless).
    out.replace("use regex::Regex;\n", "")
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 3 {
        eprintln!("usage: standalone_bake_emit <grammar.lark> <start> <out_dir>");
        std::process::exit(2);
    }
    let grammar_path = PathBuf::from(&args[0]);
    let start = args[1].clone();
    let out_dir = PathBuf::from(&args[2]);
    std::fs::create_dir_all(&out_dir).unwrap();

    let grammar = std::fs::read_to_string(&grammar_path).expect("read grammar");
    let base = grammar_path.parent().map(|p| p.to_path_buf());
    let opts = LarkOptions {
        start: vec![start.clone()],
        parser: ParserAlgorithm::Lalr,
        base_path: base.clone(),
        ..Default::default()
    };

    // 1) stock
    let stock = generate_standalone(&grammar, &opts).expect("generate stock");
    std::fs::write(out_dir.join("stock.rs"), &stock).unwrap();

    // 2) bake the plain dense DFA
    let g = load_grammar_with_base(&grammar, &[start], true, false, base).expect("load grammar");
    let cg = lower(&g);
    let conf = basic_lexer_conf(&cg, opts.g_regex_flags).with_backend(LexerBackend::Dfa);
    let _term_refs: Vec<(SymbolId, &TerminalDef)> =
        conf.terminals.iter().map(|(id, t)| (*id, t)).collect();
    let _ = scanner_plan(&_term_refs, conf.global_flags).expect("plan");
    let lexer = BasicLexer::new(&conf).expect("lexer");
    let (dfa, ids) = lexer
        .spike_plain_dense()
        .expect("single dense plain engine");
    let baked = bake(dfa, &ids).expect("DFA flattens");

    let tables_flat = format!(
        "// ── baked flat DFA tables (L5b: {} states × 256, {} KiB rodata) ──\n{}{}{}{}",
        baked.n_states,
        baked.n_states * 256 * 4 / 1024,
        arr_u32("TRANS", &baked.trans),
        arr_i32("MATCHP", &baked.match_pat),
        arr_i32("EOIP", &baked.eoi_pat),
        arr_u32("PAT2ID", &baked.pat2id),
    );
    let scan_flat = r#"    #[inline]
    fn scan(&self, bytes: &[u8], pos: usize) -> (i32, usize) {
        let len = bytes.len();
        let mut st = 1usize;
        let (mut bp, mut be, mut i) = (-1i32, pos, pos);
        while i < len {
            st = TRANS[(st << 8) + bytes[i] as usize] as usize;
            let m = MATCHP[st];
            if m >= 0 {
                if i > pos { bp = m; be = i; }
            } else if st == 0 {
                return (bp, be);
            }
            i += 1;
        }
        let m = EOIP[st];
        if m >= 0 && len > pos { bp = m; be = len; }
        (bp, be)
    }"#;
    let baked_src = format!("{tables_flat}\n{}", baked_scanner_common(scan_flat));
    std::fs::write(out_dir.join("baked.rs"), splice(&stock, &baked_src)).unwrap();

    // 3) byte-class compressed
    let tables_classed = format!(
        "// ── baked byte-class DFA tables (L5c: {} states × {} classes, {} KiB rodata) ──\n{}{}{}{}{}",
        baked.n_states,
        baked.n_bclasses,
        (baked.n_states * baked.n_bclasses * 4 + 256) / 1024,
        arr_u32("CTRANS", &baked.ctrans),
        arr_u8("CLASSOF", &baked.class_of, 256),
        arr_i32("MATCHP", &baked.match_pat),
        arr_i32("EOIP", &baked.eoi_pat),
        arr_u32("PAT2ID", &baked.pat2id),
    );
    let scan_classed = format!(
        r#"    #[inline]
    fn scan(&self, bytes: &[u8], pos: usize) -> (i32, usize) {{
        const NC: usize = {nc};
        let len = bytes.len();
        let mut st = 1usize;
        let (mut bp, mut be, mut i) = (-1i32, pos, pos);
        while i < len {{
            let cl = CLASSOF[bytes[i] as usize] as usize;
            st = CTRANS[st * NC + cl] as usize;
            let m = MATCHP[st];
            if m >= 0 {{
                if i > pos {{ bp = m; be = i; }}
            }} else if st == 0 {{
                return (bp, be);
            }}
            i += 1;
        }}
        let m = EOIP[st];
        if m >= 0 && len > pos {{ bp = m; be = len; }}
        (bp, be)
    }}"#,
        nc = baked.n_bclasses
    );
    let baked_classed_src = format!("{tables_classed}\n{}", baked_scanner_common(&scan_classed));
    std::fs::write(
        out_dir.join("baked_classed.rs"),
        splice(&stock, &baked_classed_src),
    )
    .unwrap();

    println!(
        "emitted stock.rs ({} B), baked.rs, baked_classed.rs to {} ({} states, {} classes)",
        stock.len(),
        out_dir.display(),
        baked.n_states,
        baked.n_bclasses
    );
}
