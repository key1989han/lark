//! Flat-tape output backend prototype (#243 C8c, perf spike 2026-07-02) — the
//! relative-oracle + counter gate, mirroring `tests/test_span_tree.rs`.
//!
//! The tape is beyond-oracle in *representation* only, so per ADR-0026 it ships
//! under the **relative oracle**: [`TapeTree::materialize`] must reproduce, byte
//! for byte, the tree `parse()` returns — over curated shaping-heavy grammars
//! and over the whole LALR compliance bank (no XFAIL list: relative, so any
//! divergence is a real bug). The counter gate proves the tape path builds no
//! generic output (`tree_nodes_built == 0`, `token_value_string_bytes == 0`,
//! `lexer_token_value_bytes == 0`).
//!
//! Gated on `--features tape-tree`; the counter gate additionally needs
//! `perf-counters`.

#![cfg(feature = "tape-tree")]

use lark_rs::{Lark, LarkOptions, LexerType, ParseTree, ParserAlgorithm};

fn lark(grammar: &str, lexer: LexerType, propagate: bool) -> Lark {
    Lark::new(
        grammar,
        LarkOptions {
            parser: ParserAlgorithm::Lalr,
            lexer,
            start: vec!["start".to_string()],
            propagate_positions: propagate,
            ..Default::default()
        },
    )
    .expect("grammar builds")
}

const ARITH: &str = r#"
    start: sum
    ?sum: product | sum "+" product | sum "-" product
    ?product: atom | product "*" atom | product "/" atom
    ?atom: NUMBER | "(" sum ")"
    NUMBER: /[0-9]+/
    %ignore " "
"#;

const JSON: &str = r#"
    start: value
    ?value: object | array | STRING | NUMBER | "true" | "false" | "null"
    object: "{" [pair ("," pair)*] "}"
    pair: STRING ":" value
    array: "[" [value ("," value)*] "]"
    STRING: /"[^"]*"/
    NUMBER: /-?[0-9]+/
    %ignore /[ \t\n]+/
"#;

const MAYBE: &str = r#"
    start: "[" [NAME] ("," [NAME])* "]"
    NAME: /[a-z]+/
    %ignore " "
"#;

const SHAPES: &str = r#"
    start: item+
    item: "(" _inner ")" | "!" kept
    _inner: NAME value
    ?value: NAME | NUMBER
    !kept: NAME "=" NUMBER
    NAME: /[a-z]+/
    NUMBER: /[0-9]+/
    %ignore " "
"#;

/// The relative oracle: for every (lexer, propagate) configuration and input,
/// `parse_tape(input).materialize()` is byte-identical to `parse(input)`.
fn assert_tape_projects_to_parse(grammar: &str, inputs: &[&str]) {
    for lexer in [LexerType::Basic, LexerType::Contextual] {
        for propagate in [false, true] {
            let l = lark(grammar, lexer.clone(), propagate);
            for &input in inputs {
                let via_parse = l.parse(input).expect("parse ok");
                let via_tape: ParseTree = l.parse_tape(input).expect("parse_tape ok").materialize();
                assert_eq!(
                    format!("{via_parse:?}"),
                    format!("{via_tape:?}"),
                    "tape materialize diverged from parse \
                     (lexer={lexer:?}, propagate={propagate}) on {input:?}"
                );
            }
        }
    }
}

#[test]
fn tape_projects_to_parse_arith() {
    assert_tape_projects_to_parse(ARITH, &["1", "1+2*3", "(1+2)*3-4", "10 / 2 + 3"]);
}

#[test]
fn tape_projects_to_parse_json() {
    assert_tape_projects_to_parse(
        JSON,
        &[
            r#"{"a": 1}"#,
            r#"[1, 2, 3]"#,
            r#"{"x": [true, null], "y": {"z": "s"}}"#,
            r#"[]"#,
        ],
    );
}

#[test]
fn tape_projects_to_parse_maybe_placeholders() {
    assert_tape_projects_to_parse(MAYBE, &["[]", "[a]", "[a, b]", "[, b]", "[a, , c]"]);
}

