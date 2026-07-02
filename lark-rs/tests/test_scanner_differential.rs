//! L0 — the differential oracle for the lexer DFA rewrite (`docs/LEXER_DFA_PLAN.md`).
//!
//! Every token in the lexer funnels through one seam — `ScannerBackend::match_at`
//! (`src/lexer/`), shared by `BasicLexer` and the per-state `ContextualLexer`.
//! Two interchangeable engines sit behind it: the original `regex`-crate `Scanner`
//! ([`LexerBackend::Regex`]) and the `regex-automata` DFA ([`LexerBackend::Dfa`]).
//! This test is the contract that the swap changes **nothing**: for the same
//! grammar and input, both backends must produce a **byte-identical** token stream,
//! and on a lex failure must fail at the **same byte position**.
//!
//! The corpora are the ones where the `regex` crate is the rock-solid reference:
//!
//!   * the LALR compliance bank (`fixtures/oracles/compliance/bank.json`) — every
//!     grammar strip-mined from Python Lark's own suite, with its captured inputs;
//!   * the JSONTestSuite corpus (293 files) under `json.lark`;
//!   * a couple of real Python source files under the bundled `python.lark` (which
//!     exercises the plain *and* the `fancy-regex` lookaround terminals together).
//!
//! The differential runs at the **lexer** level (a `BasicLexer` per backend) rather
//! than through a full `Lark`, so it isolates exactly the seam under test and steers
//! clear of the multi-second LALR-table builds (e.g. `python.lark`'s). The
//! `ContextualLexer` shares the same `match_at` seam, so the engine swap is covered
//! by construction.
//!
//! The L1 `DfaScanner` is live behind [`LexerBackend::Dfa`], so this drives the
//! real engine swap: every divergence here is a genuine `regex` vs `regex-automata`
//! difference, localized to `match_at`. It runs as part of `cargo test --all` (the
//! `scripts/check.sh` gate).

mod common;

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;

use common::lowering::{
    corpus, long_string_idiom_terminals, regexp_idiom_terminals, string_idiom_terminals,
    supported_terminals, GenTerminal, LONG_STRING_RAW, REGEXP_RAW,
};
use lark_rs::grammar::terminal::flags;
use lark_rs::{
    basic_lexer_conf, load_grammar, lower, lower_terminal, lower_terminal_dotall, BasicLexer,
    Lexer, LexerBackend, Lowered, ParseError,
};
use serde_json::Value;

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// The lex outcome reduced to what the differential compares: the full token
/// stream on success, or the failing byte position on a lexer error. `BasicLexer`
/// only ever fails with [`ParseError::UnexpectedCharacter`], so the position is the
/// whole story of a failure.
type LexOutcome = Result<Vec<lark_rs::Token>, usize>;

fn lex_outcome(lexer: &BasicLexer, input: &str) -> LexOutcome {
    match lexer.lex(input) {
        Ok(tokens) => Ok(tokens),
        Err(ParseError::UnexpectedCharacter { pos, .. }) => Err(pos),
        // The basic lexer emits no other ParseError; treat anything else as a
        // sentinel so an unexpected variant still surfaces as a divergence.
        Err(_) => Err(usize::MAX),
    }
}

/// Build a `BasicLexer` for `grammar_text` under `backend`, or `None` if the
/// grammar/lexer cannot be built (an unimplemented feature, an invalid pattern, or
/// a loader panic). The differential only compares grammars **both** backends build.
fn build_lexer(
    grammar_text: &str,
    start: &[String],
    maybe_placeholders: bool,
    keep_all_tokens: bool,
    g_regex_flags: u32,
    backend: LexerBackend,
) -> Option<BasicLexer> {
    catch_unwind(AssertUnwindSafe(|| {
        let grammar =
            load_grammar(grammar_text, start, maybe_placeholders, keep_all_tokens).ok()?;
        let cg = lower(&grammar);
        let conf = basic_lexer_conf(&cg, g_regex_flags).with_backend(backend);
        BasicLexer::new(&conf).ok()
    }))
    .ok()
    .flatten()
}

