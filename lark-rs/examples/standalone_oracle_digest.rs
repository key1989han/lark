//! THROWAWAY SPIKE (L5 standalone-bake spike, 2026-07-03): the in-process oracle
//! digest for Part B's correctness gate. Parses `<grammar> <start> <workload>` with
//! the in-process `Lark` **basic-lexer** LALR engine (what standalone mirrors) and
//! prints a canonical structural digest of the tree — the same FNV-1a canonicalization
//! the generated-crate harness computes over its own `ParseTree`. Equal digests ⇒ the
//! stock/baked standalone parsers produce trees byte-identical to the oracle.
//!
//! ```text
//! cargo run --release --example standalone_oracle_digest -- <grammar.lark> <start> <workload>
//! ```

use lark_rs::{Lark, LarkOptions, LexerType, ParseTree, ParserAlgorithm};

fn push(h: &mut u64, bytes: &[u8]) {
    for &b in bytes {
        *h ^= b as u64;
        *h = h.wrapping_mul(0x100000001b3);
    }
}

fn walk_tree(t: &lark_rs::Tree, h: &mut u64, n: &mut u64) {
    *n += 1;
    push(h, b"T");
    push(h, t.data.as_bytes());
    push(h, b"(");
    for c in &t.children {
        match c {
            lark_rs::Child::Tree(st) => walk_tree(st, h, n),
            lark_rs::Child::Token(tok) => {
                push(h, b"t");
                push(h, tok.type_.as_bytes());
                push(h, b"=");
                push(h, tok.value.as_bytes());
                push(h, b";");
            }
            lark_rs::Child::None => push(h, b"N"),
        }
    }
    push(h, b")");
}

fn canon(pt: &ParseTree) -> (u64, u64) {
    let mut h = 0xcbf29ce484222325u64;
    let mut n = 0u64;
    match pt {
        ParseTree::Tree(t) => walk_tree(t, &mut h, &mut n),
        ParseTree::Token(tok) => {
            push(&mut h, b"t");
            push(&mut h, tok.type_.as_bytes());
            push(&mut h, b"=");
            push(&mut h, tok.value.as_bytes());
        }
        ParseTree::None => push(&mut h, b"N"),
    }
    (h, n)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let grammar_path = &args[0];
    let start = args[1].clone();
    let workload = &args[2];
    let grammar = std::fs::read_to_string(grammar_path).expect("grammar");
    let base = std::path::Path::new(grammar_path)
        .parent()
        .map(|p| p.to_path_buf());
    let text = std::fs::read_to_string(workload).expect("workload");

    let opts = LarkOptions {
        start: vec![start],
        parser: ParserAlgorithm::Lalr,
        lexer: LexerType::Basic,
        base_path: base,
        ..Default::default()
    };
    let lark = Lark::new(&grammar, opts).expect("oracle builds");
    let tree = lark.parse(&text).expect("oracle parses");
    let (digest, nodes) = canon(&tree);
    println!(
        "ORACLE digest={digest:016x} nodes={nodes} bytes={}",
        text.len()
    );
}
