//! Whole-bank metamorphic sweep for the reconstructor (ADR-0044).
//!
//! Replays every *accepted* case of the LALR compliance bank
//! (`tests/fixtures/oracles/compliance/bank.json`) through the round-trip
//! property: for each input that lark-rs itself parses,
//! `parse(reconstruct(parse(x)))` must be structurally equal to `parse(x)`.
//! Python Lark is deliberately not the oracle here (its own `Reconstructor` is
//! experimental and its output text is not canonical); the bank supplies the
//! breadth — hundreds of real grammars — and the property supplies the check.
//!
//! Failures are gated by an XFAIL allow-list (`reconstruct_xfail.json`), the
//! same burndown discipline as the other banks: the build fails only on a
//! **regression** (a case newly failing that is not in the allow-list). Set
//! `LARK_RECONSTRUCT_WRITE_XFAIL=1` to regenerate after an intentional change,
//! then review the diff before committing.
//!
//! Failure identities are diagnosable by prefix:
//! - `placeholders:{ri}` — the grammar uses `maybe_placeholders` `[...]`
//!   placeholders, which reconstruction refuses by contract (a typed error).
//! - `recons:{ri}:{ci}` — reconstruction returned an error (e.g. a discarded
//!   regex terminal with no substitution — the sweep passes no `term_subs`).
//! - `reparse:{ri}:{ci}` — the reconstructed text failed to re-parse.
//! - `tree:{ri}:{ci}` — it re-parsed to a structurally different tree.

mod common;

use lark_rs::grammar::terminal::flags;
use lark_rs::reconstruct::{ReconstructError, Reconstructor};
use lark_rs::{Lark, LarkOptions, LexerType, ParseTree, ParserAlgorithm};
use serde_json::Value;
use std::collections::BTreeSet;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/oracles/compliance")
}

fn load_json(name: &str) -> Option<Value> {
    let path = fixtures_dir().join(name);
    let text = std::fs::read_to_string(&path).ok()?;
    Some(serde_json::from_str(&text).expect("valid JSON"))
}