/// Compare two lex outcomes; on divergence return a short human-readable
/// description of the **first** difference (mismatched token, length, or error
/// position), else `None`.
fn diff_outcomes(a: &LexOutcome, b: &LexOutcome) -> Option<String> {
    match (a, b) {
        (Ok(ta), Ok(tb)) => {
            if ta.len() != tb.len() {
                return Some(format!(
                    "token count {} (Regex) != {} (Dfa)",
                    ta.len(),
                    tb.len()
                ));
            }
            for (i, (x, y)) in ta.iter().zip(tb.iter()).enumerate() {
                if x != y {
                    return Some(format!(
                        "token {i}: Regex {:?}={:?}@{}..{} != Dfa {:?}={:?}@{}..{}",
                        x.type_,
                        x.value,
                        x.start_pos(),
                        x.end_pos(),
                        y.type_,
                        y.value,
                        y.start_pos(),
                        y.end_pos()
                    ));
                }
            }
            None
        }
        (Err(pa), Err(pb)) => {
            (pa != pb).then(|| format!("lex error position {pa} (Regex) != {pb} (Dfa)"))
        }
        (Ok(t), Err(p)) => Some(format!(
            "Regex lexed {} tokens but Dfa failed at byte {p}",
            t.len()
        )),
        (Err(p), Ok(t)) => Some(format!(
            "Regex failed at byte {p} but Dfa lexed {} tokens",
            t.len()
        )),
    }
}

/// A single grammar + its inputs, run under both backends. Records build-parity and
/// per-input divergences into `failures`, counting comparisons into `compared`.
struct Differential {
    failures: Vec<String>,
    compared: usize,
    grammars: usize,
    /// Lookaround grammars the Dfa backend *declines* to lower (and so routes to
    /// `fancy-regex`, exactly as the Regex backend does): a non-greedy-monotone guarded
    /// base, a variable-offset lookbehind, an internal lookahead. Both backends agree
    /// trivially on these, so they are tracked as a **pending** (routed-to-fancy) skip
    /// rather than a lowered comparison. With every shape and bundled idiom landed
    /// (M1–M4 + the Stage-B REGEXP/LONG_STRING idioms) the whole generated population
    /// lowers, so this is 0 today; it stays live for any future generated decline.
    pending: usize,
    /// Lookaround grammars actually *compared* (their lowering landed) — a
    /// **dedicated** counter, separate from the shared `grammars`/`compared` the
    /// bank/JSON/Python corpora bump, so the "population is wired in" guard measures
    /// the lookaround population specifically (not "hundreds of bank grammars exist").
    lookaround_compared_grammars: usize,
    lookaround_compared_inputs: usize,
}

impl Differential {
    fn new() -> Self {
        Differential {
            failures: Vec::new(),
            compared: 0,
            grammars: 0,
            pending: 0,
            lookaround_compared_grammars: 0,
            lookaround_compared_inputs: 0,
        }
    }

    /// Build both backends for one grammar and lex each input under both. `label`
    /// identifies the grammar in any divergence message.
    fn run(
        &mut self,
        label: &str,
        grammar_text: &str,
        start: &[String],
        maybe_placeholders: bool,
        keep_all_tokens: bool,
        g_regex_flags: u32,
        inputs: impl IntoIterator<Item = (String, String)>,
    ) {
        let mk = |backend| {
            build_lexer(
                grammar_text,
                start,
                maybe_placeholders,
                keep_all_tokens,
                g_regex_flags,
                backend,
            )
        };
        let (lex_a, lex_b) = (mk(LexerBackend::Regex), mk(LexerBackend::Dfa));

        // Build parity is itself part of the contract: the engine swap must not
        // change whether a lexer builds.
        match (&lex_a, &lex_b) {
            (Some(a), Some(b)) => {
                self.grammars += 1;
                for (input_label, input) in inputs {
                    self.compared += 1;
                    let oa = lex_outcome(a, &input);
                    let ob = lex_outcome(b, &input);
                    if let Some(diff) = diff_outcomes(&oa, &ob) {
                        self.failures
                            .push(format!("{label} [{input_label}]: {diff}"));
                    }
                }
            }
            (None, None) => {} // neither builds — agree, nothing to compare
            (Some(_), None) => self
                .failures
                .push(format!("{label}: Regex backend built but Dfa did not")),
            (None, Some(_)) => self
                .failures
                .push(format!("{label}: Dfa backend built but Regex did not")),
        }
    }
}

/// Pull a start-symbol list out of a bank record's `start` field (string or array).
fn record_start(rec: &Value) -> Vec<String> {
    match &rec["start"] {
        Value::String(s) => vec![s.clone()],
        Value::Array(a) => a
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
        _ => vec!["start".to_string()],
    }
}

