//! Bug-bounty findings, round 6 (h6) — failing oracle tests (XFAIL).
//!
//! Rounds 1–5 (`test_bounty_findings.rs` RC, `_h2.rs` N, `_h3.rs` H, `_h4.rs` H4-*,
//! `_h5.rs` H5-*) harvested the validation-gate layer, the lexer terminal-ordering
//! bugs, four+ waves of Python-`re` regex-dialect divergences, config legality,
//! char-vs-byte positions, error/`ParseError` parity, import-closure mangling,
//! tree-shaping lone-`None`, the standalone bake, and the bindings surface. Round 6
//! retargeted the corners those rounds either declared clean or never reached:
//!
//!   * H6-1 — terminal **value-length tiebreak** measures the *normalized* pattern
//!     (`\<\<\<` stored as `<<<`), not Python's *raw source* length, flipping which of
//!     two equal-priority/equal-width terminals wins a span (distinct from N2/#268's
//!     flag-wrapper strip and RC5/#268's `max_width`).
//!   * H6-2 — the `{,m}` quantifier (Python's `{0,m}`) is **rejected and
//!     mis-categorized** as an OutOfScope lookaround/backtracking refusal, where Python
//!     `re`/Lark accept it (opposite polarity to the H6–H9/#375 dialect *narrowings*).
//!   * H6-3 — two **nullable alternatives differing only by an alias** (`p: "a"? -> al1
//!     | "b"? -> al2`) produce a spurious LALR reduce/reduce rejection; Python's LALR
//!     and lark-rs's own Earley accept it (opposite direction to RC7/#272's
//!     *under*-reporting audit).
//!   * H6-4 — a **bare nested optional under repetition** (`[[A]]* C`) mints a recurse
//!     helper with twin byte-identical empty arms → a spurious LALR reduce/reduce; the
//!     non-nested forms `[A]* C` / `([A])* C` build fine, and Earley accepts `[[A]]*`.
//!   * H6-5 — `Tree.meta` (with `propagate_positions`) spans only the **post-filter**
//!     children, so a rule wrapped by filtered punctuation (`"(" A ")"`) reports a span
//!     that omits the parens; Python derives meta from the *pre-filter* children
//!     (distinct from N8's byte-vs-char `*_pos` and H10/#337's positionless-empty flag).
//!   * H6-6 — a string literal whose source is byte-identical to a named **regex**
//!     terminal (`"ab"` beside `AB: /ab/`) is wrongly **unified** onto that terminal, so
//!     the literal is typed `AB` and kept instead of being a distinct, filtered
//!     `__ANON_*` (distinct from H4-9/#347's Str-vs-Str *alternation-arm* dedup; this is
//!     the Re-vs-Str *interning* merge in `patterns_equivalent`).
//!   * H6-7 — `(X|X) (X|X) … (X|X)` (k duplicate-arm inline groups) makes
//!     `compile_expansion` materialize a `2^k` cartesian product before its single
//!     end-of-function dedup; Python's `SimplifyRule_Visitor` dedups each group's arms
//!     first and builds in linear time to one rule (distinct from N9's `~n..m` size and
//!     #252's `~n` repeat path, where Python *also* blows up).
//!   * H6-8 — rule/terminal names with **no alphabetic char** (`_`, `__`, `_9`) are
//!     accepted; Python's grammar lexer requires `[a-z]`/`[A-Z]` (distinct from
//!     H5-2/#361's `__foo`, which *has* a letter).
//!
//! Each test asserts the **Python Lark 1.3.1** (oracle) behavior. This file is an XFAIL
//! catalog: each test starts `#[ignore]`d and failing. Drop a test's `#[ignore]` when its
//! bug is fixed to turn it into a permanent regression guard — H6-2 (the `{,n}`
//! empty-lower-bound quantifier `{0,n}` normalization, #400), H6-5 (the
//! `propagate_positions` filtered-token meta span, #402), H6-7 (the duplicate-arm
//! inline-group cross-product build blowup, #404) and H6-8 (the letterless rule/terminal
//! name shape validation, #405) are fixed and now run by default.
//! H6-3 + H6-4 (the spurious LALR reduce/reduce on nullable arms) are fixed and run by
//! default (#401), with a differential-audit block pinning the adversarial
//! nullable/alias/nested-optional cases. (The H6-3 Earley `al1`-vs-`al2` resolution
//! divergence — a distinct forest-construction root, the SPPF `(left,right)` family
//! dedup collapsing two ε rules — is tracked as #432, NOT fixed here.)
//! Run the still-open XFAILs with:
//!
//!     cargo test --test test_bounty_findings_h6 -- --ignored
//!
//! Baseline SHA: b4ab6cd578b1bd334f7fddc79781202fc66bba4a. Catalog with repros, severity,
//! blast radius, fix contracts, the provisional bindings findings (C-API
//! `maybe_placeholders` default; error-hierarchy collapse) and the un-minimized Earley
//! token-filter lead, plus the dedup against rounds 1–5: `docs/BOUNTY_FINDINGS_H6.md`.
//!
//! NONE of these reduce to a round-1..5 root cause (RC1–RC10, N1–N10, V1–V4, H1–H12,
//! P1–P2, H4-1…H4-12, H5-1…H5-9) or the open known-issue set. Adjacencies are noted at
//! each test.

use lark_rs::{Child, Lark, LarkOptions, LexerType, ParseTree, ParserAlgorithm};

fn opts(parser: ParserAlgorithm, lexer: LexerType) -> LarkOptions {
    LarkOptions {
        parser,
        lexer,
        start: vec!["start".to_string()],
        ..Default::default()
    }
}

