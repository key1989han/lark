//! Zero-copy `SpanTree<'i>` backend (C8, #233) — the relative-oracle + counter gate.
//!
//! `SpanTree` is beyond-oracle in *representation* (borrowed spans + interned
//! labels), so per ADR-0026 it ships under the **relative oracle**: its
//! [`materialize`](lark_rs::SpanNode::materialize) projection must reproduce, byte
//! for byte, the tree `parse()` returns. Three gates here:
//!
//!   1. **Projection over curated grammars** — `parse_span(input).materialize()`
//!      equals `parse(input)` across arithmetic / JSON / `maybe_placeholders` /
//!      transparent+`expand1` / `keep_all_tokens`, both lexers, `propagate_positions`
//!      off and on. Proves the borrowed backend drives every shaping decision
//!      identically to the tree backend.
//!   2. **Projection over the whole compliance bank** — the same relative invariant
//!      over every LALR grammar strip-mined from Python Lark's suite (the "whole
//!      bank" projection the issue asks for). No XFAIL list: this is relative, so
//!      wherever `parse()` builds a tree, the projection *must* match by
//!      construction — a divergence is a real C8 bug, not a known gap.
//!   3. **Zero-owned-output counters** (`--features perf-counters`) — a span parse
//!      builds **no** `Tree` (`tree_nodes_built == 0`) and copies **no** token value
//!      (`token_value_string_bytes == 0`), while still running exactly one
//!      `semantic_reduce_call` per reduction. The "no intermediate tree / no copy"
//!      claim is a counter result, never prose (ADR-0007 / ADR-0029).
//!
//! The whole file is gated on `--features span-tree` (experimental surface,
//! ADR-0029 fork 3); the counter gate additionally needs `perf-counters`.

#![cfg(feature = "span-tree")]

use lark_rs::{Lark, LarkOptions, LexerType, ParseTree, ParserAlgorithm, SpanNode};

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

// `maybe_placeholders` on by default → an absent `[...]` optional inserts a `None`
// child (`SpanNode::None`), which must project to `Child::None` / `ParseTree::None`.
const MAYBE: &str = r#"
    start: "[" [NAME] ("," [NAME])* "]"
    NAME: /[a-z]+/
    %ignore " "
"#;

// Transparent `_rule` splicing + `?rule` expand1 + a `!rule` keep-all sibling.
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
/// `parse_span(input).materialize()` is byte-identical to `parse(input)`.
fn assert_span_projects_to_parse(grammar: &str, inputs: &[&str]) {
    for lexer in [LexerType::Basic, LexerType::Contextual] {
        for propagate in [false, true] {
            let l = lark(grammar, lexer.clone(), propagate);
            for &input in inputs {
                let via_parse = l.parse(input).expect("parse ok");
                let via_span: ParseTree = l.parse_span(input).expect("parse_span ok").materialize();
                assert_eq!(
                    format!("{via_parse:?}"),
                    format!("{via_span:?}"),
                    "span materialize diverged from parse \
                     (lexer={lexer:?}, propagate={propagate}) on {input:?}"
                );
            }
        }
    }
}

#[test]
fn span_projects_to_parse_arith() {
    assert_span_projects_to_parse(ARITH, &["1", "1+2*3", "(1+2)*3-4", "10 / 2 + 3"]);
}

