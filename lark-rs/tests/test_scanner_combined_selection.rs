//! L0 (combined-scanner slice) — the differential oracle for **cross-terminal
//! selection** in the combined `DfaScanner` (`docs/LEXER_DFA_PLAN.md`).
//!
//! The master differential (`tests/test_scanner_differential.rs`) compares the
//! `regex`-crate [`Scanner`](lark_rs) against the `regex-automata` `DfaScanner` over a
//! generated grammar population and the compliance/JSON/Python corpora. Its generated
//! lookaround grammars, however, never put a **guarded** terminal (a lowered
//! boundary assertion) in the *same* grammar as an **unguarded plain terminal whose
//! internal alternation is order-sensitive** (`/ab|abc/`, where leftmost-first must
//! pick the shorter earlier branch, not the longer one). That blind spot lets a
//! combined-engine change pass the whole net while still breaking an unrelated plain
//! terminal: an implementation that switches the shared engine to `MatchKind::All`
//! when *any* guard is present, and then selects the *longest* accept per terminal,
//! silently turns `/ab|abc/` from leftmost-first (`"ab"`) into longest-match
//! (`"abc"`). The lookaround-free bank can't catch it (no guard ⇒ the combined engine
//! never flips), and the per-terminal lowering oracle can't catch it (it measures one
//! terminal at offset 0, never the cross-terminal interaction).
//!
//! This test closes that gap. It is the same byte-identical-token-stream contract as
//! the master differential, narrowed to grammars that **mix a guard with an
//! order-sensitive plain alternation**. It is green today (lowering is stubbed, so
//! both backends route the guarded terminal to `fancy-regex` and agree); it becomes a
//! live regression net the moment a boundary shape lowers — exactly the case the
//! combined-engine work must not regress.

use lark_rs::{basic_lexer_conf, load_grammar, lower, BasicLexer, Lexer, LexerBackend, ParseError};

/// The lex outcome reduced to what the differential compares: the full token stream
/// (type, value, span) on success, or the failing byte position on a lexer error —
/// mirrors `test_scanner_differential.rs::lex_outcome`.
type LexOutcome = Result<Vec<(String, String, usize, usize)>, usize>;

fn lex_outcome(lexer: &BasicLexer, input: &str) -> LexOutcome {
    match lexer.lex(input) {
        Ok(tokens) => Ok(tokens
            .into_iter()
            .map(|t| {
                (
                    t.type_.to_string(),
                    t.value.to_string(),
                    t.start_pos() as usize,
                    t.end_pos() as usize,
                )
            })
            .collect()),
        Err(ParseError::UnexpectedCharacter { pos, .. }) => Err(pos),
        // The basic lexer emits no other ParseError; treat anything else as a
        // sentinel so an unexpected variant still surfaces as a divergence.
        Err(_) => Err(usize::MAX),
    }
}

fn lexer(grammar_text: &str, backend: LexerBackend) -> BasicLexer {
    let grammar =
        load_grammar(grammar_text, &["start".to_string()], false, false).expect("grammar loads");
    let cg = lower(&grammar);
    let conf = basic_lexer_conf(&cg, 0).with_backend(backend);
    BasicLexer::new(&conf).expect("lexer builds")
}

/// Assert the `regex`-crate and `regex-automata` backends lex every input to a
/// byte-identical token stream (or fail at the same byte) for `grammar`.
fn assert_backends_agree(grammar: &str, inputs: &[&str]) {
    let rx = lexer(grammar, LexerBackend::Regex);
    let dfa = lexer(grammar, LexerBackend::Dfa);
    for &input in inputs {
        let r = lex_outcome(&rx, input);
        let d = lex_outcome(&dfa, input);
        assert_eq!(
            r, d,
            "combined-scanner divergence on input {input:?}\n  grammar: {grammar}\n  \
             Regex backend: {r:?}\n  Dfa   backend: {d:?}",
        );
    }
}

