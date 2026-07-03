//! THROWAWAY SPIKE (L5 standalone-bake spike, 2026-07-03): reconnaissance for
//! issue #620. Answers the two structural questions the epic's cost model turns on,
//! before any bake is written:
//!
//!   1. **Which grammars can the standalone backend bake at all** (`generate_standalone`
//!      accepts) — and of those, whose plain scanner **DFA-flattens** (single dense
//!      engine, context-insensitive start; the `bake()` precondition). That is the
//!      qualifying set for L5b.
//!   2. **Footprint per qualifying scanner** at three table encodings — full 256-wide
//!      `u32[state*256]`, byte-class-compressed `u32[state*classes]` (+256-byte map),
//!      and `regex-automata` `to_bytes` — the L5c axis. Multiplied by the standalone
//!      lexer's scanner count, which is the headline: **standalone bakes ONE
//!      basic-lexer scanner, not the 47-108 deduped contextual scanners #620's cost
//!      model assumes** (mod.rs: "Basic lexer only").
//!
//! ```text
//! cargo run --release --features baked-dfa-spike --example standalone_bake_probe
//! ```
//! Deterministic signals only (byte counts + state counts); no timing here — Part A
//! (`standalone_bake_lex`) does the throughput/differential work.

use std::path::PathBuf;

use lark_rs::grammar::load_grammar_with_base;
use lark_rs::{
    basic_lexer_conf, generate_standalone, load_grammar, lower, BasicLexer, LarkOptions,
    LexerBackend, ParserAlgorithm,
};
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

/// BFS-flatten the dense DFA (same start-context probe + reachability as
/// `baked_dfa_lex::bake`), returning the number of live states (incl. the reserved
/// dead state 0) or `None` if the start is context-sensitive / has quit states.
fn bake_n_states(dfa: &dense::DFA<Vec<u32>>) -> Option<usize> {
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
    use std::collections::HashMap;
    let mut ids: HashMap<_, u32> = HashMap::new();
    let mut order = vec![start_sid];
    ids.insert(start_sid, 1u32);
    let mut qi = 0;
    while qi < order.len() {
        let sid = order[qi];
        qi += 1;
        for b in 0..=255u8 {
            let n = dfa.next_state(sid, b);
            if dfa.is_quit_state(n) {
                return None;
            }
            if dfa.is_dead_state(n) {
                continue;
            }
            ids.entry(n).or_insert_with(|| {
                order.push(n);
                order.len() as u32
            });
        }
    }
    Some(order.len() + 1)
}

struct Row {
    name: String,
    standalone: Result<usize, String>, // Ok(source bytes) or Err(reason)
    dense_qualifies: Option<String>,   // None = qualifies; Some(reason) = disqualified
    n_states: Option<usize>,
    n_classes: Option<usize>,
    flat_bytes: Option<usize>,
    classed_bytes: Option<usize>,
    to_bytes: Option<usize>,
}

fn probe(name: &str, grammar: &str, opts: &LarkOptions, base: Option<PathBuf>) -> Row {
    let mut opts = opts.clone();
    opts.base_path = base.clone();
    opts.parser = ParserAlgorithm::Lalr;

    // (1) Does the standalone backend accept this grammar?
    let standalone = match generate_standalone(grammar, &opts) {
        Ok(src) => Ok(src.len()),
        Err(e) => Err(short(&e.to_string())),
    };

    // (2) Build the in-process DFA basic lexer and probe the plain dense engine.
    let mut row = Row {
        name: name.to_string(),
        standalone,
        dense_qualifies: Some("(lexer did not build)".into()),
        n_states: None,
        n_classes: None,
        flat_bytes: None,
        classed_bytes: None,
        to_bytes: None,
    };

    let g = match load_grammar_with_base(
        grammar,
        &opts.start,
        opts.maybe_placeholders,
        opts.keep_all_tokens,
        base,
    ) {
        Ok(g) => g,
        Err(e) => {
            row.dense_qualifies = Some(format!("grammar build err: {}", short(&e.to_string())));
            return row;
        }
    };
    let cg = lower(&g);
    let conf = basic_lexer_conf(&cg, opts.g_regex_flags).with_backend(LexerBackend::Dfa);
    let lexer = match BasicLexer::new(&conf) {
        Ok(l) => l,
        Err(e) => {
            row.dense_qualifies = Some(format!("basic lexer err: {}", short(&e.to_string())));
            return row;
        }
    };
    let Some((dfa, _ids)) = lexer.spike_plain_dense() else {
        row.dense_qualifies =
            Some("guarded/fence/hybrid/overflow (not a single dense engine)".into());
        return row;
    };
    let Some(n_states) = bake_n_states(dfa) else {
        row.dense_qualifies =
            Some("DFA does not flatten (context-sensitive start / quit states)".into());
        return row;
    };
    let n_classes = dfa.byte_classes().alphabet_len();
    let (bytes, _pad) = dfa.to_bytes_native_endian();
    row.dense_qualifies = None;
    row.n_states = Some(n_states);
    row.n_classes = Some(n_classes);
    row.flat_bytes = Some(n_states * 256 * 4);
    row.classed_bytes = Some(n_states * n_classes * 4 + 256);
    row.to_bytes = Some(bytes.len());
    row
}