#[test]
fn tape_projects_to_parse_transparent_expand1_keepall() {
    assert_tape_projects_to_parse(SHAPES, &["(a b)", "(a 1)", "!x = 3", "(a b) !y = 4 (c 5)"]);
}

#[test]
fn tape_token_text_borrows_the_input_non_ascii() {
    // Non-ASCII input: char→byte cursor must map positions correctly.
    let grammar = r#"
        start: WORD ("," WORD)*
        WORD: /[^\s,]+/
        %ignore " "
    "#;
    let l = lark(grammar, LexerType::Contextual, false);
    let input = "héllo, wörld, 日本語";
    let via_parse = l.parse(input).expect("parse ok");
    let tape = l.parse_tape(input).expect("parse_tape ok");
    assert_eq!(
        format!("{via_parse:?}"),
        format!("{:?}", tape.materialize())
    );
    // And the zero-copy text really points into the input.
    let texts: Vec<&str> = tape
        .entries()
        .iter()
        .filter_map(|e| tape.token_text(e))
        .collect();
    assert!(texts.contains(&"héllo") && texts.contains(&"日本語"));
    for t in texts {
        let (ip, tp) = (input.as_ptr() as usize, t.as_ptr() as usize);
        assert!(
            tp >= ip && tp + t.len() <= ip + input.len(),
            "token text must borrow the input"
        );
    }
}

// ─── Whole-bank projection: tape materialize == tree parse over the LALR bank ────

mod bank {
    use super::*;
    use lark_rs::grammar::terminal::flags;
    use serde_json::Value;
    use std::collections::BTreeSet;
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use std::path::PathBuf;