#[test]
fn span_projects_to_parse_json() {
    assert_span_projects_to_parse(
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
fn span_projects_to_parse_maybe_placeholders() {
    assert_span_projects_to_parse(MAYBE, &["[]", "[a]", "[a, b]", "[, b]", "[a, , c]"]);
}

#[test]
fn span_projects_to_parse_transparent_expand1_keepall() {
    assert_span_projects_to_parse(SHAPES, &["(a b)", "(a 1)", "!x = 3", "(a b) !y = 4 (c 5)"]);
}

// ─── The borrowed value genuinely points *into* the input (zero copy) ───────────

#[test]
fn span_token_values_borrow_the_input() {
    let l = lark(ARITH, LexerType::Contextual, false);
    let input = String::from("12 + 345");
    let root = l.parse_span(&input).expect("parse_span ok");

    // Walk to the leaf tokens and assert each value slice lives inside `input`'s
    // allocation — a borrow, not a fresh `String`. (An owned copy would sit at an
    // unrelated address.)
    let input_start = input.as_ptr() as usize;
    let input_end = input_start + input.len();
    let mut leaves = 0usize;
    let mut stack = vec![&root];
    while let Some(node) = stack.pop() {
        match node {
            SpanNode::Token(t) => {
                let p = t.value.as_ptr() as usize;
                assert!(
                    p >= input_start && p + t.value.len() <= input_end,
                    "token value {:?} must borrow the input buffer, not own a copy",
                    t.value
                );
                // NB: `start_pos`/`end_pos` are *char* indices; this direct slice is
                // only valid because this input is pure ASCII (char index == byte
                // offset). The non-ASCII test below exercises the char→byte cursor.
                assert_eq!(&input[t.start_pos..t.end_pos], t.value);
                leaves += 1;
            }
            SpanNode::Branch(b) => stack.extend(b.children.iter()),
            SpanNode::None => {}
        }
    }
    assert_eq!(leaves, 2, "12 and 345 are the two kept NUMBER leaves");
}

// ─── Non-ASCII: the char-index → byte-offset cursor is the riskiest path ─────────

/// The one grammar/input that would break a naive `&input[start_pos..end_pos]`:
/// multibyte tokens whose char indices are *not* byte offsets. Pins that the
/// borrowed spans are the correct substrings (not mis-sliced or panicking) and that
/// they still point into the original input buffer.
#[test]
fn span_token_values_borrow_the_input_non_ascii() {
    const WORDS: &str = r#"
        start: WORD+
        WORD: /[^\s]+/
        %ignore " "
    "#;
    // "å" (2 bytes), "βeta" (β is 2 bytes), "漢" (3 bytes) — every token starts at a
    // byte offset that differs from its char index, so a char-index slice would cut
    // mid-codepoint (panic) or grab the wrong bytes.
    let input = String::from("å βeta 漢");
    let l = lark(WORDS, LexerType::Contextual, false);
    let root = l.parse_span(&input).expect("parse_span ok");

    let input_start = input.as_ptr() as usize;
    let input_end = input_start + input.len();
    let mut values: Vec<&str> = Vec::new();
    let mut stack = vec![&root];
    while let Some(node) = stack.pop() {
        match node {
            SpanNode::Token(t) => {
                let p = t.value.as_ptr() as usize;
                assert!(
                    p >= input_start && p + t.value.len() <= input_end,
                    "non-ASCII token value {:?} must borrow the input buffer",
                    t.value
                );
                values.push(t.value);
            }
            SpanNode::Branch(b) => stack.extend(b.children.iter()),
            SpanNode::None => {}
        }
    }
    // `WORD+` collects the tokens left-to-right; the stack walk above reverses, so
    // sort-independent-compare by set membership on the exact expected substrings.
    values.sort_unstable();
    let mut expected = ["å", "βeta", "漢"];
    expected.sort_unstable();
    assert_eq!(
        values, expected,
        "borrowed spans must be the exact multibyte tokens"
    );

    // And the whole thing still projects byte-identically to the owned tree.
    assert_eq!(
        format!("{:?}", l.parse(&input).expect("parse ok")),
        format!("{:?}", root.materialize()),
        "non-ASCII span parse must project to the tree parse"
    );
}

// ─── Unsupported configuration refuses, exactly like parse_into ─────────────────

#[test]
fn parse_span_rejects_earley() {
    let l = Lark::new(
        ARITH,
        LarkOptions {
            parser: ParserAlgorithm::Earley,
            lexer: LexerType::Basic,
            start: vec!["start".to_string()],
            ..Default::default()
        },
    )
    .expect("grammar builds");
    let err = l.parse_span("1+2").unwrap_err();
    assert!(
        format!("{err}").contains("parser='lalr'"),
        "expected a typed LALR-only refusal, got: {err}"
    );
}

// ─── Error parity: span-mode name-less tokens must not degrade error reports ─────

/// The span/tape token sources emit value-less, **name-less** tokens (`type_` is
/// empty — the name is resolved lazily from `type_id`). A parse error must still
/// report the correct terminal name in `token_type`: `LalrParser::unexpected`
/// resolves it from the symbol table, so `parse_span` errors match `parse()`
/// errors rather than leaking `token_type: ""`. Regression pin for the span-mode
/// name-clone-skip review finding (perf spike 2026-07-02).
#[test]
fn span_error_token_type_matches_parse() {
    const G: &str = r#"
        start: A B
        A: "a"
        B: "b"
        %ignore " "
    "#;

    fn token_type(err: &lark_rs::ParseError) -> &str {
        match err {
            lark_rs::ParseError::UnexpectedToken { token_type, .. } => token_type,
            other => panic!("expected UnexpectedToken, got {other:?}"),
        }
    }

    for lexer in [LexerType::Basic, LexerType::Contextual] {
        let l = lark(G, lexer.clone(), false);
        // "a a": the second `A` appears where `B` is expected — an UnexpectedToken.
        let parse_err = l.parse("a a").expect_err("parse must reject");
        let span_err = match l.parse_span("a a").expect_err("parse_span must reject") {
            lark_rs::LarkError::Parse(e) => e,
            other => panic!("expected a Parse error, got {other:?}"),
        };
        assert_eq!(
            token_type(&parse_err),
            token_type(&span_err),
            "span-mode error token_type must match parse() (lexer={lexer:?}); \
             an empty string means the name-less span token leaked into the error"
        );
        assert_eq!(token_type(&span_err), "A", "the offending token is an A");
    }
}

// ─── Whole-bank projection: span materialize == tree parse over the LALR bank ────

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
    /// backend `parse()`s a case, the span backend must materialize to the exact
    /// same tree; wherever `parse()` errors, `parse_span` must error too. This is
    /// **relative** (span-vs-tree), so it needs no XFAIL allow-list — a divergence
    /// is a real C8 regression, and we require zero.
    #[test]
    fn span_projects_to_parse_over_compliance_bank() {
        let Some(bank) = load_json("bank.json") else {
            eprintln!("compliance bank absent — skipping (generate with tools/)");
            return;
        };
        let records = bank.as_array().expect("bank is an array");

        // Process-aborting grammars (unbounded template recursion, etc.) — same
        // skip list the tree compliance replay uses; a stack overflow can't be
        // caught with catch_unwind.
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
            // Only interested in grammars that build; construct-error cases have no
            // parse to project.
            if rec["construct_error"].as_bool().unwrap_or(false) {
                continue;
            }
            let cases = rec["cases"].as_array().map(Vec::as_slice).unwrap_or(&[]);
            if cases.is_empty() {
                continue;
            }
            let opts = record_options(rec);
            let Ok(Ok(lark)) = catch_unwind(AssertUnwindSafe(|| Lark::new(grammar, opts))) else {
                continue; // grammar lark-rs can't build yet — tree-side XFAIL, not ours
            };
            built += 1;

            for (ci, case) in cases.iter().enumerate() {
                let input = case["input"].as_str().unwrap_or("");
                // The tree backend's own result is the oracle we project against.
                let via_parse = catch_unwind(AssertUnwindSafe(|| lark.parse(input)));
                let via_span = catch_unwind(AssertUnwindSafe(|| {
                    lark.parse_span(input).map(|n| n.materialize())
                }));
                compared += 1;
                match (via_parse, via_span) {
                    (Ok(Ok(t)), Ok(Ok(s))) => {
                        if format!("{t:?}") != format!("{s:?}") {
                            divergences.push(format!("tree:{ri}:{ci} grammar={grammar:?}"));
                        }
                    }
                    // Error/OK parity: both must agree on parseability.
                    (Ok(Err(_)), Ok(Err(_))) => {}
                    (Ok(Ok(_)), Ok(Err(_))) | (Ok(Err(_)), Ok(Ok(_))) => {
                        divergences.push(format!("ok-mismatch:{ri}:{ci} grammar={grammar:?}"));
                    }
                    // A panic on one side but not the other is also a divergence;
                    // panics on *both* (or neither building) are out of scope here.
                    (Err(_), Err(_)) => {}
                    _ => divergences.push(format!("panic-mismatch:{ri}:{ci} grammar={grammar:?}")),
                }
            }
        }

        let _ = std::panic::take_hook();
        eprintln!(
            "span projection: {built} grammars built, {compared} cases compared, \
             {} divergences",
            divergences.len()
        );
        assert!(
            divergences.is_empty(),
            "span materialize must project to the tree parse over the whole bank; \
             divergences: {:?}",
            &divergences[..divergences.len().min(20)]
        );
        assert!(compared > 0, "expected to compare at least one bank case");
    }
}