/// Map the bank's canonical `imsx` flag letters back to lark-rs's flag bitset.
fn record_flags(rec: &Value) -> u32 {
    let mut g = 0u32;
    if let Some(letters) = rec["g_regex_flags"].as_str() {
        for ch in letters.chars() {
            g |= match ch {
                'i' => flags::IGNORECASE,
                'm' => flags::MULTILINE,
                's' => flags::DOTALL,
                'x' => flags::VERBOSE,
                _ => 0,
            };
        }
    }
    g
}

/// The compliance bank: every grammar + its captured inputs, both backends.
fn run_compliance_bank(d: &mut Differential) {
    let path = manifest_dir().join("tests/fixtures/oracles/compliance/bank.json");
    let Ok(text) = std::fs::read_to_string(&path) else {
        eprintln!("compliance bank.json not found — skipping that corpus");
        return;
    };
    let bank: Value = serde_json::from_str(&text).expect("valid bank.json");
    for (ri, rec) in bank
        .as_array()
        .expect("bank is an array")
        .iter()
        .enumerate()
    {
        // A grammar Python Lark rejects at construction has no lexer to compare.
        if rec["construct_error"].as_bool().unwrap_or(false) {
            continue;
        }
        let grammar = rec["grammar"].as_str().unwrap_or("");
        let inputs: Vec<(String, String)> = rec["cases"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or(&[])
            .iter()
            .enumerate()
            .map(|(ci, case)| {
                (
                    format!("case {ci}"),
                    case["input"].as_str().unwrap_or("").to_string(),
                )
            })
            .collect();
        if inputs.is_empty() {
            continue;
        }
        d.run(
            &format!("bank[{ri}]"),
            grammar,
            &record_start(rec),
            rec["maybe_placeholders"].as_bool().unwrap_or(true),
            rec["keep_all_tokens"].as_bool().unwrap_or(false),
            record_flags(rec),
            inputs,
        );
    }
}

/// The JSONTestSuite corpus under `json.lark` (skipped if the submodule is absent).
fn run_json_corpus(d: &mut Differential) {
    let corpus_dir = manifest_dir().join("tests/corpora/JSONTestSuite/test_parsing");
    if !corpus_dir.exists() {
        eprintln!("JSONTestSuite submodule not initialised — skipping that corpus");
        return;
    }
    let grammar = std::fs::read_to_string(manifest_dir().join("tests/grammars/json.lark"))
        .expect("read json.lark");

    let mut entries: Vec<_> = std::fs::read_dir(&corpus_dir)
        .expect("read corpus dir")
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().map_or(false, |x| x == "json"))
        .collect();
    entries.sort_by_key(|e| e.file_name());

    let inputs: Vec<(String, String)> = entries
        .iter()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            std::fs::read_to_string(e.path()).ok().map(|t| (name, t))
        })
        .collect();

    d.run(
        "json_corpus",
        &grammar,
        &["start".to_string()],
        false,
        false,
        0,
        inputs,
    );
}

/// A couple of real Python source files under the bundled `python.lark`. The inputs
/// are capped at a line boundary near `CAP` bytes to keep the corpus quick: the
/// **Regex** reference backend still pays the (constant-factor-heavy) `fancy-regex`
/// side-probes for every lookaround terminal, while the **Dfa** side now lexes
/// python.lark fully lowered (STRING via the M4 splice, LONG_STRING via the Stage-B
/// idiom — the files' real docstrings drive it). Still exercises strings, comments,
/// numbers, names, and operators on real code.
fn run_python_files(d: &mut Differential) {
    const CAP: usize = 6_000;
    let grammar = std::fs::read_to_string(manifest_dir().join("src/grammars/python.lark")).ok();
    let Some(grammar) = grammar else {
        eprintln!("python.lark not found — skipping that corpus");
        return;
    };

    let cap_at_line = |s: &str| -> String {
        if s.len() <= CAP {
            return s.to_string();
        }
        let cut = s[..CAP].rfind('\n').map(|i| i + 1).unwrap_or(CAP);
        s[..cut].to_string()
    };

    let candidates = [
        "tools/generate_oracles.py",
        "tools/extract_lark_compliance.py",
    ];
    let inputs: Vec<(String, String)> = candidates
        .iter()
        .filter_map(|rel| {
            std::fs::read_to_string(manifest_dir().join(rel))
                .ok()
                .map(|t| (rel.to_string(), cap_at_line(&t)))
        })
        .collect();
    if inputs.is_empty() {
        eprintln!("no Python source files found — skipping that corpus");
        return;
    }

    d.run(
        "python.lark",
        &grammar,
        &["file_input".to_string()],
        true,
        false,
        0,
        inputs,
    );
}

