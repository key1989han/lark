//! `propagate_positions` / `Tree.meta` span oracle (bug-bounty H6-5, #402).
//!
//! With `propagate_positions`, Python derives a tree's `meta` from its rule's
//! *unfiltered* children (`PropagatePositions` wraps the node builder outside the
//! child filter), so filtered punctuation (`"(" A ")"`) contributes to the span.
//! lark-rs used to compute `meta` from the *already-filtered* children, reporting a
//! span that omitted the parens (`2..6` instead of Python's `0..8`). The default
//! diffcheck strips `Tree.meta`, so this surface had never been exercised — this
//! suite is the regression net the issue called for.
//!
//! The fixture (`tools/generate_oracles.py::generate_propagate_positions`) records,
//! for each grammar+input, Python Lark's tree **with `meta`** for every legal
//! `(parser, lexer)` pairing (propagate_positions is engine-agnostic). The replay
//! holds lark-rs to Python's recorded span per pairing — not to a hardcoded number
//! — via [`common::tree_matches_oracle_with_meta`].

mod common;

use lark_rs::{Ambiguity, Child, Lark, LarkOptions, LexerType, ParseTree, ParserAlgorithm, Tree};

fn engine(spec: &str) -> (ParserAlgorithm, LexerType) {
    match spec {
        "lalr/contextual" => (ParserAlgorithm::Lalr, LexerType::Contextual),
        "lalr/basic" => (ParserAlgorithm::Lalr, LexerType::Basic),
        "earley/basic" => (ParserAlgorithm::Earley, LexerType::Basic),
        "earley/dynamic" => (ParserAlgorithm::Earley, LexerType::Dynamic),
        "cyk/basic" => (ParserAlgorithm::Cyk, LexerType::Basic),
        other => panic!("unknown engine pairing in oracle: {other}"),
    }
}

#[test]
fn propagate_positions_meta_matches_oracle() {
    let cases = common::load_oracle("propagate_positions", "cases");
    let cases = cases.as_array().expect("oracle is an array of cases");

    let mut checked = 0usize;
    let mut failures = Vec::new();
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let grammar = case["grammar"].as_str().unwrap();
        let input = case["input"].as_str().unwrap();
        let engines = case["engines"].as_object().unwrap();

        for (spec, expected) in engines {
            // Honestly skip a pairing Python itself refused to build (none today,
            // but the fixture records refusals rather than dropping them).
            if expected.get("error").is_some() {
                continue;
            }
            let (parser, lexer) = engine(spec);
            let mut opts = LarkOptions {
                parser,
                lexer,
                start: vec!["start".to_string()],
                ..Default::default()
            };
            opts.propagate_positions = true;

            let lark = match Lark::new(grammar, opts) {
                Ok(l) => l,
                Err(e) => {
                    failures.push(format!("[{name}/{spec}] build failed: {e:?}"));
                    continue;
                }
            };
            let tree = match lark.parse(input) {
                Ok(t) => t,
                Err(e) => {
                    failures.push(format!("[{name}/{spec}] parse failed: {e:?}"));
                    continue;
                }
            };
            if let Err(msg) = common::tree_matches_oracle_with_meta(&tree, expected) {
                failures.push(format!("[{name}/{spec}] {msg}"));
            }
            checked += 1;
        }
    }

    assert!(
        failures.is_empty(),
        "propagate_positions meta mismatches:\n{}",
        failures.join("\n")
    );
    // Guard against the fixture silently emptying (a regenerate that drops cases).
    assert!(
        checked >= 60,
        "expected many engine×case checks, ran {checked}"
    );
}

/// The minimal repro from the issue, pinned directly (independent of the fixture):
/// `start: "(" A ")"` on `( cafX )` must span the filtered parens (`0..8`), not the
/// kept `A` (`2..6`). Mirrors the un-#[ignore]d `h6_5` bounty test, kept here so the
/// contract is legible in this suite too.
#[test]
fn issue_402_minimal_repro_spans_filtered_parens() {
    let mut opts = LarkOptions {
        parser: ParserAlgorithm::Lalr,
        lexer: LexerType::Contextual,
        start: vec!["start".to_string()],
        ..Default::default()
    };
    opts.propagate_positions = true;
    let g = "start: \"(\" A \")\"\nA: /caf./\n%import common.WS\n%ignore WS\n";
    let lark = Lark::new(g, opts).expect("builds");
    let tree = lark.parse("( cafX )").expect("parses");
    let t = tree.as_tree().expect("a tree root");
    assert_eq!((t.meta.start_pos(), t.meta.end_pos()), (Some(0), Some(8)));
}