// ─── Counter gate: zero owned output (needs perf-counters too) ──────────────────

#[cfg(feature = "perf-counters")]
mod counters {
    use super::*;
    use lark_rs::perf;

    /// The same list grammar the C5 counter gate uses: `n` items → `2n+1`
    /// reductions, `n` kept one-byte `ITEM` tokens (see `test_output_counters.rs`).
    const LIST_GRAMMAR: &str = r#"
start: list
list: list item | item
item: ITEM
ITEM: "a"
%ignore " "
"#;

    #[test]
    fn span_backend_builds_no_tree_and_copies_no_token_bytes() {
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
            let root = parser.parse_span(&input).expect("span parse ok");

            let nodes = perf::tree_nodes_built();
            let bytes = perf::token_value_string_bytes();
            let lexer_bytes = perf::lexer_token_value_bytes();
            let reduces = perf::semantic_reduce_calls();
            eprintln!(
                "n={n}: tree_nodes_built={nodes}, token_value_string_bytes={bytes}, \
                 lexer_token_value_bytes={lexer_bytes}, semantic_reduce_calls={reduces}"
            );

            // ── The two C8 gates the issue body defers to this backend. ──────────
            assert_eq!(
                nodes, 0,
                "the span backend must build NO Tree node (n={n}); tree_nodes_built \
                 counts only `TreeOutputBuilder`, so a span parse leaves it at 0"
            );
            assert_eq!(
                bytes, 0,
                "the span backend must copy NO token value bytes (n={n}); values \
                 borrow the input, so token_value_string_bytes stays 0"
            );
            // ── The C8.1 (#582) gate: the *lexer* allocates no owned token value on
            //    the span path. This is the upstream half `token_value_string_bytes`
            //    (output) could not see — the span-emitting lexer path emits
            //    value-less tokens, so the lexer counter is 0 too. ────────────────
            assert_eq!(
                lexer_bytes, 0,
                "the span-emitting lexer path must allocate NO owned Token.value \
                 (n={n}); lexer_token_value_bytes counts the upstream `value.to_string()` \
                 the span path skips"
            );
            // ── …while still shaping exactly one reduction per parser reduction. ──
            assert_eq!(
                reduces,
                (2 * n + 1) as u64,
                "one semantic_reduce_call per reduction: n items → 2n+1 (n={n})"
            );

            // Sanity: the parse actually produced the shape (guards against a
            // vacuous "0 because nothing ran"). `n` kept ITEM leaves, all borrowed.
            let mut leaves = 0usize;
            let mut stack = vec![&root];
            while let Some(node) = stack.pop() {
                match node {
                    SpanNode::Token(_) => leaves += 1,
                    SpanNode::Branch(b) => stack.extend(b.children.iter()),
                    SpanNode::None => {}
                }
            }
            assert_eq!(leaves, n, "span tree keeps n={n} ITEM leaves");
        }