/// Real `.lark` grammar files lexed under the bundled `lark.lark` (which is
/// self-hosting). This drives the **real `lark.OP`** terminal — `/[+*]|[?](?![a-z])/`,
/// a trailing-boundary guard M1 lowers — under the Dfa backend over genuine grammar
/// text full of `+`/`*`/`?` EBNF operators, alongside the now-lowered `REGEXP`
/// (the Stage-B regex-literal idiom) over the files' real `/…/` terminal patterns.
/// Both backends must lex identically.
fn run_lark_files(d: &mut Differential) {
    let grammar = std::fs::read_to_string(manifest_dir().join("src/grammars/lark.lark")).ok();
    let Some(grammar) = grammar else {
        eprintln!("lark.lark not found — skipping that corpus");
        return;
    };

    let candidates = [
        "tests/grammars/arithmetic.lark",
        "tests/grammars/json.lark",
        "tests/grammars/csv.lark",
        "src/grammars/common.lark",
    ];
    let inputs: Vec<(String, String)> = candidates
        .iter()
        .filter_map(|rel| {
            std::fs::read_to_string(manifest_dir().join(rel))
                .ok()
                .map(|t| (rel.to_string(), t))
        })
        .collect();
    if inputs.is_empty() {
        eprintln!("no .lark source files found — skipping that corpus");
        return;
    }

    d.run(
        "lark.lark",
        &grammar,
        &["start".to_string()],
        false,
        false,
        0,
        inputs,
    );
}

/// A generated single-terminal lookaround grammar: `start: TOK+` over the
/// generated terminal, plus the raw terminal pattern for the lowerability check.
fn lookaround_grammar(t: &GenTerminal) -> (String, String) {
    // Inside `/…/`, a literal `/` must be escaped as `\/`. Escape only an *unescaped*
    // slash (one not already preceded by a backslash) so an already-`\/` pattern is
    // not double-escaped into `\\/`.
    let mut escaped = String::with_capacity(t.pattern.len());
    let mut prev_backslash = false;
    for ch in t.pattern.chars() {
        if ch == '/' && !prev_backslash {
            escaped.push('\\');
        }
        escaped.push(ch);
        prev_backslash = ch == '\\' && !prev_backslash;
    }
    let grammar = format!("start: {name}+\n{name}: /{escaped}/\n", name = t.name);
    (grammar, t.pattern.clone())
}

/// The **generated lookaround-grammar population** (the master differential's
/// fourth corpus, `docs/LEXER_DFA_PLAN.md` layer 1). For each generated supported
/// terminal: build both backends (build parity is enforced), then either compare
/// token streams over the terminal's exhaustive corpus — *iff* the terminal actually
/// lowers ([`lower_terminal`] returns `Ok`) — or record it as a **pending**
/// (routed-to-fancy) skip when the lowering declines it. With every shape and bundled
/// idiom landed the generated population is all lowerable, so every grammar here is
/// compared; the pending path stays live for any future generated decline.
fn run_lookaround_grammars(d: &mut Differential) {
    // The bare boundary/lookbehind population, python.STRING's real nested/prefixed
    // opening-guard idiom (the marquee L2 splice), lark.REGEXP's regex-literal idiom,
    // *and* python.LONG_STRING's long-string idiom (Stage B) — all must lex
    // byte-identically under the two backends, and (with the splice + idioms landed)
    // all genuinely lower.
    for t in supported_terminals()
        .into_iter()
        .chain(string_idiom_terminals())
        .chain(regexp_idiom_terminals())
        .chain(long_string_idiom_terminals())
    {
        let (grammar, pattern) = lookaround_grammar(&t);
        let start = ["start".to_string()];
        let mk = |backend| build_lexer(&grammar, &start, false, false, 0, backend);
        let (lex_a, lex_b) = (mk(LexerBackend::Regex), mk(LexerBackend::Dfa));

        match (&lex_a, &lex_b) {
            (Some(a), Some(b)) => {
                // Does this terminal actually lower? `Ok` → gated token-stream
                // comparison; an `Err` (the lowering declines it, routing to fancy) →
                // a tracked pending skip where both backends agree trivially.
                if lower_terminal(&t.name, &pattern).is_ok() {
                    d.lookaround_compared_grammars += 1;
                    for input in corpus(&t.alphabet, t.max_len) {
                        d.lookaround_compared_inputs += 1;
                        let oa = lex_outcome(a, &input);
                        let ob = lex_outcome(b, &input);
                        if let Some(diff) = diff_outcomes(&oa, &ob) {
                            d.failures
                                .push(format!("lookaround/{} [{input:?}]: {diff}", t.name));
                        }
                    }
                } else {
                    d.pending += 1;
                }
            }
            (None, None) => {}
            (Some(_), None) => d.failures.push(format!(
                "lookaround/{}: Regex backend built but Dfa did not",
                t.name
            )),
            (None, Some(_)) => d.failures.push(format!(
                "lookaround/{}: Dfa backend built but Regex did not",
                t.name
            )),
        }
    }
}