/// `propagate_positions=false` (the default) must leave the prior behavior intact:
/// lark-rs still populates `meta` from the *post-filter* children, so the span is
/// the kept `A` (`2..6`), not the filtered parens. This pins that the #402 fix is
/// gated strictly on the flag and never widens the span when it is off.
#[test]
fn propagate_positions_off_keeps_post_filter_span() {
    let opts = LarkOptions {
        parser: ParserAlgorithm::Lalr,
        lexer: LexerType::Contextual,
        start: vec!["start".to_string()],
        propagate_positions: false,
        ..Default::default()
    };
    let g = "start: \"(\" A \")\"\nA: /caf./\n%import common.WS\n%ignore WS\n";
    let lark = Lark::new(g, opts).expect("builds");
    let tree = lark.parse("( cafX )").expect("parses");
    let t = tree.as_tree().expect("a tree root");
    assert_eq!((t.meta.start_pos(), t.meta.end_pos()), (Some(2), Some(6)));
}

// ─── Cross-engine empty/nullable-production span agreement (#500) ─────────────
//
// #500: an empty/nullable production's *synthesized* span under
// `propagate_positions=true` must agree across LALR, CYK and **Earley's two
// distinct forest-walk paths** — the resolve-mode *streaming mirror*
// (`shape_with_container` driven by the `observe_*` container, `tree_walk.rs`) and
// the explicit-mode *assemble* path (`TreeOutputBuilder::assemble`,
// `tree_builder.rs`) — and match Python Lark's oracle. The standing
// `propagate_positions_meta_matches_oracle` test above replays the empty/nullable
// cases on every engine in resolve mode; these two tests add the missing axes:
// (1) the Earley **explicit/assemble** path (the one the cross-engine differential
// in #500 found could diverge from the streaming mirror), and (2) an explicit
// engine-vs-engine span-equality assertion that does not route through the oracle,
// so a future regression that drifts *all* engines together (matching the oracle is
// then necessary but not sufficient — ADR-0021) is still caught.
//
// #543 tightened both tests: the compared shape is now the **full** `Meta` tuple
// (`line`/`column`/`end_line`/`end_column` *and* `start_pos`/`end_pos`), not just the
// byte offsets, so an assemble-only regression that keeps the offsets but corrupts the
// line/column metadata on an empty/nullable node fails here too — the same contract
// `tree_matches_oracle_with_meta` holds the resolve-mode engines to. Two multi-line
// cases were added so `end_line`/`end_column` carry a non-trivial (line-crossing)
// value, making a line/column drift observable rather than masked by an always-`1`.

/// The fixture case-names the #500 differential targets — every empty/nullable
/// production whose span must stay *positionless* while its parent widens over the
/// surrounding tokens/punctuation. These are *names*, not grammars: the grammar +
/// input are pulled from the committed oracle (`load_empty_nullable_cases`), so
/// there is a single source of truth (the generator's `PROPAGATE_POSITIONS_CASES`)
/// and no hand-duplicated grammar string can drift from it.
const EMPTY_NULLABLE_CASE_NAMES: &[&str] = &[
    "empty_rule_between_filtered_punct",
    "empty_rule_between_tokens",
    "empty_rule_leading",
    "empty_rule_trailing",
    "two_empties_between_tokens",
    "nested_empty_widened_by_outer_punct",
    "empty_via_transparent_chain",
    "empty_expand1_keeps_positionless",
    "nullable_optional_absent",
    "nullable_alternation_picks_empty",
    "empty_root_alone",
    "nullable_rep_zero_between_punct",
    "nullable_star_empty_between_punct",
    // Multi-line cases (#543): the parent widens across a newline (`end_line = 2`),
    // so a `line`/`column`/`end_line`/`end_column` regression on the explicit/
    // assemble path that keeps the byte offsets correct is observable here.
    "empty_rule_between_tokens_multiline",
    "nullable_star_empty_multiline",
];

