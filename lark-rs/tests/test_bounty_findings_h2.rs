//! Bug-bounty findings, round 2 (Phase 2) — failing oracle tests (XFAIL).
//!
//! Round 1 (`test_bounty_findings.rs`, PR #263) harvested the "front door too
//! permissive" layer: missing build-time validation gates plus the RC5 lexer
//! width bug. Round 2 went after the subtler classes the user called out — valid
//! grammar → wrong token/parse, config/backend validation drift, distribution and
//! binding divergence, regex-dialect taxonomy, and deterministic resource growth.
//!
//! Each test asserts the **Python Lark 1.3.1** (oracle) behavior. This file began
//! as an XFAIL catalog; as each finding is fixed its `#[ignore]` is dropped and the
//! test becomes a live regression guard, while the remaining known divergences stay
//! `#[ignore]`d (XFAIL — Rust has no native one). It is therefore a *mix* — consult
//! each test's own attribute, not this header, for its status. Run only the
//! still-open XFAILs with:
//!
//!     cargo test --test test_bounty_findings_h2 -- --ignored
//!
//! Every find here was independently re-verified through `tools/diffcheck.py`
//! (`compare()`), the `diffcheck` binary, or a direct `lark_rs` API check.
//!
//! Target SHA: the round-1 branch tip (PR #263 stacked on `master` @ a005423).
//! Catalog with repros/severity/blast-radius: `docs/BOUNTY_FINDINGS_H2.md`.
//!
//! NONE of these reduce to a round-1 root cause (RC1–RC10) or the ineligible
//! baseline issues. The IDs below are the round-2 "N" series.

use lark_rs::{Ambiguity, Lark, LarkOptions, LexerType, ParseTree, ParserAlgorithm};

fn opts(parser: ParserAlgorithm, lexer: LexerType) -> LarkOptions {
    LarkOptions {
        parser,
        lexer,
        start: vec!["start".to_string()],
        ..Default::default()
    }
}

