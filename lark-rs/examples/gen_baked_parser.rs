//! THROWAWAY SPIKE (standalone-bake spike 2026-07-03, issue #620): emit two
//! self-contained JSON parser crates — a **stock** `generate_standalone` parser
//! (regex combined-alternation scanner) and a **baked** copy whose `Scanner` is
//! replaced by a flat `u32[state×256]` opcode-table interpreter (L5b) with the
//! `regex` dependency removed — into an output directory, so a script can compile
//! both and measure the one-shot / compile-time / binary-size deltas #620 asks for.
//!
//! The baked splice is surgical: it swaps ONLY the `Scanner` struct+impl (whose
//! surface used by the rest of the runtime is exactly `Scanner::new(&data)` and
//! `match_at(text,pos) -> Option<(u32,&str)>`) and drops the `use regex::Regex;`
//! line. Every other line — `run`/`assemble`/`shape`/`Tree` — is byte-identical, so
//! a tree divergence between the two crates is a bake bug, gated by the harness's
//! per-parse tree digest.
//!
//! ```text
//! cargo run --release --features baked-dfa-spike --example gen_baked_parser -- <out_dir>
//! ```

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use lark_rs::{
    basic_lexer_conf, generate_standalone, load_grammar, lower, BasicLexer, LarkOptions,
    LexerBackend, SymbolId,
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

struct Baked {
    trans: Vec<u32>,
    match_pat: Vec<i32>,
    eoi_pat: Vec<i32>,
    n_states: usize,
    pat_to_term: Vec<u32>,
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
    let pat_to_term = ids.iter().map(|s| s.index() as u32).collect();
    Some(Baked {
        trans,
        match_pat,
        eoi_pat,
        n_states: n,
        pat_to_term,
    })
}

/// The baked `Scanner` block that replaces the runtime's regex one. Same public
/// surface (`new` / `match_at`), backed by the injected `SCAN_*` statics.
fn baked_scanner_block(b: &Baked) -> String {
    let mut s = String::new();
    // Statics.
    let list_u32 = |v: &[u32]| {
        v.iter()
            .map(|x| x.to_string())
            .collect::<Vec<_>>()
            .join(",")
    };
    let list_i32 = |v: &[i32]| {
        v.iter()
            .map(|x| x.to_string())
            .collect::<Vec<_>>()
            .join(",")
    };
    let _ = writeln!(s, "static SCAN_TRANS: &[u32] = &[{}];", list_u32(&b.trans));
    let _ = writeln!(
        s,
        "static SCAN_MATCH: &[i32] = &[{}];",
        list_i32(&b.match_pat)
    );
    let _ = writeln!(s, "static SCAN_EOI: &[i32] = &[{}];", list_i32(&b.eoi_pat));
    let _ = writeln!(
        s,
        "static PAT_TO_TERM: &[u32] = &[{}];",
        list_u32(&b.pat_to_term)
    );
    s.push_str(
        r#"
struct Scanner;

impl Scanner {
    // Baked: no regex. The `data` argument is ignored (the tables are static);
    // this grammar has an empty `unless` map, so PAT_TO_TERM is the final id.
    fn new(data: &GrammarData) -> Scanner {
        assert!(data.unless.is_empty(), "baked spike scanner: unless not supported");
        Scanner
    }

    fn match_at<'t>(&self, text: &'t str, pos: usize) -> Option<(u32, &'t str)> {
        let bytes = text.as_bytes();
        let len = bytes.len();
        let mut st = 1usize;
        let mut bp: i32 = -1;
        let mut be = pos;
        let mut i = pos;
        let mut dead = false;
        while i < len {
            st = SCAN_TRANS[(st << 8) + bytes[i] as usize] as usize;
            let m = SCAN_MATCH[st];
            if m >= 0 {
                if i > pos {
                    bp = m;
                    be = i;
                }
            } else if st == 0 {
                dead = true;
                break;
            }
            i += 1;
        }
        if !dead {
            let m = SCAN_EOI[st];
            if m >= 0 && len > pos {
                bp = m;
                be = len;
            }
        }
        if bp >= 0 && be > pos {
            Some((PAT_TO_TERM[bp as usize], &text[pos..be]))
        } else {
            None
        }
    }
}
"#,
    );
    s
}