/// A **trailing** guard (`NUM`) coexists with an order-sensitive plain alternation
/// (`AB = ab|abc`). Leftmost-first must keep `"abc"` lexing as `AB("ab") C("c")`,
/// never `AB("abc")`, even though a guarded terminal is present in the grammar.
#[test]
fn trailing_guard_does_not_disturb_sibling_alternation() {
    let grammar = r#"
start: (AB | NUM | C)+
AB: /ab|abc/
NUM: /[0-9]+(?![0-9])/
C: /c/
"#;
    assert_backends_agree(
        grammar,
        &[
            "abc", "ab", "abcabc", "abcc", "cabc", "ab12", "12ab", "0", "12", "abc12abc",
        ],
    );
}

/// A **leading** guard (`G`) coexists with the same order-sensitive plain alternation.
/// A leading boundary also flips the combined engine into its guarded mode, so it must
/// be covered too.
#[test]
fn leading_guard_does_not_disturb_sibling_alternation() {
    let grammar = r#"
start: (AB | G | C)+
AB: /ab|abc/
G: /(?!0)[0-9]+/
C: /c/
"#;
    assert_backends_agree(
        grammar,
        &[
            "abc", "ab", "abcabc", "abcc", "cabc", "12", "12abc", "abc12",
        ],
    );
}

/// A **lazy** quantifier in a guarded body must never be mis-lowered to longest:
/// `T=/ab??(?!c)/` on `"ab"` is `"a"` (lazy `b??` prefers empty), and a
/// longest-accept accumulator over the guard's accept-set would wrongly pick `"ab"`.
/// Since L4 the lowering's refusal is a **categorized build error** — the dangerous
/// direction (silently lexing longest) is structurally impossible, and this pins it
/// as the NotYetImplemented decline on the engine path. (Pairs with the in-crate
/// `dfa_lazy_guarded_base_is_a_categorized_nyi_error`.)
#[test]
fn lazy_guarded_body_is_refused_never_lowered_to_longest() {
    use lark_rs::{DeclineReason, GrammarError, LookaroundIssue, Scope};
    for grammar in [
        "start: (T | C)+\nT: /ab??(?!c)/\nC: /c/\n",
        // A lazy body with a *positive* guard, beside a plain order-sensitive sibling.
        "start: (AB | T | C)+\nAB: /ab|abc/\nT: /a.??(?=c)/\nC: /c/\n",
    ] {
        let g = load_grammar(grammar, &["start".to_string()], false, false)
            .expect("grammar loads; the scope verdict lands at lexer build");
        let cg = lower(&g);
        let conf = basic_lexer_conf(&cg, 0).with_backend(LexerBackend::Dfa);
        match BasicLexer::new(&conf) {
            Err(GrammarError::LookaroundScope { scope, issue, .. }) => {
                assert_eq!(scope, Scope::NotYetImplemented);
                assert_eq!(
                    issue,
                    LookaroundIssue::Declined(DeclineReason::NonRealizableGuardedBase)
                );
            }
            Err(other) => panic!("expected the categorized NYI error, got {other:?}"),
            Ok(_) => panic!(
                "a lazy guarded base BUILT — either it was mis-lowered (the \
                 longest-accept hazard) or it was promoted without moving this pin; \
                 see docs/LOOKAROUND_SCOPE.md"
            ),
        }
    }
}

/// The shorter branch winning is genuinely order-dependent: `/abc|ab/` (longer first)
/// is leftmost-first `"abc"`, while `/ab|abc/` (shorter first) is `"ab"`. Both must
/// hold under a guard, so a longest-match engine is caught from either direction.
#[test]
fn alternation_order_is_honoured_both_ways_under_guard() {
    let shorter_first = r#"
start: (AB | NUM | C)+
AB: /ab|abc/
NUM: /[0-9]+(?![0-9])/
C: /c/
"#;
    let longer_first = r#"
start: (AB | NUM | C)+
AB: /abc|ab/
NUM: /[0-9]+(?![0-9])/
C: /c/
"#;
    assert_backends_agree(shorter_first, &["abc"]);
    assert_backends_agree(longer_first, &["abc"]);
}