fn short(s: &str) -> String {
    let s = s.replace('\n', " ");
    if s.len() > 60 {
        format!("{}…", &s[..60])
    } else {
        s
    }
}

fn wild_opts(meta: &serde_json::Value) -> Option<(String, LarkOptions, PathBuf)> {
    let o = &meta["lark_options"];
    if o["parser"].as_str() != Some("lalr") {
        return None;
    }
    let start = o["start"].as_str()?.to_string();
    let mut flags = 0u32;
    if let Some(letters) = o["g_regex_flags"].as_str() {
        use lark_rs::grammar::terminal::flags as tf;
        for ch in letters.chars() {
            flags |= match ch {
                'i' => tf::IGNORECASE,
                'm' => tf::MULTILINE,
                's' => tf::DOTALL,
                'x' => tf::VERBOSE,
                _ => 0,
            };
        }
    }
    let opts = LarkOptions {
        start: vec![start],
        parser: ParserAlgorithm::Lalr,
        maybe_placeholders: o["maybe_placeholders"].as_bool().unwrap_or(true),
        keep_all_tokens: o["keep_all_tokens"].as_bool().unwrap_or(false),
        g_regex_flags: flags,
        ..Default::default()
    };
    let entry = meta["entry_grammar"].as_str()?.to_string();
    Some((entry, opts, PathBuf::new()))
}

fn main() {
    let mut rows: Vec<Row> = Vec::new();

    // JSON synthetic (canonical bench grammar; imports common.* from the bundled lib).
    let json_opts = LarkOptions {
        start: vec!["start".to_string()],
        parser: ParserAlgorithm::Lalr,
        ..Default::default()
    };
    // sanity: it must load in-process
    let _ = load_grammar(JSON_GRAMMAR, &["start".to_string()], true, false).expect("json loads");
    rows.push(probe("json", JSON_GRAMMAR, &json_opts, None));

    // Wild bank: every LALR project.
    let wild = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/wild");
    let mut projects: Vec<PathBuf> = std::fs::read_dir(&wild)
        .expect("tests/wild exists")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.join("meta.json").is_file())
        .collect();
    projects.sort();
    for pdir in &projects {
        let Ok(meta) = serde_json::from_str::<serde_json::Value>(
            &std::fs::read_to_string(pdir.join("meta.json")).unwrap_or_default(),
        ) else {
            continue;
        };
        let name = meta["name"].as_str().unwrap_or("?").to_string();
        let Some((entry, opts, _)) = wild_opts(&meta) else {
            rows.push(Row {
                name: format!("{name} (non-LALR)"),
                standalone: Err("skipped: non-LALR".into()),
                dense_qualifies: Some("non-LALR".into()),
                n_states: None,
                n_classes: None,
                flat_bytes: None,
                classed_bytes: None,
                to_bytes: None,
            });
            continue;
        };
        let grammar_path = pdir.join(&entry);
        let grammar = std::fs::read_to_string(&grammar_path).unwrap_or_default();
        let base = grammar_path.parent().map(|p| p.to_path_buf());
        rows.push(probe(&name, &grammar, &opts, base));
    }

    // ── report ──
    println!("# L5 standalone-bake probe (issue #620): bakeable set + footprint\n");
    println!(
        "{:<18} {:<28} {:<40} {:>7} {:>6} {:>10} {:>10} {:>10}",
        "grammar",
        "standalone",
        "dense-flatten?",
        "states",
        "class",
        "flat KiB",
        "classKiB",
        "toB KiB"
    );
    println!("{}", "-".repeat(140));
    for r in &rows {
        let sa = match &r.standalone {
            Ok(n) => format!("bakes ({} B src)", n),
            Err(e) => format!("NO: {e}"),
        };
        let dense = match &r.dense_qualifies {
            None => "YES".to_string(),
            Some(reason) => format!("no: {reason}"),
        };
        let kib = |b: Option<usize>| {
            b.map(|b| format!("{:.1}", b as f64 / 1024.0))
                .unwrap_or_else(|| "-".into())
        };
        println!(
            "{:<18} {:<28} {:<40} {:>7} {:>6} {:>10} {:>10} {:>10}",
            r.name,
            sa,
            dense,
            r.n_states
                .map(|n| n.to_string())
                .unwrap_or_else(|| "-".into()),
            r.n_classes
                .map(|n| n.to_string())
                .unwrap_or_else(|| "-".into()),
            kib(r.flat_bytes),
            kib(r.classed_bytes),
            kib(r.to_bytes),
        );
    }

    println!("\n# Notes:");
    println!("# - 'standalone' = generate_standalone() verdict (the L5b-bakeable set).");
    println!("# - 'dense-flatten?' = plain scanner is a single dense engine AND bake()'s");
    println!("#   context-insensitive-start precondition holds (the L5b table-emittable set).");
    println!("# - flat KiB = states*256*4; classKiB = states*classes*4 + 256 (byte-class map);");
    println!("#   toB KiB = regex-automata dense::to_bytes_native_endian length.");
    println!("# - MULTIPLIER: the standalone runtime bakes ONE basic-lexer scanner, so the");
    println!("#   per-grammar rodata is x1 of the above — NOT x(47-108) deduped contextual");
    println!("#   scanners (#620's worst-case assumption, which is an in-process-only number).");
}