/// Splice the stock generated source into a baked one: drop the `use regex::Regex;`
/// line and replace the `struct Scanner … impl Scanner { … }` region with the baked
/// block.
fn make_baked(stock: &str, b: &Baked) -> String {
    let stock = stock.replacen("use regex::Regex;\n", "", 1);
    let start = stock
        .find("struct Scanner {")
        .expect("Scanner struct present");
    // The runtime places `fn lex(` immediately after the Scanner impl.
    let end_rel = stock[start..]
        .find("\nfn lex(")
        .expect("fn lex after Scanner");
    let end = start + end_rel + 1; // keep the leading '\n' before fn lex
    let mut out = String::with_capacity(stock.len() + b.trans.len() * 6);
    out.push_str(&stock[..start]);
    out.push_str(&baked_scanner_block(b));
    out.push('\n');
    out.push_str(&stock[end..]);
    out
}

const MAIN_RS: &str = r#"mod parser;
use std::time::{Duration, Instant};

fn fnv(bytes: &[u8]) -> u64 {
    let mut h = 0xcbf29ce484222325u64;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn main() {
    let path = std::env::args().nth(1).expect("usage: <bin> <input>");
    let text = std::fs::read_to_string(&path).expect("read input");

    // One-shot: fresh Parser (compiles the scanner) + the first parse.
    let t = Instant::now();
    let p = parser::Parser::new();
    let first = p.parse(&text).expect("parse ok");
    let one_shot = t.elapsed();
    let digest = first.to_string();

    // Reused: parse-only, min of many.
    let mut best = Duration::from_secs(3600);
    for _ in 0..80 {
        let t = Instant::now();
        let tr = p.parse(&text).expect("ok");
        best = best.min(t.elapsed());
        std::hint::black_box(&tr);
    }
    println!("ONESHOT_NS {}", one_shot.as_nanos());
    println!("REUSED_NS {}", best.as_nanos());
    println!("BYTES {}", text.len());
    println!("DIGEST_LEN {}", digest.len());
    println!("DIGEST_HASH {:016x}", fnv(digest.as_bytes()));
}
"#;

fn cargo_toml(name: &str, with_regex: bool) -> String {
    let dep = if with_regex { "regex = \"1.10\"\n" } else { "" };
    format!(
        "[package]\nname = \"{name}\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\n\
         [[bin]]\nname = \"{name}\"\npath = \"src/main.rs\"\n\n\
         [dependencies]\n{dep}\n\
         [profile.release]\nstrip = true\nlto = false\n"
    )
}

fn write_crate(dir: &Path, name: &str, parser_src: &str, with_regex: bool) {
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("Cargo.toml"), cargo_toml(name, with_regex)).unwrap();
    std::fs::write(dir.join("src/main.rs"), MAIN_RS).unwrap();
    std::fs::write(dir.join("src/parser.rs"), parser_src).unwrap();
}

fn main() {
    let out = PathBuf::from(std::env::args().nth(1).expect("usage: <bin> <out_dir>"));

    let opts = LarkOptions {
        start: vec!["start".to_string()],
        maybe_placeholders: false,
        ..Default::default()
    };
    let stock = generate_standalone(JSON_GRAMMAR, &opts).expect("generate stock");

    // Bake the same grammar's basic-lexer DFA.
    let g = load_grammar(JSON_GRAMMAR, &["start".to_string()], false, false).expect("loads");
    let cg = lower(&g);
    let conf = basic_lexer_conf(&cg, 0).with_backend(LexerBackend::Dfa);
    let lexer = BasicLexer::new(&conf).expect("dfa lexer");
    let (dfa, ids) = lexer
        .spike_plain_dense()
        .expect("json is a plain dense engine");
    let baked = bake(dfa, &ids).expect("json bakes");
    let baked_src = make_baked(&stock, &baked);

    write_crate(&out.join("stock"), "stock_json", &stock, true);
    write_crate(&out.join("baked"), "baked_json", &baked_src, false);

    eprintln!(
        "wrote stock ({} B parser.rs) + baked ({} B parser.rs, {} states, {} KiB table) to {}",
        stock.len(),
        baked_src.len(),
        baked.n_states,
        baked.n_states * 256 * 4 / 1024,
        out.display()
    );
}