        // ── The C8.1 discriminator: the lexer counter is a real result, not
        //    vacuously 0. The owned `parse()` path over the same grammar has
        //    lexer_token_value_bytes > 0 (it materializes each token's value in the
        //    lexer), while the span path drives it to 0 — on BOTH the eager basic
        //    (PreLexed) and lazy contextual lexers. This is the "distinguish LEXER
        //    from OUTPUT token-value allocation" clause of #582.
        //
        //    Folded into this one test on purpose: the `perf` counters are
        //    process-global atomics, so a second parallel `#[test]` mutating them
        //    would corrupt the reads (same rationale as the scaling gates).
        for lexer in [LexerType::Basic, LexerType::Contextual] {
            let parser = Lark::new(
                LIST_GRAMMAR,
                LarkOptions {
                    parser: ParserAlgorithm::Lalr,
                    lexer: lexer.clone(),
                    start: vec!["start".to_string()],
                    ..Default::default()
                },
            )
            .expect("list grammar builds");
            let input = "a a a a a a a a"; // 8 one-byte ITEM tokens

            // Owned path: the lexer materializes each kept token's value string.
            perf::reset();
            let _ = parser.parse(input).expect("parse ok");
            let owned_lexer_bytes = perf::lexer_token_value_bytes();
            assert!(
                owned_lexer_bytes > 0,
                "the owned parse() path must allocate lexer token values \
                 (lexer={lexer:?}); got {owned_lexer_bytes}"
            );

            // Span path: value-less tokens — the lexer counter is 0.
            perf::reset();
            let _ = parser.parse_span(input).expect("span parse ok");
            assert_eq!(
                perf::lexer_token_value_bytes(),
                0,
                "the span path must allocate no lexer token values (lexer={lexer:?})"
            );
        }
    }
}