#[test]
fn test_scanner_backends_lex_identically() {
    // The loader emits panic backtraces for the many bank grammars that exercise
    // unimplemented features; silence them (we already treat a build panic as
    // "did not build", same as test_compliance.rs).
    std::panic::set_hook(Box::new(|_| {}));

    let mut d = Differential::new();
    run_compliance_bank(&mut d);
    run_json_corpus(&mut d);
    run_python_files(&mut d);
    run_lark_files(&mut d);
    run_lookaround_grammars(&mut d);

    let _ = std::panic::take_hook();

    // Deliverable 3: the python.lark differential is run *with STRING lowered*, not
    // silently routed to `fancy-regex`. Confirm the bundled STRING actually lowers to
    // branches (the splice landed) — so the 0-divergence result above is the lowered
    // engine agreeing with fancy, not two fancy side-probes trivially agreeing.
    const STRING_RAW: &str =
        r#"([ubf]?r?|r[ubf])("(?!"").*?(?<!\\)(\\\\)*?"|'(?!'').*?(?<!\\)(\\\\)*?')"#;
    assert!(
        matches!(
            lower_terminal_dotall("STRING", STRING_RAW, false),
            Ok(Lowered::Branches(_))
        ),
        "python.STRING must LOWER (Lowered::Branches) under the splice, not route to fancy"
    );
    // Likewise lark.REGEXP: the lark.lark differential above runs with REGEXP *lowered*
    // (the Stage-B regex-literal idiom), so its 0-divergence result is the lowered
    // engine agreeing with fancy on real grammar files, not two fancy probes agreeing.
    assert!(
        matches!(
            lower_terminal_dotall("REGEXP", REGEXP_RAW, false),
            Ok(Lowered::Branches(_))
        ),
        "lark.REGEXP must LOWER (Lowered::Branches) via the regex-literal idiom"
    );
    // And python.LONG_STRING: the python.lark differential above runs with LONG_STRING
    // *lowered* (the Stage-B long-string idiom), so the real docstring-bearing Python
    // source is the lowered engine agreeing with fancy — the Dfa side now lexes
    // python.lark with no fancy side-probe at all.
    assert!(
        matches!(
            lower_terminal_dotall("LONG_STRING", LONG_STRING_RAW, true),
            Ok(Lowered::Branches(_))
        ),
        "python.LONG_STRING must LOWER (Lowered::Branches) via the long-string idiom"
    );

    eprintln!(
        "scanner differential: {} input(s) across {} grammar(s) compared; \
         lookaround: {} pending + {} grammar(s)/{} input(s) compared; {} divergence(s)",
        d.compared,
        d.grammars,
        d.pending,
        d.lookaround_compared_grammars,
        d.lookaround_compared_inputs,
        d.failures.len()
    );

    // The lookaround population must be *tracked* — as gated comparisons (lowered
    // shapes) or as pending (routed-to-fancy declines). Both counters are
    // lookaround-specific, so this can't be satisfied by the bank/JSON/Python grammars
    // alone: a silently-dropped lookaround population fails here.
    assert!(
        d.pending > 0 || d.lookaround_compared_grammars > 0,
        "no lookaround grammars were tracked — the generated population is missing"
    );

    assert!(
        d.failures.is_empty(),
        "Regex vs Dfa scanner divergences ({}):\n{}",
        d.failures.len(),
        d.failures
            .iter()
            .take(40)
            .map(|f| format!("  - {f}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}
