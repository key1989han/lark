//! THROWAWAY SPIKE (parser-optimization spike 2026-07-03): generate the baked
//! (RE/flex `--fast`-style goto/switch) scanner for the JSON bench grammar.
//!
//! Extracts the plain engine's dense DFA out of the real `DfaScanner` (via the
//! `baked-dfa-spike` accessors), flattens it to compact `u32` states, and emits a
//! self-contained Rust `match_at` whose state dispatch is a compiled `match`
//! (per-state byte-range arms) instead of a table walk — the directly-executable
//! analog of the `regex-automata` table interpretation. The output is written to
//! `examples/baked/json_dfa.rs` and `include!`d by `examples/baked_dfa_lex.rs`.
//!
//! ```text
//! cargo run --release --features baked-dfa-spike --example bake_dfa_gen
//! ```
//!
//! Regenerate whenever the JSON grammar or the DFA construction changes; the
//! consuming example asserts baked output == live-scanner output over the whole
//! workload, so drift fails loudly there.

use std::collections::HashMap;
use std::fmt::Write as _;

use lark_rs::{basic_lexer_conf, load_grammar, lower, BasicLexer, LexerBackend};
use regex_automata::dfa::{dense, Automaton};
use regex_automata::{Anchored, Input};

// Byte-identical to the grammar in `baked_dfa_lex.rs` / `benches/lex_backends.rs`.
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

/// The flattened automaton: compact state 0 is the dead sentinel, `start` is the
/// anchored start state, `trans[state*256 + byte]` the next compact state,
/// `match_pat[state]` the leftmost-first winning `PatternID` (or -1) when the
/// state is a match state, `eoi_pat[state]` the same after the EOI transition.
pub struct Baked {
    pub trans: Vec<u32>,
    pub match_pat: Vec<i32>,
    pub eoi_pat: Vec<i32>,
    pub start: u32,
    pub n_states: usize,
}

/// BFS-flatten `dfa` (anchored, empty-context start). Returns `None` when the
/// automaton is context-sensitive at the start (a look-behind-dependent start
/// state) or reaches a quit state — preconditions under which a single baked
/// start state would not be faithful.
pub fn bake(dfa: &dense::DFA<Vec<u32>>) -> Option<Baked> {
    let start_sid = dfa
        .start_state_forward(&Input::new("").anchored(Anchored::Yes))
        .ok()?;
    // Start-state stability probe: the anchored start must not depend on the byte
    // before `pos` (no look-behind/anchor-conditioned start configurations).
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
                continue; // stays 0, the dead sentinel
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
        start: 1,
        n_states: n,
    })
}

/// Render one state's 256-entry row as compressed `match b { ranges => next }` arms.
fn byte_arms(row: &[u32]) -> String {
    // Group contiguous byte ranges with the same target; the dead target (0) is
    // the `_` arm.
    let mut arms: Vec<(u8, u8, u32)> = Vec::new();
    let mut b = 0usize;
    while b < 256 {
        let tgt = row[b];
        let s = b;
        while b + 1 < 256 && row[b + 1] == tgt {
            b += 1;
        }
        if tgt != 0 {
            arms.push((s as u8, b as u8, tgt));
        }
        b += 1;
    }
    let mut out = String::new();
    for (s, e, tgt) in arms {
        if s == e {
            let _ = write!(out, "{s} => {tgt}, ");
        } else {
            let _ = write!(out, "{s}..={e} => {tgt}, ");
        }
    }
    out.push_str("_ => 0,");
    out
}

fn main() {
    let g = load_grammar(JSON_GRAMMAR, &["start".to_string()], true, false)
        .expect("json grammar loads");
    let cg = lower(&g);
    let conf = basic_lexer_conf(&cg, 0).with_backend(LexerBackend::Dfa);
    let lexer = BasicLexer::new(&conf).expect("json lexer builds");
    let (dfa, ids) = lexer
        .spike_plain_dense()
        .expect("json scanner is a single dense plain engine");
    let baked = bake(dfa).expect("json DFA bakes (context-free start, no quit states)");

    let mut src = String::new();
    src.push_str(
        "// @generated by `cargo run --release --features baked-dfa-spike --example \
         bake_dfa_gen`\n\
         // THROWAWAY SPIKE (parser-optimization spike 2026-07-03): the baked\n\
         // (goto/switch-style) scanner for the JSON bench grammar — the directly-\n\
         // executable form of the same dense DFA the live DfaScanner interprets.\n\
         // Do not edit; regenerate. Consumed (include!) by examples/baked_dfa_lex.rs.\n\n",
    );
    let _ = writeln!(
        src,
        "// {} states (incl. dead sentinel 0), {} patterns.",
        baked.n_states,
        ids.len()
    );
    src.push_str(
        "/// Anchored leftmost-first match at `pos`: `(pattern id or -1, match end)`.\n\
         /// Same delayed-by-one convention as the flat-table interpreter; zero-width\n\
         /// matches rejected.\n\
         #[allow(unreachable_patterns, clippy::match_overlapping_arm, clippy::manual_range_patterns)]\n\
         pub fn match_at(bytes: &[u8], pos: usize) -> (i32, usize) {\n\
         \x20   let len = bytes.len();\n\
         \x20   let mut bp: i32 = -1;\n\
         \x20   let mut be: usize = pos;\n\
         \x20   let mut st: u32 = 1;\n\
         \x20   let mut i = pos;\n\
         \x20   while i < len {\n\
         \x20       let b = bytes[i];\n\
         \x20       st = match st {\n",
    );
    for c in 1..baked.n_states {
        let _ = writeln!(
            src,
            "            {c} => match b {{ {} }},",
            byte_arms(&baked.trans[c * 256..(c + 1) * 256])
        );
    }
    src.push_str("            _ => 0,\n        };\n        match st {\n");
    src.push_str("            0 => return (bp, be),\n");
    for c in 1..baked.n_states {
        if baked.match_pat[c] >= 0 {
            let _ = writeln!(
                src,
                "            {c} => {{ if i > pos {{ bp = {}; be = i; }} }}",
                baked.match_pat[c]
            );
        }
    }
    src.push_str("            _ => {}\n        }\n        i += 1;\n    }\n");
    // EOI transitions: bake only the states whose EOI step lands on a match.
    src.push_str("    match st {\n");
    for c in 1..baked.n_states {
        if baked.eoi_pat[c] >= 0 {
            let _ = writeln!(
                src,
                "        {c} => {{ if len > pos {{ bp = {}; be = len; }} }}",
                baked.eoi_pat[c]
            );
        }
    }
    src.push_str("        _ => {}\n    }\n    (bp, be)\n}\n");

    let out =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/baked/json_dfa.rs");
    std::fs::create_dir_all(out.parent().unwrap()).unwrap();
    std::fs::write(&out, &src).unwrap();
    println!(
        "wrote {} ({} states, {} patterns, {} bytes of source)",
        out.display(),
        baked.n_states,
        ids.len(),
        src.len()
    );
}