fn assert_build_rejected(grammar: &str, o: LarkOptions, why: &str) {
    assert!(
        Lark::new(grammar, o).is_err(),
        "{why}: Python Lark rejects this at build, but lark-rs accepted it"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// N1 — `%override` / `%extend` directive modifiers are silently dropped
// (grammar/loader/parser.rs: "consume modifier; treat same as normal for now").
// The directive never reaches the compiler, so bodies are merged like a plain
// duplicate. Distinct from RC1/RC2 (those are plain duplicate-definition merges
// with no directive). The merge case is a VALID-grammar parse divergence.
// ─────────────────────────────────────────────────────────────────────────────

/// N1a (CRITICAL). `%override start: B` should *replace* `start`, so only `"b"`
/// parses. lark-rs merges to `start: A | B` and wrongly accepts `"a"`.
/// Fixed in #269: directives now reach the compiler, which replaces the body.
#[test]
fn n1a_override_replaces_not_merges() {
    let g = "start: A\n%override start: B\nA: \"a\"\nB: \"b\"\n";
    let lark = Lark::new(g, opts(ParserAlgorithm::Lalr, LexerType::Contextual))
        .expect("N1a: grammar builds");
    // After override, the grammar is `start: B`, so "a" must be rejected (Python does).
    assert!(
        lark.parse("a").is_err(),
        "N1a: %override should have replaced `start` with B; \"a\" must not parse"
    );
}

/// N1b (HIGH). `%override` of a rule that does not exist. Python:
/// `GrammarError: Cannot override a nonexisting rule`. Fixed in #269.
#[test]
fn n1b_override_nonexistent_rejected() {
    let g = "%override foo: A\nstart: A\nA: \"a\"\n";
    assert_build_rejected(g, opts(ParserAlgorithm::Lalr, LexerType::Contextual), "N1b");
}

/// N1c (HIGH). `%extend` of a rule that does not exist. Python:
/// `GrammarError: Can't extend rule foo as it wasn't defined before`. Fixed in #269.
#[test]
fn n1c_extend_nonexistent_rejected() {
    let g = "%extend foo: A\nstart: A\nA: \"a\"\n";
    assert_build_rejected(g, opts(ParserAlgorithm::Lalr, LexerType::Contextual), "N1c");
}

/// N1 differential pin (#269 audit). `%extend` / `%override` of a *parameterized
/// rule* (template) must edit the template, not compile as a flat rule — Python
/// instantiates `foo{C}` from the overridden / extended body. (Pre-fix the
/// directive misrouted to `compile_rule`, which then rejected the template
/// parameter as an "undefined rule".)
#[test]
fn n1_template_override_and_extend() {
    let o = opts(ParserAlgorithm::Lalr, LexerType::Contextual);
    // override: `foo{C}` is `"b" C`, so "ac" is rejected, "bc" parses.
    let ov = Lark::new(
        "foo{x}: \"a\" x\n%override foo{x}: \"b\" x\nstart: foo{C}\nC: \"c\"\n",
        o.clone(),
    )
    .expect("template override builds");
    assert!(
        ov.parse("ac").is_err(),
        "override replaced the template body"
    );
    assert!(ov.parse("bc").is_ok(), "override body parses");
    // extend: both `"a" C` and `"b" C` arms are kept.
    let ex = Lark::new(
        "foo{x}: \"a\" x\n%extend foo{x}: \"b\" x\nstart: foo{C}\nC: \"c\"\n",
        o,
    )
    .expect("template extend builds");
    assert!(ex.parse("ac").is_ok(), "extend keeps the original arm");
    assert!(ex.parse("bc").is_ok(), "extend adds the new arm");
}

/// N1 differential pin (#269 audit). `%extend` of an *imported* terminal adds the
/// new alternative (Python: `"z"` parses, and `"123"` still parses). Fixed in #286:
/// the imported terminal is already a compiled `TerminalDef` by the time the
/// directive is staged, so the new alternatives are staged in
/// `pending_term_extends` and prepended onto the resolved terminal's regex in
/// `resolve_terminals` (Python's `_extend` does `base.children.insert(0, exp)` on
/// the still-AST definition tree). Same-grammar terminal extend and imported
/// terminal *override* already worked; this closes the last imported-terminal gap.
#[test]
fn n1_extend_imported_terminal_keeps_both() {
    let g = "%import common.INT\nstart: INT\n%extend INT: \"z\"\n";
    let lark =
        Lark::new(g, opts(ParserAlgorithm::Lalr, LexerType::Contextual)).expect("grammar builds");
    assert!(lark.parse("123").is_ok(), "the imported INT still parses");
    assert!(
        lark.parse("z").is_ok(),
        "the extended `\"z\"` alternative should parse (Python accepts it)"
    );
}

/// #286 edge — *multiple* `%extend`s of one imported terminal each add their
/// alternative (repeated `_extend` = repeated `insert(0, exp)`). Python accepts
/// `"123"`, `"y"`, and `"z"`.
#[test]
fn extend_imported_terminal_multiple_times_keeps_all() {
    let g = "%import common.INT\nstart: INT\n%extend INT: \"y\"\n%extend INT: \"z\"\n";
    let lark =
        Lark::new(g, opts(ParserAlgorithm::Lalr, LexerType::Contextual)).expect("grammar builds");
    for s in ["123", "y", "z"] {
        assert!(lark.parse(s).is_ok(), "extended INT should parse {s:?}");
    }
}

/// #286 edge — an imported-terminal `%extend` body may itself *reference another
/// terminal* (resolution reuses the full terminal-algebra machinery, not just
/// literals). Python accepts both the original `INT` and a `WORD`.
#[test]
fn extend_imported_terminal_body_references_terminal() {
    let g = "%import common.INT\n%import common.WORD\nstart: INT\n%extend INT: WORD\n";
    let lark =
        Lark::new(g, opts(ParserAlgorithm::Lalr, LexerType::Contextual)).expect("grammar builds");
    assert!(
        lark.parse("123").is_ok(),
        "the original INT body still parses"
    );
    assert!(
        lark.parse("hello").is_ok(),
        "the extended WORD alternative should parse (Python accepts it)"
    );
}

/// #286 — the extend arm must be ranked by *match width*, not regex-source length.
/// The lexer is leftmost-first, so a wider new arm has to sort ahead of a narrower
/// imported body or it never matches. `%extend LETTER: "abc"` makes `LETTER` match
/// `"abc"` (width 3) even though the imported `[A-Z]|[a-z]` body (width 1) has a
/// shorter source; Python accepts `"abc"`. A naive `str::len` sort would put the
/// 1-char body first and reject `"abc"` (matching only `"a"`).
#[test]
fn extend_imported_terminal_wider_arm_outranks_narrower_body() {
    let g = "%import common.LETTER\nstart: LETTER\n%extend LETTER: \"abc\"\n";
    let lark =
        Lark::new(g, opts(ParserAlgorithm::Lalr, LexerType::Contextual)).expect("grammar builds");
    assert!(
        lark.parse("abc").is_ok(),
        "the wider extended arm \"abc\" must win over the 1-char LETTER body"
    );
    assert!(
        lark.parse("a").is_ok(),
        "the original LETTER body still parses"
    );
}

/// #286 — a self-referential `%extend` of an imported terminal is rejected, exactly
/// as Python (`Recursion in terminal 'INT'`). A terminal denotes a regular language,
/// so it may not reference itself; without the recursion check the imported terminal
/// short-circuits resolution and the build would over-accept (`"123x"` would parse).
#[test]
fn extend_imported_terminal_self_recursion_rejected() {
    let g = "%import common.INT\nstart: INT\n%extend INT: INT \"x\"\n";
    let err = Lark::new(g, opts(ParserAlgorithm::Lalr, LexerType::Contextual))
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
    assert!(
        err.contains("Recursion in terminal"),
        "self-referential terminal extend must be rejected like Python; got: {err:?}"
    );
}

// ───────────────────────────────────────────────────────────────────────────────
// #286 CORRECTIVE (PR #450 review findings) — the imported-terminal `%extend`
// fix had two defects the architect caught on the omnibus:
//   D1 (HIGH): a local/imported terminal that *references* an extended import was
//       resolved against the PRE-extension pattern (the extension mutated the
//       imported terminal only AFTER all dependents were memoized), so the new
//       alternative never reached the dependent — order-dependent semantics.
//   D2 (MAJOR): the new arm sorter omitted Python's `min_width` tie-break, so an
//       equal-max-width arm pair was ordered by source length and the leftmost-
//       first engine matched the wrong (narrower-min-width) arm.
// All three cases are Python Lark 1.3.1 oracle-verified.
// ───────────────────────────────────────────────────────────────────────────────

/// #286 D1 (HIGH). A *local* terminal `X: WORD` references an imported terminal
/// that is then extended (`%extend WORD: "@"`). Python's `_extend` mutates the
/// imported `WORD` definition *before* any terminal that references it is compiled,
/// so `X` (which is just `WORD`) gains the `"@"` alternative too: both `"hello"`
/// and `"@"` parse as `X`. On the pre-fix branch `resolve_terminals` snapshotted
/// `WORD`'s regex into `imported` and memoized `X` against it *before* applying the
/// pending extend, so `X` kept the old `WORD` and `"@"` was rejected.
#[test]
fn extend_imported_terminal_rebuilds_dependent_local_terminal() {
    let g = "%import common.WORD\nX: WORD\n%extend WORD: \"@\"\nstart: X\n";
    let lark =
        Lark::new(g, opts(ParserAlgorithm::Lalr, LexerType::Contextual)).expect("grammar builds");
    assert!(
        lark.parse("hello").is_ok(),
        "the original WORD body still parses through X"
    );
    assert!(
        lark.parse("@").is_ok(),
        "X: WORD must pick up the `%extend WORD: \"@\"` alternative (Python accepts it)"
    );
}

/// #286 D1 (HIGH). Extension-to-extension dependency: one imported terminal's
/// extension *body references another imported terminal that is itself extended
/// later*. `%extend INT: WORD` then `%extend WORD: "@"` — Python resolves the whole
/// graph, so `INT` ends up accepting `"@"` (via `WORD`, which gained `"@"`). On the
/// pre-fix branch the first extension resolved `WORD` through its pre-extension
/// snapshot, so `INT`'s `WORD` arm never saw `"@"`.
#[test]
fn extend_imported_terminal_resolves_extension_to_extension_dependency() {
    let g = "%import common.INT\n%import common.WORD\nstart: INT\n\
             %extend INT: WORD\n%extend WORD: \"@\"\n";
    let lark =
        Lark::new(g, opts(ParserAlgorithm::Lalr, LexerType::Contextual)).expect("grammar builds");
    assert!(
        lark.parse("123").is_ok(),
        "the original INT body still parses"
    );
    assert!(
        lark.parse("@").is_ok(),
        "INT extended with WORD, WORD extended with \"@\" → \"@\" parses as INT \
         (Python accepts it)"
    );
}

/// #286 D2 (MAJOR). Equal-max-width arms must break ties by `min_width` before
/// source length, matching Python's `TerminalTreeToPattern` key
/// `(-max_width, -min_width, -len(value))`. Imported `T: /a|bc/` (max_width 2,
/// min_width 1, source length 4) is extended with `"ab"` (max_width 2, min_width 2,
/// source length 2). Both have max_width 2; Python's `min_width` tie-break puts the
/// `"ab"` arm first, so the leftmost-first engine matches `"ab"` as ONE `T` token.
/// The pre-fix branch sorted by source length as the 2nd key, placed `a|bc` first,
/// and matched only `"a"` — leaving `"b"` unconsumed (a parse error).
#[test]
fn extend_imported_terminal_min_width_breaks_equal_max_width_tie() {
    let sources: std::collections::HashMap<String, String> =
        [("tokens.lark".to_string(), "T: /a|bc/\n".to_string())].into();
    let g = "%import .tokens (T)\n%extend T: \"ab\"\nstart: T\n";
    let lark = Lark::new(
        g,
        LarkOptions {
            parser: ParserAlgorithm::Lalr,
            lexer: LexerType::Contextual,
            start: vec!["start".to_string()],
            import_sources: Some(std::sync::Arc::new(sources)),
            ..Default::default()
        },
    )
    .expect("grammar builds");
    let tree = lark
        .parse("ab")
        .expect("\"ab\" parses (the `\"ab\"` arm has the larger min_width, sorts first)");
    let tree = tree.as_tree().expect("tree root");
    // Exactly one child: the single `T=\"ab\"` token. Two tokens would mean the
    // narrow `a|bc` arm won and matched only `\"a\"`, then `\"b\"` as a second T.
    assert_eq!(
        tree.children.len(),
        1,
        "\"ab\" must be ONE T token (min_width tie-break), got {} children: {:?}",
        tree.children.len(),
        tree.children
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// #449 — within-terminal alternation arms must be sorted by **match width**
// (`(-max_width, -min_width, -len(value))`), NOT by regex-source-string length.
// Spun off from #286: the `%extend` path already used the width key
// (`sort_terminal_arms`), but the MAIN multi-alt path in `resolve_term_regex` still
// did `alts.sort_by(|a, b| b.len().cmp(&a.len()))`. The lexer is leftmost-FIRST
// (`MatchKind::LeftmostFirst`), so an arm with a larger match width but a SHORTER
// source string (e.g. `"ab"`, src len 4, beside `/[A-Za-z]/`, src len 9) was placed
// second and never tried. All cases below are Python Lark 1.3.1 oracle-verified.
// ─────────────────────────────────────────────────────────────────────────────

/// Leaf `(type, value)` pairs of a parse tree, in order — for asserting a token
/// segmentation against the Python oracle.
fn leaf_tokens(t: &ParseTree) -> Vec<(String, String)> {
    fn walk(c: &lark_rs::Child, out: &mut Vec<(String, String)>) {
        match c {
            lark_rs::Child::Token(tok) => out.push((tok.type_.clone(), tok.value.clone())),
            lark_rs::Child::Tree(tr) => tr.children.iter().for_each(|ch| walk(ch, out)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    match t {
        ParseTree::Tree(tr) => tr.children.iter().for_each(|ch| walk(ch, &mut out)),
        ParseTree::Token(tok) => out.push((tok.type_.clone(), tok.value.clone())),
        _ => {}
    }
    out
}

/// #449 HEADLINE. `V: /[A-Za-z]/ | "ab"` on `"ab"` → ONE `V="ab"` token, matching
/// Python Lark. Python's `TerminalTreeToPattern` sorts the arms by match width, so
/// `"ab"` (width 2) precedes `/[A-Za-z]/` (width 1) and the leftmost-first engine
/// takes the 2-char match. The pre-fix source-length sort put the 9-char `[A-Za-z]`
/// source ahead of the 4-char `"ab"`, so the engine matched only `"a"` then `"b"` —
/// two `V` tokens. (Before this fix this test asserted the wrong tokenization.)
#[test]
fn within_terminal_arms_sorted_by_match_width_not_source_length() {
    let g = "start: V+\nV: /[A-Za-z]/ | \"ab\"\n";
    let lark =
        Lark::new(g, opts(ParserAlgorithm::Lalr, LexerType::Contextual)).expect("grammar builds");
    let tree = lark.parse("ab").expect("\"ab\" parses");
    assert_eq!(
        leaf_tokens(&tree),
        vec![("V".to_string(), "ab".to_string())],
        "#449: `\"ab\"` (width 2) must sort ahead of `/[A-Za-z]/` (width 1) → one V=\"ab\" token"
    );
}

/// #449. Arm declaration order is irrelevant — width ordering is recomputed — so the
/// string-first spelling `V: "ab" | /[A-Za-z]/` tokenizes identically (one V="ab").
#[test]
fn within_terminal_width_sort_is_order_independent() {
    let g = "start: V+\nV: \"ab\" | /[A-Za-z]/\n";
    let lark =
        Lark::new(g, opts(ParserAlgorithm::Lalr, LexerType::Contextual)).expect("grammar builds");
    assert_eq!(
        leaf_tokens(&lark.parse("ab").expect("\"ab\" parses")),
        vec![("V".to_string(), "ab".to_string())],
    );
}

/// #449. Three arms of widths 1 / 2 / 3 (`/[A-Za-z]/ | "ab" | "abc"`): the widest
/// (`"abc"`, src len 5 — shorter than the 9-char class) must win on `"abc"`. Pins
/// the full descending-width ordering, not just a two-arm swap.
#[test]
fn within_terminal_widest_arm_wins_across_three_widths() {
    let g = "start: V+\nV: /[A-Za-z]/ | \"ab\" | \"abc\"\n";
    let lark =
        Lark::new(g, opts(ParserAlgorithm::Lalr, LexerType::Contextual)).expect("grammar builds");
    assert_eq!(
        leaf_tokens(&lark.parse("abc").expect("\"abc\" parses")),
        vec![("V".to_string(), "abc".to_string())],
        "#449: the width-3 `\"abc\"` arm must be tried first → one V=\"abc\""
    );
}

/// #449. A wider *regex* arm with a shorter source beats a narrower string arm:
/// `V: /[0-9][0-9]/ | "5"` on `"55"` → one V="55" (Python). The two-digit class
/// (max_width 2) sorts ahead of the width-1 `"5"`.
#[test]
fn within_terminal_wider_regex_arm_beats_narrower_string() {
    let g = "start: V+\nV: /[0-9][0-9]/ | \"5\"\n";
    let lark =
        Lark::new(g, opts(ParserAlgorithm::Lalr, LexerType::Contextual)).expect("grammar builds");
    assert_eq!(
        leaf_tokens(&lark.parse("55").expect("\"55\" parses")),
        vec![("V".to_string(), "55".to_string())],
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// #449 / #286 RESIDUAL — DECIDED BY THE ORACLE (kept; not a divergence).
//
// The issue asked whether an `%extend`ed imported terminal should retain its
// PER-ARM structure so a new extend arm of intermediate width interleaves *among*
// the imported body's internal arms (the way Python flattens-then-width-sorts a
// same-grammar `expansions` tree). Differential audit vs Python Lark 1.3.1 settles
// it: Python does **NOT** flatten an imported body. An imported terminal is already
// a compiled `Pattern` (PatternRE), so `%extend`'s `expansions` holds it as ONE
// opaque arm; the width sort ranks the whole imported body against the new arm, and
// the leftmost-first engine then resolves the imported body internally on its own
// (declaration) order. lark-rs's monolithic-arm treatment reproduces this exactly —
// so it is oracle-FAITHFUL, not a residual divergence. These pins lock that parity.
// ─────────────────────────────────────────────────────────────────────────────

/// Build a `LarkOptions` carrying inline import sources.
fn opts_with_imports(files: &[(&str, &str)]) -> LarkOptions {
    let sources: std::collections::HashMap<String, String> = files
        .iter()
        .map(|(n, c)| (n.to_string(), c.to_string()))
        .collect();
    LarkOptions {
        import_sources: Some(std::sync::Arc::new(sources)),
        ..opts(ParserAlgorithm::Lalr, LexerType::Contextual)
    }
}

/// #449/#286 RESIDUAL pin. Imported `T: /a|aaa/` (internal arms width 1 & 3, shared
/// prefix `a`) `%extend`ed with `"aa"` (width 2, *strictly between* the internal
/// arms). The headline residual question. Python keeps the imported body opaque, so
/// the combined pattern is `(?:(?:a|aaa))|(?:aa)` — on `"aa"` the first arm matches
/// only `"a"` (leftmost-first), giving TWO `T="a"` tokens. Python Lark 1.3.1 yields
/// exactly `[T="a", T="a"]`; lark-rs must too (NOT interleave `"aa"` between `a` and
/// `aaa`, which would have produced a single `T="aa"`).
#[test]
fn extend_imported_body_is_opaque_arm_matching_oracle() {
    let lark = Lark::new(
        "%import .tok (T)\n%extend T: \"aa\"\nstart: T+\n",
        opts_with_imports(&[("tok.lark", "T: /a|aaa/\n")]),
    )
    .expect("grammar builds");
    assert_eq!(
        leaf_tokens(&lark.parse("aa").expect("\"aa\" parses")),
        vec![
            ("T".to_string(), "a".to_string()),
            ("T".to_string(), "a".to_string())
        ],
        "#286 residual: imported body stays one opaque arm (Python parity) → two T=\"a\", \
         not a single interleaved T=\"aa\""
    );
}

/// #449/#286 RESIDUAL pin (companion). The SAME arms written as a single same-grammar
/// terminal `V: /a|aaa/ | "aa"` tokenize `"aa"` identically (two `V="a"`) — because
/// `/a|aaa/` is itself one regex-literal arm (max_width 3) that sorts ahead of `"aa"`
/// (width 2), and leftmost-first inside it takes `"a"`. Confirms the imported-body
/// "opaque arm" behavior is not special-casing: it is the same width-sort + leftmost-
/// first rule the headline fix applies, and it matches Python in both spellings.
#[test]
fn same_grammar_regex_alt_arm_matches_oracle_like_opaque_import() {
    let lark = Lark::new(
        "start: V+\nV: /a|aaa/ | \"aa\"\n",
        opts(ParserAlgorithm::Lalr, LexerType::Contextual),
    )
    .expect("grammar builds");
    assert_eq!(
        leaf_tokens(&lark.parse("aa").expect("\"aa\" parses")),
        vec![
            ("V".to_string(), "a".to_string()),
            ("V".to_string(), "a".to_string())
        ],
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Lexer / regex dialect.
// ─────────────────────────────────────────────────────────────────────────────

/// N2 (HIGH). A case-insensitive (flagged) regex terminal is mis-ranked. Python
/// sorts on `len(pattern.value)` (the *raw* source, flags kept separate); lark-rs
/// bakes the flag into the regex string (`(?i:aa)`) and the tiebreak in
/// `lexer/plan.rs` compares that **wrapped** length. So `B: /aa/i` (raw len 2,
/// wrapped len 7) outranks the equal `A: /aa/` and the name-asc tiebreak is
/// subverted: Python emits `A`, lark-rs emits `B`. Distinct from RC5 (max_width):
/// both widths tie; the bug is the flag-wrapper length leaking into the tiebreak.
#[test] // FIXED (#268): raw-pattern-length tiebreak strips the baked flag wrapper.
fn n2_flagged_terminal_ranking() {
    let g = "start: A | B\nA: /aa/\nB: /aa/i\n";
    let lark = Lark::new(g, opts(ParserAlgorithm::Lalr, LexerType::Contextual))
        .expect("N2: grammar builds");
    let ParseTree::Tree(t) = lark.parse("aa").expect("N2: \"aa\" parses") else {
        panic!("N2: expected a tree");
    };
    match &t.children[0] {
        lark_rs::Child::Token(tok) => assert_eq!(
            tok.type_, "A",
            "N2: equal width+priority → name-asc picks A; lark-rs picked {}",
            tok.type_
        ),
        other => panic!("N2: expected a token, got {other:?}"),
    }
}

/// N3 (HIGH). A *global* inline flag group `(?i)` (also `(?m)`, `(?s)`, `(?x)`).
/// Python rejects every terminal with a global inline flag because Lark wraps the
/// pattern source, demoting the flag off position 0:
/// `error: global flags not at the start of the expression`. lark-rs strips the
/// wrapper into a flag bitset and accepts + applies it. (Scoped `(?i:…)` is fine on
/// both — only the global form diverges.) A new more-permissive validation family.
#[test] // FIXED (#274): a global (bodiless) inline flag group is rejected at build,
        // while scoped `(?i:…)` stays accepted — `PatternRe::new` parity gate.
fn n3_global_inline_flag_rejected() {
    let g = "start: A\nA: /(?i)abc/\n";
    assert_build_rejected(g, opts(ParserAlgorithm::Lalr, LexerType::Contextual), "N3");
}

/// N4 (MEDIUM). A named backreference `(?P=name)` is mis-categorized. General
/// backreferences are a documented OUT-OF-SCOPE non-goal (LOOKAROUND_SCOPE.md), so
/// the *correct* behavior is a categorized `GrammarError::LookaroundScope` refusal
/// — exactly what `\1`/`\k`/`\g` produce ("not supported (by design) … a
/// backreference …"). But the classifier's `has_backref` misses the `(?P=name)`
/// spelling, so it slips past classification and lark-rs instead leaks a raw,
/// uncategorized regex error (`Invalid regex pattern … regex parse error`). The
/// XFAIL asserts the *categorized* refusal (matching `\1`), NOT support — this is
/// not promotion to a supported feature. Distinct from RC6 (`\b`, different
/// construct).
#[test] // FIXED (#274): the front-end keeps `(?P=name)` verbatim, so it routes through
        // the categorized backref refusal (`BacktrackingOnlySyntax`) like `\1`/`\k`.
fn n4_named_backref_categorized() {
    let g = "start: A\nA: /(?P<x>a)(?P=x)/\n";
    let err = Lark::new(g, opts(ParserAlgorithm::Lalr, LexerType::Contextual))
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
    // A numeric backref `\1` yields the categorized refusal; `(?P=name)` must too.
    assert!(
        err.contains("not supported (by design)"),
        "N4: (?P=name) should give the categorized backref refusal (like \\1), \
         but lark-rs leaked: {err:?}"
    );
}

/// N10 (MEDIUM). The end-of-string anchor `\Z` is a plain Python `re` anchor that
/// Python accepts and tokenizes. lark-rs rejects it at build AND mis-categorizes
/// it as `LookaroundScope` "backtracking-only" — `\Z` is neither lookaround nor
/// backtracking. (Same mis-categorization hits oversized bounded repeats; see
/// catalog.) Distinct from RC6 (uncategorized leak) — this is a *wrong* category.
#[test]
#[ignore = "XFAIL (bounty N10): \\Z anchor rejected + mis-categorized as lookaround"]
fn n10_end_anchor_supported_like_python() {
    let g = r"start: A
A: /x\Z/
";
    let lark = Lark::new(g, opts(ParserAlgorithm::Lalr, LexerType::Contextual))
        .expect("N10: Python builds and tokenizes /x\\Z/");
    assert!(lark.parse("x").is_ok(), "N10: Python accepts \"x\"");
}

// ─────────────────────────────────────────────────────────────────────────────
// Configuration / backend validation drift.
// ─────────────────────────────────────────────────────────────────────────────

/// N5 (HIGH). Parser/lexer pairing legality is not enforced. Python raises
/// `ConfigurationError` for every illegal pair (e.g. `lalr`+`dynamic`,
/// `cyk`+`contextual`, `earley`+`contextual`); lark-rs silently substitutes a
/// working lexer and parses. The only pairing gate in the tree is the
/// postlex+dynamic refusal. Distinct from RC8 (zero-width *content* on dynamic).
// Fixed in #273: a front-door config-legality gate (`parsers::validate_config`)
// now mirrors Python's parser→allowed-lexer matrix, so this is a live regression
// test (un-ignored from XFAIL).
#[test]
fn n5_illegal_parser_lexer_pairing_rejected() {
    let g = "start: \"a\"\n";
    assert_build_rejected(
        g,
        opts(ParserAlgorithm::Lalr, LexerType::Dynamic),
        "N5 lalr+dynamic",
    );
    assert_build_rejected(
        g,
        opts(ParserAlgorithm::Cyk, LexerType::Contextual),
        "N5 cyk+contextual",
    );
}

/// N6 (HIGH). `ambiguity=` is only valid for Earley/CYK; Python raises
/// `ConfigurationError: 'lalr' doesn't support disambiguation`. lark-rs's
/// `build_lalr` never reads `options.ambiguity`, so it silently accepts and builds.
// Fixed in #273: `validate_config` rejects `ambiguity=explicit|forest` on
// `parser=lalr` (live regression test, un-ignored from XFAIL).
#[test]
fn n6_ambiguity_on_lalr_rejected() {
    let g = "start: \"a\"\n";
    let mut o = opts(ParserAlgorithm::Lalr, LexerType::Contextual);
    o.ambiguity = Ambiguity::Explicit;
    assert_build_rejected(g, o, "N6");
}

// ─────────────────────────────────────────────────────────────────────────────
// Core correctness surfaced through the bindings.
// ─────────────────────────────────────────────────────────────────────────────

/// N8 (MEDIUM). `start_pos`/`end_pos` were byte offsets in lark-rs but char indices
/// in Python Lark. On `"héllo"` (the `é` is 2 UTF-8 bytes) lark-rs reported
/// `end_pos=6`, Python `5`. `column`/`end_column` are char-based in both and match;
/// only `*_pos` diverged. Core-rooted (`LexCursor` advanced by byte length), copied
/// verbatim into the PyO3/WASM/C bindings under a Python-compatible API.
// Fixed in #278: the lexer cursors (`LexCursor`/`LexerState`/the interactive and
// Earley-dynamic cursors) now track a character index alongside the byte offset and
// emit it as `start_pos`/`end_pos`; the byte offset stays the scanner cursor for
// slicing. Live regression test, un-ignored from XFAIL.
#[test]
fn n8_positions_are_char_indices() {
    let g = "start: A\nA: /h.llo/\n";
    let mut o = opts(ParserAlgorithm::Lalr, LexerType::Contextual);
    o.propagate_positions = true;
    let lark = Lark::new(g, o).expect("N8: grammar builds");
    let ParseTree::Tree(t) = lark.parse("héllo").expect("N8: parses") else {
        panic!("N8: expected a tree");
    };
    let lark_rs::Child::Token(tok) = &t.children[0] else {
        panic!("N8: expected a token");
    };
    assert_eq!(
        tok.end_pos(),
        5,
        "N8: Python reports char index end_pos=5 for \"héllo\"; lark-rs gave {} (bytes)",
        tok.end_pos()
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Deterministic resource growth.
// ─────────────────────────────────────────────────────────────────────────────

/// N9 (MEDIUM). `x~mn..mx` above Python's `REPEAT_BREAK_THRESHOLD` (50) lowers to
/// O(n²) grammar size in lark-rs — `compile_repeat`'s range branch emits one rule
/// per count `k` with `k` copies, Σk = n(n+1)/2 — where Python's `_generate_repeats`
/// factors it into O(log n) shared sub-rules. Measured deterministically (no
/// wall-clock) via the total RHS-symbol count of the lowered grammar: for a 4×
/// bound it grows ≈16× (quadratic). A factored lowering would be near-flat.
/// Both engines build correct parsers — the divergence is purely build/size cost.
// FIXED (#279): `compile_repeat` now factors a large `~mn..mx` into shared
// transparent sub-rules (Python's `small_factors`/`_add_repeat_rule`), so the
// grammar size is sub-quadratic. The dedicated gate + tree-parity pins live in
// `tests/test_repeat_factoring.rs`; this asserts the headline bound stays closed.
#[test]
fn n9_bounded_repeat_grammar_size_subquadratic() {
    let total_rhs = |n: usize| -> usize {
        let g = lark_rs::load_grammar(
            &format!("start: \"x\"~0..{n}\n"),
            &["start".to_string()],
            false,
            false,
        )
        .expect("N9: grammar loads");
        lark_rs::lower(&g)
            .rules
            .iter()
            .map(|r| r.expansion.len())
            .sum()
    };
    let (r100, r400) = (total_rhs(100), total_rhs(400));
    // A 4× larger bound should NOT quadruple-squared the grammar. Python's factored
    // lowering keeps this ratio ~flat; lark-rs's per-count expansion makes it ≈16×.
    assert!(
        r400 < r100 * 6,
        "N9: total RHS symbols grew {:.1}× for a 4× bound (≈quadratic): {r100}→{r400}",
        r400 as f64 / r100 as f64
    );
}