    fn fixtures_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/oracles/compliance")
    }

    fn load_json(name: &str) -> Option<Value> {
        let text = std::fs::read_to_string(fixtures_dir().join(name)).ok()?;
        Some(serde_json::from_str(&text).expect("valid JSON"))
    }

    fn record_options(rec: &Value) -> LarkOptions {
        let start = match &rec["start"] {
            Value::String(s) => vec![s.clone()],
            Value::Array(a) => a
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect(),
            _ => vec!["start".to_string()],
        };
        let lexer = match rec["lexer"].as_str() {
            Some("basic") => LexerType::Basic,
            _ => LexerType::Contextual,
        };
        let mut g_regex_flags = 0u32;
        if let Some(letters) = rec["g_regex_flags"].as_str() {
            for ch in letters.chars() {
                g_regex_flags |= match ch {
                    'i' => flags::IGNORECASE,
                    'm' => flags::MULTILINE,
                    's' => flags::DOTALL,
                    'x' => flags::VERBOSE,
                    _ => 0,
                };
            }
        }
        LarkOptions {
            start,
            parser: ParserAlgorithm::Lalr,
            lexer,
            maybe_placeholders: rec["maybe_placeholders"].as_bool().unwrap_or(true),
            keep_all_tokens: rec["keep_all_tokens"].as_bool().unwrap_or(false),
            strict: rec["strict"].as_bool().unwrap_or(false),
            g_regex_flags,
            ..Default::default()
        }
    }

    /// The projection invariant over the full compliance bank: wherever the tree
    /// backend `parse()`s a case, the tape must materialize to the exact same
    /// tree; wherever `parse()` errors, `parse_tape` must error too. Relative, so
    /// no XFAIL allow-list — zero divergences required.
    #[test]
    fn tape_projects_to_parse_over_compliance_bank() {
        let Some(bank) = load_json("bank.json") else {
            eprintln!("compliance bank absent — skipping (generate with tools/)");
            return;
        };
        let records = bank.as_array().expect("bank is an array");

        let skip: BTreeSet<String> = load_json("skip.json")
            .and_then(|v| v.as_array().cloned())
            .map(|a| {
                a.into_iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        std::panic::set_hook(Box::new(|_| {}));

        let mut divergences: Vec<String> = Vec::new();
        let mut compared = 0usize;
        let mut built = 0usize;

        for (ri, rec) in records.iter().enumerate() {
            let grammar = rec["grammar"].as_str().unwrap_or("");
            if grammar.is_empty() || skip.contains(grammar) {
                continue;
            }
            if rec["construct_error"].as_bool().unwrap_or(false) {
                continue;
            }
            let cases = rec["cases"].as_array().map(Vec::as_slice).unwrap_or(&[]);
            if cases.is_empty() {
                continue;
            }
            let opts = record_options(rec);
            let Ok(Ok(lark)) = catch_unwind(AssertUnwindSafe(|| Lark::new(grammar, opts))) else {
                continue;
            };
            built += 1;

            for (ci, case) in cases.iter().enumerate() {
                let input = case["input"].as_str().unwrap_or("");
                let via_parse = catch_unwind(AssertUnwindSafe(|| lark.parse(input)));
                let via_tape = catch_unwind(AssertUnwindSafe(|| {
                    lark.parse_tape(input).map(|t| t.materialize())
                }));
                compared += 1;
                match (via_parse, via_tape) {
                    (Ok(Ok(t)), Ok(Ok(s))) => {
                        if format!("{t:?}") != format!("{s:?}") {
                            divergences.push(format!("tree:{ri}:{ci} grammar={grammar:?}"));
                        }
                    }
                    (Ok(Err(_)), Ok(Err(_))) => {}
                    (Ok(Ok(_)), Ok(Err(_))) | (Ok(Err(_)), Ok(Ok(_))) => {
                        divergences.push(format!("ok-mismatch:{ri}:{ci} grammar={grammar:?}"));
                    }
                    (Err(_), Err(_)) => {}
                    _ => divergences.push(format!("panic-mismatch:{ri}:{ci} grammar={grammar:?}")),
                }
            }
        }

        let _ = std::panic::take_hook();
        eprintln!(
            "tape projection: {built} grammars built, {compared} cases compared, \
             {} divergences",
            divergences.len()
        );
        assert!(
            divergences.is_empty(),
            "tape materialize must project to the tree parse over the whole bank; \
             divergences: {:?}",
            &divergences[..divergences.len().min(20)]
        );
        assert!(compared > 0, "expected to compare at least one bank case");
    }
}

// ─── Counter gate: zero generic output on the tape path ─────────────────────────

#[cfg(feature = "perf-counters")]
mod counters {
    use super::*;
    use lark_rs::perf;

    const LIST_GRAMMAR: &str = r#"
start: list
list: list item | item
item: ITEM
ITEM: "a"
%ignore " "
"#;

    #[test]
    fn tape_backend_builds_no_tree_and_copies_no_token_bytes() {
        assert!(
            perf::ENABLED,
            "built with perf-counters but counters report disabled"
        );
        let parser = Lark::new(
            LIST_GRAMMAR,
            LarkOptions {
                parser: ParserAlgorithm::Lalr,
                lexer: LexerType::Contextual,
                start: vec!["start".to_string()],
                ..Default::default()
            },
        )
        .expect("list grammar builds under LALR");

        for &n in &[1usize, 2, 4, 8, 16] {
            let input = vec!["a"; n].join(" ");
            perf::reset();
            let tape = parser.parse_tape(&input).expect("tape parse ok");
            assert_eq!(perf::tree_nodes_built(), 0, "tape path built a Tree node");
            assert_eq!(
                perf::token_value_string_bytes(),
                0,
                "tape path copied token value bytes into the output"
            );
            assert_eq!(
                perf::lexer_token_value_bytes(),
                0,
                "tape path allocated an owned lexer token value"
            );
            assert_eq!(
                perf::semantic_reduce_calls(),
                (2 * n + 1) as u64,
                "tape path must still shape one reduction per node"
            );
            // And the tape is real: materialize matches parse.
            let via_parse = parser.parse(&input).expect("parse ok");
            assert_eq!(
                format!("{via_parse:?}"),
                format!("{:?}", tape.materialize())
            );
        }
    }
}