/// First token (pre-order) in the tree — for asserting which terminal won a span.
fn first_token_type(t: &ParseTree) -> Option<String> {
    fn walk(c: &Child) -> Option<String> {
        match c {
            Child::Token(tok) => Some(tok.type_.clone()),
            Child::Tree(tr) => tr.children.iter().find_map(walk),
            Child::None => None,
        }
    }
    match t {
        ParseTree::Token(tok) => Some(tok.type_.clone()),
        ParseTree::Tree(tr) => tr.children.iter().find_map(walk),
        ParseTree::None => None,
    }
}

/// Count tokens (pre-order) in the tree — for asserting filtering behaviour.
fn token_count(t: &ParseTree) -> usize {
    fn walk(c: &Child) -> usize {
        match c {
            Child::Token(_) => 1,
            Child::Tree(tr) => tr.children.iter().map(walk).sum(),
            Child::None => 0,
        }
    }
    match t {
        ParseTree::Token(_) => 1,
        ParseTree::Tree(tr) => tr.children.iter().map(walk).sum(),
        ParseTree::None => 0,
    }
}

/// The `data` name of the first child *tree* of `start` — for `start: p` over an
/// aliased `p`, this is the surviving alias (`al1`/`al2`), the tree-naming metadata
/// Python keeps outside the LALR reduce/reduce comparison. Returns `start`'s own
/// `data` if it has no tree child (e.g. an aliased rule directly on `start`).
fn child_tree_name(t: &ParseTree) -> Option<String> {
    match t {
        ParseTree::Tree(tr) => {
            for c in &tr.children {
                if let Child::Tree(inner) = c {
                    return Some(inner.data.clone());
                }
            }
            Some(tr.data.clone())
        }
        _ => None,
    }
}