/// One node's full propagate-positions span: `(data, line, column, end_line,
/// end_column, start_pos, end_pos, empty)` — the *complete* `Meta` tuple. #543
/// widened this from the original byte-offset-only shape so the explicit/assemble
/// Earley path is held to the same contract as `tree_matches_oracle_with_meta`
/// (which pins `line`/`column`/`end_*` against Python), not just `start_pos`/
/// `end_pos`. An assemble-only regression that corrupts line/column while keeping
/// the offsets correct now fails the cross-engine net instead of slipping through.
type SpanShape = (
    String,
    Option<usize>, // line
    Option<usize>, // column
    Option<usize>, // end_line
    Option<usize>, // end_column
    Option<usize>, // start_pos
    Option<usize>, // end_pos
    bool,          // empty
);

/// Load `(name, grammar, input)` for each `EMPTY_NULLABLE_CASE_NAMES` entry from the
/// committed `propagate_positions` oracle — the same fixture the
/// `propagate_positions_meta_matches_oracle` replay holds every engine to. Pulling
/// the grammar/input from the oracle (rather than re-literaling them) means the
/// differential tests below can never exercise a grammar that has drifted from the
/// Python-pinned case; a renamed/removed case fails loudly here.
fn load_empty_nullable_cases() -> Vec<(String, String, String)> {
    let cases = common::load_oracle("propagate_positions", "cases");
    let cases = cases.as_array().expect("oracle is an array of cases");
    EMPTY_NULLABLE_CASE_NAMES
        .iter()
        .map(|&want| {
            let case = cases
                .iter()
                .find(|c| c["name"].as_str() == Some(want))
                .unwrap_or_else(|| panic!("oracle missing empty/nullable case '{want}'"));
            (
                want.to_string(),
                case["grammar"].as_str().unwrap().to_string(),
                case["input"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

/// Flatten a tree into [`SpanShape`] tuples in pre-order so two engines' span
/// assignments can be compared directly (ignoring token leaves, which carry the
/// lexer's spans and are not the synthesized-node question #500 is about). `_ambig`
/// wrappers are skipped — their alternatives are unordered. The tuple is the **full**
/// `Meta` (line/column/end_line/end_column included, #543), so a line/column drift on
/// any compared path is caught, not just a byte-offset drift.
fn span_shape(tree: &Tree, out: &mut Vec<SpanShape>) {
    if tree.data != "_ambig" {
        out.push((
            tree.data.clone(),
            tree.meta.line(),
            tree.meta.column(),
            tree.meta.end_line(),
            tree.meta.end_column(),
            tree.meta.start_pos(),
            tree.meta.end_pos(),
            tree.meta.empty,
        ));
    }
    for c in &tree.children {
        if let Child::Tree(sub) = c {
            span_shape(sub, out);
        }
    }
}

fn parse_spans(
    grammar: &str,
    input: &str,
    parser: ParserAlgorithm,
    lexer: LexerType,
    ambiguity: Ambiguity,
) -> Option<Vec<SpanShape>> {
    let opts = LarkOptions {
        parser,
        lexer,
        ambiguity,
        start: vec!["start".to_string()],
        propagate_positions: true,
        ..Default::default()
    };
    // An engine that refuses to build (CYK on a directly-ε rule) is skipped, not
    // failed — Python refuses the same pairing (recorded as an error in the oracle).
    let lark = Lark::new(grammar, opts).ok()?;
    match lark.parse(input).ok()? {
        ParseTree::Tree(t) => {
            let mut v = Vec::new();
            span_shape(&t, &mut v);
            Some(v)
        }
        // A bare-token / bare-None root carries no synthesized node span to compare.
        ParseTree::Token(_) | ParseTree::None => Some(Vec::new()),
    }
}

/// Earley's **explicit/assemble** path must produce the same empty-production spans
/// as its **resolve/streaming** path. These cases are all unambiguous, so explicit
/// mode yields a single tree (no `_ambig` wrapper) — making the two walks directly
/// comparable. This is the exact streaming-mirror-vs-assemble axis #500 names.
#[test]
fn empty_production_span_earley_streaming_matches_assemble() {
    let mut failures = Vec::new();
    for (name, grammar, input) in load_empty_nullable_cases() {
        let resolve = parse_spans(
            &grammar,
            &input,
            ParserAlgorithm::Earley,
            LexerType::Basic,
            Ambiguity::Resolve,
        );
        let explicit = parse_spans(
            &grammar,
            &input,
            ParserAlgorithm::Earley,
            LexerType::Basic,
            Ambiguity::Explicit,
        );
        if resolve != explicit {
            failures.push(format!(
                "[{name}] earley resolve(streaming) != explicit(assemble):\n  streaming: {resolve:?}\n  assemble:  {explicit:?}"
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "Earley streaming-mirror vs assemble span divergence (#500):\n{}",
        failures.join("\n")
    );
}

/// Every engine that *accepts* an empty/nullable production must synthesize the
/// same node spans as every other accepting engine — proven by direct engine-vs-
/// engine equality, not only via the oracle (ADR-0021: a banks/oracle-green that
/// drifts all engines together is necessary but not sufficient). LALR contextual is
/// the reference and is deliberately *not* in `pairings` (a self-comparison would be
/// trivially green); each other accepting pairing must match it tuple-for-tuple.
///
/// Note CYK's reach here is the *parent-widening* axis only: it rejects a directly-ε
/// user rule (`e:`, ADR-0024), so the 12 cases carrying an actual positionless empty
/// *node* skip CYK; the three nullable-via-repetition cases it accepts elide the empty
/// node entirely (ε-removal), leaving just the widened `start`. LALR + Earley (both
/// walk paths) carry the empty-node axis in full. With #543 the compared tuple is the
/// full `Meta` (line/column/end_*), and two of these cases are multi-line, so the
/// parent-widening axis now exercises a real `end_line = 2` on every engine.
#[test]
fn empty_production_span_agrees_across_engines() {
    let pairings = [
        (
            "lalr/basic",
            ParserAlgorithm::Lalr,
            LexerType::Basic,
            Ambiguity::Resolve,
        ),
        (
            "earley/basic",
            ParserAlgorithm::Earley,
            LexerType::Basic,
            Ambiguity::Resolve,
        ),
        (
            "earley/dynamic",
            ParserAlgorithm::Earley,
            LexerType::Dynamic,
            Ambiguity::Resolve,
        ),
        (
            "earley/explicit",
            ParserAlgorithm::Earley,
            LexerType::Basic,
            Ambiguity::Explicit,
        ),
        (
            "cyk/basic",
            ParserAlgorithm::Cyk,
            LexerType::Basic,
            Ambiguity::Resolve,
        ),
    ];
    let cases = load_empty_nullable_cases();
    let mut failures = Vec::new();
    let mut compared = 0usize;
    for (name, grammar, input) in &cases {
        let reference = parse_spans(
            grammar,
            input,
            ParserAlgorithm::Lalr,
            LexerType::Contextual,
            Ambiguity::Resolve,
        )
        .unwrap_or_else(|| panic!("[{name}] LALR contextual reference must parse"));
        for (spec, parser, lexer, amb) in &pairings {
            // Skip a pairing that refuses to build/parse (CYK on a directly-ε rule).
            let Some(spans) =
                parse_spans(grammar, input, parser.clone(), lexer.clone(), amb.clone())
            else {
                continue;
            };
            compared += 1;
            if spans != reference {
                failures.push(format!(
                    "[{name}/{spec}] span shape != LALR-contextual reference:\n  ref:  {reference:?}\n  this: {spans:?}"
                ));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "cross-engine empty-production span divergence (#500):\n{}",
        failures.join("\n")
    );
    // Guard against the case table silently emptying: 15 cases × 4 always-accepting
    // pairings (lalr/basic + the three Earley walks) + 3 CYK-accepting (the two
    // nullable-via-repetition cases + the multi-line `*` case) = 63.
    assert!(
        compared >= 58,
        "expected many engine×case comparisons, ran {compared}"
    );
}