fn load_string_set(name: &str) -> BTreeSet<String> {
    load_json(name)
        .and_then(|v| v.as_array().cloned())
        .map(|a| {
            a.into_iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
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

fn try_build(grammar: &str, opts: LarkOptions) -> Option<Lark> {
    match catch_unwind(AssertUnwindSafe(|| Lark::new(grammar, opts))) {
        Ok(Ok(lark)) => Some(lark),
        _ => None,
    }
}

fn try_parse(lark: &Lark, input: &str) -> Option<ParseTree> {
    match catch_unwind(AssertUnwindSafe(|| lark.parse(input))) {
        Ok(Ok(tree)) => Some(tree),
        _ => None,
    }
}

use common::parse_tree_structural_eq as parse_tree_eq;

#[test]
fn test_reconstruct_bank() {
    let Some(records) = load_json("bank.json") else {
        eprintln!("compliance bank.json not found — run tools/extract_lark_compliance.py");
        return;
    };
    let records = records.as_array().expect("bank is an array");

    // Silence panic backtraces from grammars lark-rs cannot build/parse.
    std::panic::set_hook(Box::new(|_| {}));

    // Grammars that abort the lark-rs process (see test_compliance.rs).
    let skip = load_string_set("skip.json");

    let mut failures: BTreeSet<String> = BTreeSet::new();
    let mut total = 0usize; // round-trips attempted (lark-rs parsed the input)
    let mut refused_placeholders = 0usize;
    let mut out_of_domain = 0usize; // lark-rs didn't build/parse — not ours to judge

    let trace = std::env::var("LARK_RECONSTRUCT_TRACE").is_ok();
    for (ri, rec) in records.iter().enumerate() {
        let grammar = rec["grammar"].as_str().unwrap_or("");
        if skip.contains(grammar) || rec["construct_error"].as_bool().unwrap_or(false) {
            continue;
        }
        let cases = rec["cases"].as_array().map(Vec::as_slice).unwrap_or(&[]);
        if !cases
            .iter()
            .any(|c| c["should_parse"].as_bool() == Some(true))
        {
            continue;
        }
        if trace {
            eprintln!("[{ri}] grammar={grammar:?}");
        }
        let Some(lark) = try_build(grammar, record_options(rec)) else {
            out_of_domain += cases.len();
            continue;
        };

        let recons = match Reconstructor::new(&lark) {
            Ok(r) => Some(r),
            Err(ReconstructError::MaybePlaceholders) => {
                // Refused by contract; tracked in the ledger as one entry so
                // the class stays visible (and burns down if ever supported).
                refused_placeholders += 1;
                failures.insert(format!("placeholders:{ri}"));
                None
            }
            Err(e) => panic!("Reconstructor::new failed unexpectedly on [{ri}]: {e}"),
        };

        for (ci, case) in cases.iter().enumerate() {
            if case["should_parse"].as_bool() != Some(true) {
                continue;
            }
            let input = case["input"].as_str().unwrap_or("");
            let Some(tree) = try_parse(&lark, input) else {
                out_of_domain += 1; // a compliance gap, already xfail'd there
                continue;
            };
            let Some(recons) = &recons else { continue };
            total += 1;

            let details = std::env::var("LARK_RECONSTRUCT_DETAILS").is_ok();
            match recons.reconstruct(&tree) {
                Err(e) => {
                    if details {
                        eprintln!("recons:{ri}:{ci} input={input:?} err={e}");
                    }
                    failures.insert(format!("recons:{ri}:{ci}"));
                }
                Ok(text) => match try_parse(&lark, &text) {
                    None => {
                        if details {
                            eprintln!(
                                "reparse:{ri}:{ci} input={input:?} text={text:?}\n  grammar={grammar:?}"
                            );
                        }
                        failures.insert(format!("reparse:{ri}:{ci}"));
                    }
                    Some(tree2) => {
                        if !parse_tree_eq(&tree, &tree2) {
                            if details {
                                eprintln!("tree:{ri}:{ci} input={input:?} text={text:?}");
                            }
                            failures.insert(format!("tree:{ri}:{ci}"));
                        }
                    }
                },
            }
        }
    }

    let xfail = load_string_set("reconstruct_xfail.json");
    let passing = total
        - failures
            .iter()
            .filter(|f| !f.starts_with("placeholders:"))
            .count();
    eprintln!(
        "reconstruct bank: {passing}/{total} round-trips hold; \
         {} known-XFAIL ({refused_placeholders} placeholder-refused grammars), \
         {out_of_domain} cases out of domain (lark-rs build/parse gaps, tracked by \
         the compliance bank)",
        xfail.len()
    );

    if std::env::var("LARK_RECONSTRUCT_WRITE_XFAIL").is_ok() {
        let list: Vec<&String> = failures.iter().collect();
        let path = fixtures_dir().join("reconstruct_xfail.json");
        std::fs::write(&path, serde_json::to_string_pretty(&list).unwrap() + "\n")
            .expect("write reconstruct_xfail.json");
        eprintln!(
            "wrote {} XFAIL entries to {}",
            failures.len(),
            path.display()
        );
        return;
    }

    let regressions: Vec<&String> = failures.difference(&xfail).collect();
    let fixed: Vec<&String> = xfail.difference(&failures).collect();
    if !fixed.is_empty() {
        eprintln!(
            "note: {} XFAIL entries now pass — consider regenerating reconstruct_xfail.json",
            fixed.len()
        );
    }
    assert!(
        regressions.is_empty(),
        "reconstruct round-trip regressions ({} newly failing, not in reconstruct_xfail.json):\n{}",
        regressions.len(),
        regressions
            .iter()
            .take(40)
            .map(|s| format!("  - {s}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}