/// A flat `data:[child …]` rendering of a tree (token type for tokens, `None` for
/// placeholders) — enough to compare a small parse tree against the Python oracle's
/// `pretty()` shape without a full structural matcher.
fn flat(t: &ParseTree) -> String {
    fn child(c: &Child) -> String {
        match c {
            Child::Token(tok) => tok.type_.clone(),
            Child::None => "None".to_string(),
            Child::Tree(tr) => tree(tr),
        }
    }
    fn tree(tr: &lark_rs::Tree) -> String {
        let kids: Vec<String> = tr.children.iter().map(child).collect();
        format!("{}[{}]", tr.data, kids.join(","))
    }
    match t {
        ParseTree::Tree(tr) => tree(tr),
        ParseTree::Token(tok) => tok.type_.clone(),
        ParseTree::None => "None".to_string(),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Lexer: terminal ranking / dialect.
// ─────────────────────────────────────────────────────────────────────────────

/// H6-1 (MEDIUM, lexer ranking). Python's terminal sort key is
/// `(-priority, -max_width, -len(pattern.value), name)`, where `pattern.value` is the
/// **verbatim source**. lark-rs's `Pattern::raw_value_len` (`grammar/terminal.rs`)
/// measures the *normalized* stored pattern: `PatternRe::new` runs
/// `normalize_python_escapes`, which rewrites `\<\<\<` → `<<<` (len 6 → 3) before
/// storage and discards the raw source. So for two equal-priority, equal-`max_width`
/// terminals `A: /\<\<\</` (Python value-len 6) and `B: /<<<|q/` (value-len 5), Python
/// ranks `A` first and emits token `A` on `"<<<"`, while lark-rs sees `A`=3 < `B`=5,
/// ranks `B` first, and emits `B`. Distinct from N2/#268 (the *flag-wrapper* leak,
/// fixed by `strip_whole_pattern_flag_wrapper`) and RC5/#268 (`max_width`, the 2nd key);
/// this is the 3rd key (`raw_value_len`) and a different lost-length source (body-escape
/// normalization). Fix (#399): `PatternRe` retains the pre-normalization `raw` source and
/// `raw_value_len` measures that, so `raw_value_len() == len(pattern.value)`.
#[test] // FIXED (#399): the value-length tiebreak measures the verbatim source, not the
        // normalized pattern.
fn h6_1_value_length_tiebreak_uses_raw_source() {
    let g = "start: A | B\nA: /\\<\\<\\</\nB: /<<<|q/\n";
    let lark = Lark::new(g, opts(ParserAlgorithm::Lalr, LexerType::Basic)).expect("H6-1: builds");
    let tree = lark.parse("<<<").expect("H6-1: parses");
    assert_eq!(
        first_token_type(&tree).as_deref(),
        Some("A"),
        "H6-1: Python's value-length tiebreak (source len 6 > 5) selects terminal A; \
         lark-rs measured the normalized pattern (len 3) and selected B"
    );
}

/// H6-1, second trigger (the `(?#…)` comment-strip length-loss source). The issue calls
/// for the comment-strip case to reproduce/pass identically to the `\<\<\<` body-escape
/// case: `normalize_python_escapes` drops a `(?#…)` comment span before storage, so
/// `ZZ: /ab(?#cccc)/` (verbatim source len 10) normalizes to `ab` (len 2) with the same
/// `max_width` 2 as `B: /ab/` (len 2). Python ranks by `(-priority, -max_width,
/// -len(pattern.value), name)`: equal priority and width, ZZ's *longer raw value* wins
/// the 3rd key *before* the name sort, so Python emits `ZZ` on `"ab"`. A `raw_value_len`
/// that measured the normalized pattern would tie both at 2 and fall through to the name
/// sort (`B` < `ZZ` → wrong `B`). Names chosen so the name tiebreak disagrees with the
/// value-length tiebreak, isolating the 3rd key.
#[test] // FIXED (#399): comment-strip body normalization no longer changes a terminal's rank.
fn h6_1_value_length_tiebreak_uses_raw_source_comment_strip() {
    let g = "start: ZZ | B\nZZ: /ab(?#cccc)/\nB: /ab/\n";
    let lark = Lark::new(g, opts(ParserAlgorithm::Lalr, LexerType::Basic)).expect("H6-1c: builds");
    let tree = lark.parse("ab").expect("H6-1c: parses");
    assert_eq!(
        first_token_type(&tree).as_deref(),
        Some("ZZ"),
        "H6-1c: ZZ's verbatim source (len 10, comment stripped to `ab`) outranks B (len 2) on \
         the value-length tiebreak before the name sort; a normalized-length measure would \
         tie both at 2 and wrongly pick B by name"
    );
}

/// H6-2 (MEDIUM, regex dialect). The `{,m}` quantifier is Python `re`'s shorthand for
/// `{0,m}` (`re.match(r'a{,3}b','aaab')` matches), and Python Lark builds
/// `A: /a{,3}b/`. The Rust `regex` crate requires a decimal lower bound, so `Regex::new`
/// fails; `PatternRe::new` then hands the pattern to the lookaround analyzer, which
/// can't parse it, and routes it to `GrammarError::LookaroundScope` / `OutOfScope`
/// "backtracking-only syntax" — two faults: it rejects a Python-accepted regex, and the
/// refusal category is wrong (it is a plain dialect-normalization gap, not lookaround).
/// Note `base_quantifier_len` already *recognizes* `{,n}` as a well-formed quantifier;
/// only the `{,n}` → `{0,n}` normalization is missing. Opposite polarity to the
/// H6–H9/#375 dialect *narrowings* (which reject to match Python's rejection). Expected
/// fix: normalize `{,n}` → `{0,n}` in `normalize_python_escapes` (class/escape-aware).
#[test] // FIXED (#400): `normalize_python_escapes` rewrites the empty-lower-bound `{,n}`
        // → `{0,n}` (class-aware, escape-aware, only on a `base_quantifier_len`-valid
        // `{,n}`), so `/a{,3}b/` builds and matches exactly as Python's `{0,3}` does. The
        // inverted-bound `a{3,2}` stays rejected by both engines (the negative control).
fn h6_2_empty_lower_bound_quantifier_accepted() {
    let g = "start: A\nA: /a{,3}b/\n";
    let lark = Lark::new(g, opts(ParserAlgorithm::Lalr, LexerType::Contextual));
    let lark =
        lark.expect("H6-2: Python accepts /a{,3}b/ (== /a{0,3}b/); lark-rs rejected the build");
    let tree = lark.parse("aaab").expect("H6-2: parses 'aaab'");
    assert_eq!(
        first_token_type(&tree).as_deref(),
        Some("A"),
        "H6-2: /a{{,3}}b/ must match 'aaab' as token A, matching Python"
    );
    // `{,3}` is `{0,3}`, not unbounded: four `a`s overrun the bound, so the token can no
    // longer cover the whole input — the parse must fail exactly as Python's does
    // (`re.match(r'a{,3}b','aaaab')` is None). This pins the *semantics*, not just that
    // the build stopped rejecting.
    assert!(
        lark.parse("aaaab").is_err(),
        "H6-2: /a{{,3}}b/ == /a{{0,3}}b/ caps at 3 leading 'a's; 'aaaab' (4) must NOT parse"
    );

    // Negative control: the inverted-bound `a{3,2}` (min > max) is a Python `re` build
    // error ("min repeat greater than max repeat" → Lark LexError). It has a *lower*
    // bound, so the `{,n}` rewrite never touches it; the regex crate rejects it too, and
    // it routes to a build error on every engine path. Both engines must keep rejecting.
    let inverted = "start: A\nA: /a{3,2}b/\n";
    for (parser, lexer) in [
        (ParserAlgorithm::Lalr, LexerType::Contextual),
        (ParserAlgorithm::Lalr, LexerType::Basic),
        (ParserAlgorithm::Earley, LexerType::Basic),
        (ParserAlgorithm::Earley, LexerType::Dynamic),
    ] {
        assert!(
            Lark::new(inverted, opts(parser.clone(), lexer.clone())).is_err(),
            "H6-2 negative control ({parser:?}/{lexer:?}): the inverted-bound /a{{3,2}}b/ \
             must stay a build error (Python rejects it; the `{{,n}}` rewrite does not touch a \
             lower-bounded quantifier)"
        );
    }
}

/// H6-6 (MEDIUM-HIGH, terminal unification). A string literal `"ab"` whose source is
/// byte-identical to a named **regex** terminal `AB: /ab/` is wrongly unified onto `AB`
/// during anon-terminal interning: `patterns_equivalent` (`grammar/loader/terminals.rs`)
/// compares `a.as_regex_str() == b.as_regex_str() && flags match`, which collapses
/// `PatternStr("ab")` and `PatternRe(/ab/)` because both project to the source `ab`.
/// Python's `Pattern.__eq__` requires `type(self) == type(other)`, and `term_reverse`
/// is consulted only for `PatternStr`, so a literal never unifies with a regex
/// terminal — Python mints a distinct anonymous `__ANON_*` terminal (filtered from the
/// tree). lark-rs keeps the literal typed `AB` and *unfiltered*, an extra child under
/// default options. Distinct from H4-9/#347 (Str-vs-Str *alternation-arm* dedup via
/// `sym_key`); this is the Re-vs-Str *interning* merge. Expected fix: gate
/// `patterns_equivalent` on matching `Pattern` kind (never `Str` ≡ `Re`).
#[test] // FIXED (#403): `patterns_equivalent` gates on matching `Pattern` kind, so a
        // string literal never unifies onto a same-source regex terminal — it stays a
        // distinct, filtered `__ANON_*` exactly as Python's `Pattern.__eq__` requires.
fn h6_6_string_literal_not_unified_with_regex_terminal() {
    // The issue's named configs: lalr/contextual, lalr/basic, earley/basic
    // (earley/dynamic already agreed). All must match Python 1.3.1 (verified live).
    let configs = [
        (ParserAlgorithm::Lalr, LexerType::Contextual),
        (ParserAlgorithm::Lalr, LexerType::Basic),
        (ParserAlgorithm::Earley, LexerType::Basic),
    ];

    let g = "start: AB | \"ab\"\nAB: /ab/\n";
    for (parser, lexer) in configs.clone() {
        let lark = Lark::new(g, opts(parser.clone(), lexer.clone()))
            .unwrap_or_else(|e| panic!("H6-6 ({parser:?}/{lexer:?}): builds: {e:?}"));
        let tree = lark.parse("ab").expect("H6-6: parses");
        // Python: the literal "ab" is a distinct PatternStr (__ANON_0), filtered → no tokens.
        // lark-rs (pre-fix): the literal is unified onto AB and kept → one token.
        assert_eq!(
            token_count(&tree),
            0,
            "H6-6 ({parser:?}/{lexer:?}): Python keeps the literal as a distinct filtered __ANON \
             terminal (0 tokens in the tree); lark-rs unified it onto the regex terminal AB and kept it"
        );
    }

    // keep_all_tokens=True proves the collapse: Python types the surviving token `__ANON_0`
    // (the distinct literal terminal), where the pre-fix lark-rs typed it `AB` (the regex
    // terminal it had been merged onto).
    for (parser, lexer) in configs.clone() {
        let mut o = opts(parser.clone(), lexer.clone());
        o.keep_all_tokens = true;
        let lark = Lark::new(g, o)
            .unwrap_or_else(|e| panic!("H6-6 keep_all ({parser:?}/{lexer:?}): builds: {e:?}"));
        let tree = lark.parse("ab").expect("H6-6 keep_all: parses");
        assert_eq!(
            first_token_type(&tree).as_deref(),
            Some("__ANON_0"),
            "H6-6 keep_all ({parser:?}/{lexer:?}): Python types the literal token __ANON_0 (a \
             distinct anonymous terminal); lark-rs collapsed it onto the regex terminal AB"
        );
    }

    // Control: a regex `/[ab]/` beside a literal `"a"` must NOT unify (regex source `[ab]`
    // ≠ literal `a`) — and must NOT diverge from Python. Python filters the literal under
    // default options (childless start) and types the surviving token `A` (the literal's
    // name hint) under keep_all_tokens. The fix must not perturb this control.
    let control = "start: R | \"a\"\nR: /[ab]/\n";
    for (parser, lexer) in configs {
        let lark = Lark::new(control, opts(parser.clone(), lexer.clone()))
            .unwrap_or_else(|e| panic!("H6-6 control ({parser:?}/{lexer:?}): builds: {e:?}"));
        assert_eq!(
            token_count(&lark.parse("a").expect("H6-6 control: parses")),
            0,
            "H6-6 control ({parser:?}/{lexer:?}): literal \"a\" beside /[ab]/ is filtered (0 tokens), \
             matching Python — the regex source [ab] never matches the literal a, so no unification"
        );
        let mut o = opts(parser.clone(), lexer.clone());
        o.keep_all_tokens = true;
        let lark = Lark::new(control, o).unwrap_or_else(|e| {
            panic!("H6-6 control keep_all ({parser:?}/{lexer:?}): builds: {e:?}")
        });
        assert_eq!(
            first_token_type(&lark.parse("a").expect("H6-6 control keep_all: parses")).as_deref(),
            Some("A"),
            "H6-6 control keep_all ({parser:?}/{lexer:?}): the literal \"a\" keeps its own name A, \
             matching Python — it never merged with /[ab]/"
        );
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// LALR table construction: spurious reduce/reduce on nullable arms.
// ─────────────────────────────────────────────────────────────────────────────

/// H6-3 (MEDIUM-HIGH, lalr-table). Two nullable alternatives of the same origin that
/// differ only by an **alias** (`p: "a"? -> al1 | "b"? -> al2`) survive lowering as two
/// distinct `Rule`s with distinct `tree_name` (`grammar/loader/ebnf.rs`,
/// `grammar/intern.rs`); the reduce/reduce detector (`parsers/lalr.rs`) then treats the
/// two `p -> ε` reductions on `$END` as an unresolvable collision. Without aliases the
/// arms dedup and the grammar builds. Python's LALR resolves same-rule ties by
/// priority/order (first arm wins) and treats the alias as pure tree-naming metadata
/// outside the R/R comparison; lark-rs's own Earley also accepts (proving the grammar is
/// legal). Opposite direction to RC7/#272 (recurse-helper over-share, which *under*-
/// reports). Expected fix: in the R/R resolution, reduce (not error) candidates that
/// share `origin`+`expansion` and differ only by alias, picking the lowest `rule.order`.
#[test] // FIXED (#401): aliased nullable alternatives resolve by lowest rule.order.
fn h6_3_aliased_nullable_alternatives_build() {
    let g = "p: \"a\"? -> al1 | \"b\"? -> al2\nstart: p\n";
    // Both LALR lexers: builds, and the resolved tree-name matches Python exactly —
    // `''→al1` (first-arm-wins, NOT al2), `'a'→al1`, `'b'→al2`.
    for lexer in [LexerType::Basic, LexerType::Contextual] {
        let lark = Lark::new(g, opts(ParserAlgorithm::Lalr, lexer.clone())).unwrap_or_else(|e| {
            panic!(
                "H6-3 ({lexer:?}): Python's LALR accepts aliased nullable alternatives; \
                 lark-rs reported a spurious reduce/reduce collision: {e:?}"
            )
        });
        for (inp, want) in [("", "al1"), ("a", "al1"), ("b", "al2")] {
            let t = lark.parse(inp).expect("H6-3: parses");
            assert_eq!(
                child_tree_name(&t).as_deref(),
                Some(want),
                "H6-3 ({lexer:?}): on {inp:?} Python resolves the same-origin nullable tie to \
                 {want} (lowest rule.order, first-arm-wins); lark-rs picked the wrong arm"
            );
        }
    }
}

/// H6-4 (MEDIUM, ebnf-loader). A bare nested optional under repetition (`[[A]]* C`)
/// reaches `inner_alternatives` (`grammar/loader/ebnf.rs`) as an `Expr::Maybe` whose sole
/// content is another `Maybe`; it is wrapped in a single `__anon_group_*` helper whose
/// rule then carries **two byte-identical empty productions** (inner-absent and
/// outer-absent), never collapsed the way a lone `([A])?` is. Those twin ε-arms surface
/// as a self reduce/reduce collision (`__anon_group_0 -> / __anon_group_0 ->`). The
/// non-nested forms `[A]* C`, `([A])* C`, and `[[A] B]* C` all build, and Earley accepts
/// `[[A]]*` — isolating the bare-double-bracket-under-repetition path. Python's
/// `EBNF_to_BNF`/`SimplifyRule_Visitor` collapses the twin empties and accepts.
/// Expected fix: collapse the helper's duplicate empty arms (or distribute the nested
/// maybe's arms) so a single ε base arm is emitted.
#[test] // FIXED (#401): the recurse helper's twin empty arms are collapsed.
fn h6_4_nested_optional_under_repetition_builds() {
    let g = "start: [[A]]* C\nA: \"a\"\nC: \"c\"\n";
    for lexer in [LexerType::Basic, LexerType::Contextual] {
        let lark = Lark::new(g, opts(ParserAlgorithm::Lalr, lexer.clone())).unwrap_or_else(|e| {
            panic!(
                "H6-4 ({lexer:?}): Python accepts [[A]]* C; lark-rs minted a recurse helper with \
                 twin empty arms and reported a spurious LALR reduce/reduce: {e:?}"
            )
        });
        // Python parses 'c' to start[C], 'aac' to start[A,A,C].
        assert_eq!(
            first_token_type(&lark.parse("c").expect("H6-4: parses 'c'")).as_deref(),
            Some("C"),
            "H6-4 ({lexer:?}): [[A]]* C on 'c' must yield token C, matching Python"
        );
        assert_eq!(
            flat(&lark.parse("aac").expect("H6-4: parses 'aac'")),
            "start[A,A,C]",
            "H6-4 ({lexer:?}): [[A]]* C on 'aac' must be start[A,A,C], matching Python"
        );
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Differential audit (#401): nullable-arm / R/R-resolution cases the banks
// under-sample. Each expectation is the Python Lark 1.3.1 tree (verified live);
// these pin the LALR R/R-resolution fix against the adversarial inputs the bounty
// catalog and issue #401 name (distinct-alias arms, the H6-4 controls, nested
// optionals under `*`/`+`), so a future regression is caught structurally rather
// than relying on banks-green alone.
// ─────────────────────────────────────────────────────────────────────────────

/// Three nullable alternatives differing only by alias resolve to the first arm
/// (lowest `rule.order`) per matching present token — Python's first-arm-wins. A
/// three-way variant of H6-3, exercising the (origin, expansion)-group collapse over
/// more than two candidates.
#[test]
fn h6_3_three_aliased_nullable_alternatives_resolve_first_arm() {
    let g = "p: \"a\"? -> al1 | \"b\"? -> al2 | \"c\"? -> al3\nstart: p\n";
    for lexer in [LexerType::Basic, LexerType::Contextual] {
        let lark = Lark::new(g, opts(ParserAlgorithm::Lalr, lexer.clone()))
            .unwrap_or_else(|e| panic!("H6-3/3 ({lexer:?}): builds: {e:?}"));
        for (inp, want) in [("", "al1"), ("a", "al1"), ("b", "al2"), ("c", "al3")] {
            assert_eq!(
                child_tree_name(&lark.parse(inp).expect("parses")).as_deref(),
                Some(want),
                "H6-3/3 ({lexer:?}): {inp:?} → {want} (first-arm-wins)"
            );
        }
    }
}

/// Aliased nullable alternatives directly on `start` (no wrapping rule) resolve the
/// same way — confirming the R/R collapse is keyed on (origin, expansion), not on the
/// presence of an enclosing rule.
#[test]
fn h6_3_aliased_nullable_on_start_resolves_first_arm() {
    let g = "start: \"a\"? -> s1 | \"b\"? -> s2\n";
    for lexer in [LexerType::Basic, LexerType::Contextual] {
        let lark = Lark::new(g, opts(ParserAlgorithm::Lalr, lexer.clone()))
            .unwrap_or_else(|e| panic!("H6-3/start ({lexer:?}): builds: {e:?}"));
        for (inp, want) in [("", "s1"), ("a", "s1"), ("b", "s2")] {
            // The aliased arm *is* `start`, so the tree's own `data` is the alias.
            let t = lark.parse(inp).expect("parses");
            let ParseTree::Tree(tr) = &t else {
                panic!("expected tree")
            };
            assert_eq!(
                tr.data, want,
                "H6-3/start ({lexer:?}): {inp:?} → {want} (first-arm-wins)"
            );
        }
    }
}

/// The H6-4 controls (`[A]* C`, `([A])* C`, `[[A] B]* C`) all build and parse
/// tree-identical to Python on both LALR lexers — they built before the fix too, and
/// must keep doing so (the fix must not perturb the single-empty-arm cases).
#[test]
fn h6_4_controls_build_and_match() {
    let cases: &[(&str, &[(&str, &str)])] = &[
        (
            "start: [A]* C\nA: \"a\"\nC: \"c\"\n",
            &[("c", "start[C]"), ("ac", "start[A,C]")],
        ),
        (
            "start: ([A])* C\nA: \"a\"\nC: \"c\"\n",
            &[("c", "start[C]"), ("ac", "start[A,C]")],
        ),
        (
            "start: [[A] B]* C\nA: \"a\"\nB: \"b\"\nC: \"c\"\n",
            &[("c", "start[C]"), ("abc", "start[A,B,C]")],
        ),
    ];
    for (g, ios) in cases {
        for lexer in [LexerType::Basic, LexerType::Contextual] {
            let lark = Lark::new(g, opts(ParserAlgorithm::Lalr, lexer.clone()))
                .unwrap_or_else(|e| panic!("H6-4 control ({lexer:?}) {g:?}: builds: {e:?}"));
            for (inp, want) in *ios {
                assert_eq!(
                    &flat(&lark.parse(inp).expect("parses")),
                    want,
                    "H6-4 control ({lexer:?}) {g:?}: {inp:?} → {want}"
                );
            }
        }
    }
}

/// The `+` sibling of H6-4: `[[A]]+ C` under `maybe_placeholders` builds and emits a
/// `None` placeholder for the absent inner `[A]` of the mandatory first copy on `'c'`
/// (Python: `start[None, C]`), and `start[A, C]` on `'ac'`. Pins that collapsing the
/// twin empty arms does not disturb the placeholder count Python emits.
#[test]
fn h6_4_plus_sibling_with_placeholders() {
    let g = "start: [[A]]+ C\nA: \"a\"\nC: \"c\"\n";
    for lexer in [LexerType::Basic, LexerType::Contextual] {
        let mut o = opts(ParserAlgorithm::Lalr, lexer.clone());
        o.maybe_placeholders = true;
        let lark = Lark::new(g, o).unwrap_or_else(|e| panic!("H6-4+ ({lexer:?}): builds: {e:?}"));
        assert_eq!(
            flat(&lark.parse("c").expect("parses 'c'")),
            "start[None,C]",
            "H6-4+ ({lexer:?}): [[A]]+ C on 'c' → start[None, C] (Python's placeholder)"
        );
        assert_eq!(
            flat(&lark.parse("ac").expect("parses 'ac'")),
            "start[A,C]",
            "H6-4+ ({lexer:?}): [[A]]+ C on 'ac' → start[A, C]"
        );
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tree metadata: propagate_positions span.
// ─────────────────────────────────────────────────────────────────────────────

/// H6-5 (MEDIUM, core / tree-meta). With `propagate_positions`, Python wraps the node
/// builder as `child_filter → PropagatePositions`, deriving a tree's `meta` from the
/// **unfiltered** children (`_pp_get_meta`), so filtered punctuation contributes to the
/// span. lark-rs computes meta in `Meta::from_children` (`tree.rs`) over the
/// **already-filtered** children that `apply_rule_options` produced, so a rule wrapped by
/// filtered literals (`start: "(" A ")"`) reports a span that omits the parens
/// (start_pos/end_pos `2..6` instead of Python's `0..8`). The token positions are
/// correct; only the tree-meta span diverges (and the diffcheck harness strips meta, so
/// this surface was never exercised). Distinct from N8 (byte-vs-char `*_pos`, fixed) and
/// H10/#337 (positionless-empty `meta.empty` flag). Expected fix: compute meta from the
/// production's pre-filter child span (a filtered token contributes its own start/end).
#[test]
fn h6_5_meta_span_includes_filtered_tokens() {
    let mut o = opts(ParserAlgorithm::Lalr, LexerType::Contextual);
    o.propagate_positions = true;
    let g = "start: \"(\" A \")\"\nA: /caf./\n%import common.WS\n%ignore WS\n";
    let lark = Lark::new(g, o).expect("H6-5: builds");
    let ParseTree::Tree(t) = lark.parse("( cafX )").expect("H6-5: parses") else {
        panic!("H6-5: expected a tree");
    };
    // Python: start meta spans the whole "( cafX )" including the filtered parens: 0..8.
    assert_eq!(
        (t.meta.start_pos(), t.meta.end_pos()),
        (Some(0), Some(8)),
        "H6-5: start meta must span the filtered '(' and ')' (0..8), matching Python; \
         lark-rs computed it from post-filter children (2..6)"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Grammar name-token lexer: name shape validation.
// ─────────────────────────────────────────────────────────────────────────────

/// H6-8 (LOW, grammar-loader) — **FIXED** (#405). Python's grammar lexer regexes are
/// `RULE = _?[a-z][_a-z0-9]*` and `TERMINAL = _?[A-Z][_A-Z0-9]*` — at most one leading
/// underscore and **at least one** alphabetic char. lark-rs's `lex_rule`/`lex_terminal`
/// (`grammar/loader/tokenizer.rs`) consumed any run of name characters with no name-shape
/// validation, so a name with no letter (`_`, `__`, `_9`) was accepted where Python
/// rejects it at grammar-lex time (oracle-confirmed, lark 1.3.1: each rejects at build
/// with `GrammarError: Unexpected input`). Distinct from H5-2/#361 (`__foo` — a name that
/// *has* a letter but a disallowed `__` prefix); this is the no-letter-at-all class, a
/// different validation predicate. Per ADR-0017 (being more permissive than the oracle
/// is unfalsifiable), the fix rejects-like-Python: `reject_letterless_name` in the
/// tokenizer, alongside #361's `reject_double_underscore_name`.
///
/// The two checks **compose**: `reject_double_underscore_name` requires a letter present,
/// so it never fires on `_`/`__`/`_9`; `reject_letterless_name` closes exactly that gap.
/// This test pins all three letterless rule-name forms reject, the terminal-name analog
/// rejects, the accepted boundary (`_x`/`_X` single-underscore-then-letter, non-leading
/// `x__`/`a__b`) still builds + parses, and #361's `__`-leading rejection still holds.
#[test]
fn h6_8_letterless_names_rejected() {
    // All three folded letterless rule-name forms (oracle: REJECT at build).
    for g in [
        "_: \"a\"\nstart: _\n",
        "__: \"a\"\nstart: __\n",
        "_9: \"a\"\nstart: _9\n",
    ] {
        for lexer in [LexerType::Basic, LexerType::Contextual] {
            assert!(
                Lark::new(g, opts(ParserAlgorithm::Lalr, lexer.clone())).is_err(),
                "H6-8 ({lexer:?}): Python rejects a name with no [a-z]/[A-Z] at grammar-lex; \
                 lark-rs accepted it. grammar={g:?}"
            );
        }
    }

    // Terminal-name analog. A purely letterless name can carry no uppercase letter, so
    // Python's grammar-of-grammars lexes `_9` as a (rejected) RULE rather than a TERMINAL;
    // referencing a letterless name where a terminal is expected (`A: _9` / `%declare _9`)
    // still rejects at grammar-lex. lark-rs's dispatch likewise routes a letterless name to
    // `lex_rule`, but `reject_letterless_name` guards `lex_terminal` too (belt-and-suspenders),
    // so these reject regardless of which name-token lexer the dispatch picks (oracle: REJECT).
    for g in ["start: A\nA: _9\n", "start: _9\n%declare _9\n"] {
        assert!(
            Lark::new(g, opts(ParserAlgorithm::Lalr, LexerType::Contextual)).is_err(),
            "H6-8 (terminal analog): Python rejects a letterless name in a terminal position \
             at grammar-lex; lark-rs accepted it. grammar={g:?}"
        );
    }

    // Boundary — still accepted by both (oracle: BUILD + parse `a`): a single leading
    // underscore followed by a letter (`_x`/`_X`), and non-leading underscores
    // (`x__`/`a__b`). The fix must not regress these.
    for (label, g) in [
        ("single-underscore rule", "start: _x\n_x: \"a\"\n"),
        ("single-underscore terminal", "start: _X\n_X: \"a\"\n"),
        ("trailing underscores", "start: x__\nx__: \"a\"\n"),
        ("mid underscores", "start: a__b\na__b: \"a\"\n"),
    ] {
        let lark =
            Lark::new(g, opts(ParserAlgorithm::Lalr, LexerType::Contextual)).unwrap_or_else(|e| {
                panic!("H6-8 boundary ({label}): must still build. {g:?} err={e:?}")
            });
        assert!(
            lark.parse("a").is_ok(),
            "H6-8 boundary ({label}): must still parse `a`. grammar={g:?}"
        );
    }

    // #361 composition: a `__`-leading name that *has* a letter still rejects (the
    // `reject_double_underscore_name` predicate, unaffected by the new check). Oracle: REJECT.
    for g in [
        "start: __foo\n__foo: \"a\"\n",
        "start: __FOO\n__FOO: \"a\"\n",
    ] {
        assert!(
            Lark::new(g, opts(ParserAlgorithm::Lalr, LexerType::Contextual)).is_err(),
            "H6-8 (composes with #361): a `__`-leading name with a letter must still reject. grammar={g:?}"
        );
    }
}

/// #445 (LOW, grammar-loader) — **FIXED**. The **third** name-shape class, falling between
/// #361 (`__`-leading WITH a letter) and #405 (letterless): a single leading underscore
/// followed by a non-letter that still contains a letter further along (`_9x`, `_9A`).
/// Python's name tokens are `RULE = _?[a-z][_a-z0-9]*` / `TERMINAL = _?[A-Z][_A-Z0-9]*` —
/// the char immediately after the (at most one) optional leading underscore must be a
/// letter, so `_9x` (one underscore, then a digit, then a letter) fails the shape and
/// Python rejects the grammar at grammar-lex (`GrammarError: Unexpected input`,
/// oracle-confirmed lark 1.3.1). Neither sibling predicate fires on it: #361 needs `>= 2`
/// leading underscores; #405 needs no letter at all (`_9x` has `x`). lark-rs accepted it
/// (more-permissive → unfalsifiable, ADR-0017). Fix: `reject_nonletter_after_underscore_name`
/// in the tokenizer, composing with the other two.
///
/// This is the **differential audit** for the three composing classes: every adversarial
/// name is pinned to Python Lark's accept/reject (oracle, lark 1.3.1):
///   `_9x` REJECT, `_9A` REJECT, `_9a` REJECT, `_0_a` REJECT (this fix);
///   `_` REJECT, `__` REJECT, `_9` REJECT (#405, letterless);
///   `__foo` REJECT (#361, `__`-leading);
///   `_a` ACCEPT, `_A` ACCEPT, `_x9` ACCEPT, `a9` ACCEPT, `_a9` ACCEPT (boundary).
#[test]
fn h_445_nonletter_after_single_underscore_name_rejected() {
    // The new third class (oracle: REJECT at build) — single leading underscore, then a
    // non-letter, still containing a letter. Rule-name and terminal-name analogs.
    for g in [
        "start: _9x\n_9x: \"a\"\n", // single-underscore + digit + letter (rule)
        "start: A\nA: _9X\n_9X: \"a\"\n", // terminal-name analog reached via a rule body
        "start: _9a\n_9a: \"a\"\n",
        "start: _0_a\n_0_a: \"a\"\n", // underscore, digit, underscore, then a letter
    ] {
        for lexer in [LexerType::Basic, LexerType::Contextual] {
            assert!(
                Lark::new(g, opts(ParserAlgorithm::Lalr, lexer.clone())).is_err(),
                "#445 ({lexer:?}): Python rejects `_<non-letter>…<letter>` at grammar-lex; \
                 lark-rs accepted it. grammar={g:?}"
            );
        }
    }

    // Boundary — still accepted by both (oracle: BUILD + parse `a`). The fix must leave a
    // single leading underscore + letter, a letter-first name, and an interior digit alone.
    for (label, g) in [
        ("single-underscore + letter", "start: _a\n_a: \"a\"\n"),
        ("single-underscore + upper", "start: _A\n_A: \"a\"\n"),
        ("interior digit after letter", "start: _x9\n_x9: \"a\"\n"),
        ("letter-first with digit", "start: a9\na9: \"a\"\n"),
        ("underscore + letter + digit", "start: _a9\n_a9: \"a\"\n"),
    ] {
        for lexer in [LexerType::Basic, LexerType::Contextual] {
            let lark =
                Lark::new(g, opts(ParserAlgorithm::Lalr, lexer.clone())).unwrap_or_else(|e| {
                    panic!("#445 boundary ({label}, {lexer:?}): must still build. {g:?} err={e:?}")
                });
            assert!(
                lark.parse("a").is_ok(),
                "#445 boundary ({label}, {lexer:?}): must still parse `a`. grammar={g:?}"
            );
        }
    }

    // Composition with #405 (letterless) and #361 (`__`-leading): both still reject, and
    // the new predicate must not have widened to swallow or mis-message those classes.
    for g in [
        "_: \"a\"\nstart: _\n",         // letterless (#405)
        "_9: \"a\"\nstart: _9\n",       // letterless (#405)
        "start: __foo\n__foo: \"a\"\n", // `__`-leading with a letter (#361)
    ] {
        assert!(
            Lark::new(g, opts(ParserAlgorithm::Lalr, LexerType::Contextual)).is_err(),
            "#445 (composes with #361/#405): sibling class must still reject. grammar={g:?}"
        );
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Deterministic resource bounds (grammar build).
// ─────────────────────────────────────────────────────────────────────────────

/// H6-7 (MEDIUM, perf / grammar build) — **FIXED** (#404). `compile_expansion`'s
/// per-position loop (`grammar/loader/ebnf.rs`) used to fold each group into `acc`
/// with the **non-deduping** `concat_alts`, deduping only once at the end. A chain
/// of `k` inline groups with `m` duplicate arms (`(X|X) (X|X) … (X|X)`) materialized
/// `m^k` intermediate alternatives before collapsing to a single rule — a
/// deterministic `2^k`-vs-`O(1)` blowup (measured before the fix: k=12 → 12 ms,
/// k=14 → 65 ms, k=16 → 325 ms, k=18 → 1569 ms; ~2× per +1 k, final surface rules = 1).
/// Python's `SimplifyRule_Visitor` dedups each group's arms *before* the product and
/// builds the identical grammar in flat linear time. Distinct from N9 (`~n..m` O(n²)
/// *size*) and #252 (the `~n` repeat path, which already used `concat_alts_dedup` and
/// where Python *also* blows up).
///
/// The fix folds with `concat_alts_dedup` at each position, so the running product is
/// bounded by the *distinct* alternatives at each prefix length (one, here) — producing
/// the byte-identical final alternative set with no `2^k` materialization. The
/// **deterministic** scaling net is `tests/test_grammar_build_scaling.rs` (the
/// `expansion_alts` perf counter stays flat in `k`); this wall-clock worker-thread pin
/// is the coarse behavioral backstop — `(X|X)^20` must build well within a generous
/// budget instead of hanging for seconds.
#[test]
fn h6_7_duplicate_group_cross_product_build_blowup() {
    use std::sync::mpsc;
    use std::time::Duration;

    let k = 20usize;
    let body = vec!["(X|X)"; k].join(" ");
    let grammar = format!("start: {body}\nX: \"x\"\n");

    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let built = Lark::new(&grammar, opts(ParserAlgorithm::Lalr, LexerType::Contextual)).is_ok();
        let _ = tx.send(built);
    });

    match rx.recv_timeout(Duration::from_secs(3)) {
        Ok(true) => { /* fixed: the deduped fold builds the trivial grammar instantly */ }
        Ok(false) => panic!("H6-7: (X|X)^{k} unexpectedly rejected"),
        Err(_) => panic!(
            "H6-7: (X|X)^{k} build did not finish in 3 s — the non-deduping `concat_alts` \
             materialized a 2^{k} cartesian product; Python builds the identical grammar \
             (1 rule) in linear time"
        ),
    }
}
