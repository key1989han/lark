//! Standalone parser generation (issue #42, Phase 3).
//!
//! Emits a *self-contained* Rust source file that parses a fixed grammar without
//! any dependency on lark-rs at parse time — only the `regex` crate and the Rust
//! standard library. This mirrors Python Lark's `lark.tools.standalone`, which
//! bakes a grammar into a single importable module.
//!
//! ## What this is for (and what it is not)
//!
//! The value is **dependency footprint and parity with Python's `standalone`**:
//! exporting a parser to a consumer that must not depend on lark-rs (a small crate,
//! a vendored file, a different build graph). It is deliberately *not* a throughput
//! play and *not* (yet) a `no_std`/firmware artifact:
//!
//!   * the baked parser is still **table-interpreted** (it walks the same dense
//!     ACTION/GOTO tables the in-process engine does), so it is not faster than
//!     lark-rs — hence there is no benchmark here;
//!   * the lexer **compiles its combined regex at runtime** on first use, and the
//!     shim uses `regex` + `HashMap`/`HashSet`, so the output is not `#![no_std]`.
//!
//! The issue gestures at no-std firmware/wasm as motivation; that remains future
//! work (it would need a baked DFA lexer and an alloc-only runtime). What ships
//! here is the self-contained-export use case, which is the Python-parity goal.
//!
//! ## What is baked
//!
//! The generator runs the *normal* lark-rs pipeline once at build time:
//!
//! ```text
//! .lark text → load_grammar → lower → build_lalr_table  (ParseTable)
//!                                   → basic_lexer_conf + scanner_plan (lexer)
//! ```
//!
//! and serializes the results into one `static DATA: GrammarData` (see
//! [`runtime::GrammarData`]): the sparse LALR ACTION/GOTO tables, every rule's
//! tree-shaping flags, the symbol-name table, and the
//! [`ScannerPlan`](crate::lexer::ScannerPlan) (alternation order + each terminal's
//! inline regex + the `unless` keyword-retype map + `%ignore` + global flags).
//!
//! ## How drift is prevented
//!
//! Two pieces could drift from the in-process engine; both are *shared by
//! construction* rather than re-derived:
//!
//!   * the **lexer recipe** comes from the same [`scanner_plan`](crate::lexer::scanner_plan)
//!     `Scanner::build` uses, so the baked scanner is byte-identical; and
//!   * the **driver** (lexer loop + LALR reduce/shape) lives in [`runtime`], a real
//!     compiled, type-checked, unit-tested module — `include_str!`d into the
//!     generated file, never hand-copied as text.
//!
//! A generated parser therefore produces byte-identical trees to lark-rs, pinned
//! by `tests/test_standalone.rs` (committed fixtures compiled + run vs the live
//! oracle, plus a determinism/freshness gate).
//!
//! ## Generated-code API surface — struct-shape change (#457/#522)
//!
//! The generated parser exposes its own `Tree` / `Token` / `Meta` types (the
//! [`runtime`] shapes, emitted verbatim into each file). **#522 grew that public
//! surface** to carry source positions; downstream code that *destructures* or
//! *constructs* these values must update. The additions are:
//!
//!   * `Token`: `end_line`, `end_column`, `start_pos`, `end_pos` position fields;
//!   * `Tree`: a `meta: Meta` field;
//!   * `Meta` (new): `line`/`column`/`end_line`/`end_column`/`start_pos`/`end_pos`
//!     (all `Option<usize>`) + `empty: bool`. Its `Default` is **hand-written**, not
//!     derived — a position-less default `Meta` is `empty: true` (see
//!     [`runtime::Meta`]);
//!   * `ContainerSpan` (internal) + `GrammarData::propagate_positions`, threaded so a
//!     baked parser honors `propagate_positions` byte-identically to the in-process
//!     engine (#402).
//!
//! Older generated parsers (pre-#522) have none of these; regenerate the fixture to
//! pick them up (`LARK_STANDALONE_WRITE=1`). This is a generated-code API change, not
//! a lark-rs library-API change.
//!
//! ## Limitations (documented parity gaps)
//!
//!   * **LALR only** — the baked artifact is a `ParseTable`; Earley/CYK are not
//!     supported (the generator returns an error).
//!   * **Basic lexer only** — the standalone lexer is the combined-regex basic
//!     lexer, not the contextual lexer. Grammars that *require* the contextual
//!     lexer to resolve terminal collisions are rejected by Python Lark's
//!     standalone tool too; here they will simply fail to lex at runtime.
//!   * **No postlex** — `%declare` + an `Indenter` postlex hook is not baked
//!     (the generator returns an error if one is configured).
//!   * **No lookaround grammars — REJECTED at bake time (#280), not baked.**
//!     The baked `ScannerPlan` is a *regex* alternation (each terminal's inline pattern
//!     compiled on the `regex` crate at runtime), so a grammar with **lookaround**
//!     terminals (the bundled `python`/`lark`) is **not** standalone-able: the regex
//!     runtime cannot host `(?!…)`/`(?<…)`, and this generator does not bake the
//!     `regex-automata` DFA scanner bundle or its guard side-tables. The in-process
//!     lexer lowers such terminals into the DFA backend, so the core engine builds
//!     these grammars fine; to prevent the standalone bake from shipping a
//!     `regex`-rejected pattern that would **panic** the generated parser at
//!     `Regex::new(...).expect(...)`, [`bake`] now runs every baked terminal through
//!     [`check_standalone_regex_hostable`](crate::lexer::check_standalone_regex_hostable)
//!     — the standalone analogue of the engine-build refusal seam — and returns a clear
//!     compile-time error (RC10), as does any `\Z` anchor (V1) or oversized bounded
//!     repeat (V2) the pure-`regex` runtime cannot compile. Closing the *capability*
//!     gap (actually baking lookaround) is **L5** of the lexer DFA plan (serialize the
//!     plain + guarded DFAs, guard/lookbehind tables, rank maps, start-byte prefilter,
//!     `unless`, and `%ignore`, and replace the `ScannerPlan` path with it). L4 (drop
//!     runtime `fancy-regex`) has landed, so L5 is unblocked. See
//!     `docs/LEXER_DFA_PLAN.md` (L5) and `docs/LEXER_DFA_STATUS.md`.

// Compiled + type-checked here so the embedded driver cannot rot, then `include_str!`d
// into every generated parser. `dead_code` is expected: nothing in the lib's normal
// build path calls it (the round-trip fixtures and the unit test below do).
#[allow(dead_code)]
pub mod runtime;

use std::fmt::Write as _;

use crate::error::{GrammarError, LarkError};
use crate::grammar::load_grammar_with_base;
use crate::lexer::{
    check_regex_collisions, check_standalone_regex_hostable, check_zero_width_terminals,
    scanner_plan,
};
use crate::parsers::basic_lexer_conf;
use crate::parsers::lalr::{build_lalr_table, Action};
use crate::{LarkOptions, ParserAlgorithm};

/// A rule's baked tree-shaping metadata, owned (the lifetime-free mirror of
/// [`runtime::RuleData`]).
struct BakedRule {
    origin: u32,
    len: u32,
    tree_name: String,
    transparent: bool,
    expand1: bool,
    has_alias: bool,
    keep_all: bool,
    filter_pos: Vec<bool>,
    placeholder_count: u32,
    nones_before: Vec<u32>,
    is_start: bool,
}

/// The fully-baked, owned grammar tables — the single source both the code
/// emitter ([`emit_data`]) and the in-process oracle runner (`leak_grammar_data`
/// in tests) read from, so the bytes a generated parser ships are the bytes the
/// compliance oracle actually exercises. The lifetime-free mirror of
/// [`runtime::GrammarData`].
struct Baked {
    n_terminals: u32,
    symbol_names: Vec<String>,
    rules: Vec<BakedRule>,
    /// Sparse `(terminal id, action)` per state.
    action: Vec<Vec<(u32, Action)>>,
    /// Sparse `(nonterminal index, next state)` per state.
    goto: Vec<Vec<(u32, u32)>>,
    start_states: Vec<(String, u32)>,
    start_default: String,
    global_prefix: String,
    scan_groups: Vec<(u32, String)>,
    unless: Vec<(u32, Vec<(String, bool, u32)>)>,
    ignore: Vec<u32>,
    propagate_positions: bool,
}

/// Run the normal pipeline and collect everything a standalone parser needs into
/// an owned [`Baked`]. Applies the same basic-lexer build-time validation
/// `build_frontend`'s LALR/basic path does — zero-width terminals are rejected
/// always, and (under `strict`) same-priority regex-terminal collisions — so the
/// standalone backend accepts exactly what the in-process basic lexer would.
fn bake(grammar_src: &str, options: &LarkOptions) -> Result<Baked, LarkError> {
    if options.parser != ParserAlgorithm::Lalr {
        return Err(LarkError::Grammar(GrammarError::Other {
            msg: "standalone generation supports only parser='lalr'".to_string(),
        }));
    }
    if options.postlex.is_some() {
        return Err(LarkError::Grammar(GrammarError::Other {
            msg: "standalone generation does not support a postlex (Indenter) hook".to_string(),
        }));
    }
    // `propagate_positions` is now supported (#457, option a): the standalone runtime
    // (`runtime.rs`) grew a `Tree.meta` span and the byte-offset fields on `Token` it
    // derives from, and `bake` threads the flag below so a node's `meta` spans its
    // rule's **pre-filter** children — byte-identical to the in-process LALR engine's
    // `TreeOutputBuilder::with_propagate_positions` (#402). The #425 fail-loud
    // rejection that stood here while the runtime lacked span support is therefore
    // removed (its guard test in `tests/test_standalone.rs` is retired). When the flag
    // is false the runtime still populates `meta` from post-filter children, exactly
    // as the in-process default does.

    // Run the same front-door config-legality gate the in-process build runs
    // (`build_frontend` → `validate_config`, bug-bounty N5/N6, #273) so the two
    // front doors reject identical illegal configs (#298). The standalone backend
    // is LALR + basic-lexer only, but a caller can still *set* an illegal
    // `lexer`/`ambiguity` (e.g. `lexer=dynamic` or `ambiguity=explicit` on the
    // LALR-only standalone path); without this gate they were silently accepted
    // where the in-process API rejects them — an unfalsifiable, more-permissive
    // asymmetry (ADR-0017). The parser==Lalr guard above already fired for non-LALR
    // parsers, so what this adds for standalone is the lexer matrix + ambiguity legality.
    crate::parsers::validate_config(options)?;

    let grammar = load_grammar_with_base(
        grammar_src,
        &options.start,
        options.maybe_placeholders,
        options.keep_all_tokens,
        options.base_path.clone(),
    )?;
    let cg = crate::grammar::lower(&grammar);
    let table = build_lalr_table(&cg, options.strict)?;
    // Run the same post-lowering reduce/reduce audit the live LALR build runs
    // (RC7/#272, ADR-0013) — shared helper so standalone generation can never bake a
    // parser for a grammar the live LALR build and the oracle reject.
    crate::parsers::lalr::audit_lalr_reduce_reduce(&grammar, options.strict)?;
    let lexer_conf = basic_lexer_conf(&cg, options.g_regex_flags);

    // Mirror build_frontend's LALR/basic sanitization (the standalone lexer is the
    // basic lexer, so it must reject what that lexer would).
    check_zero_width_terminals(&lexer_conf)?;
    check_regex_collisions(&lexer_conf, options.strict, None)?;

    // Reuse the in-process scanner recipe so the baked lexer is byte-identical.
    let term_refs: Vec<_> = lexer_conf
        .terminals
        .iter()
        .map(|(id, t)| (*id, t))
        .collect();
    let plan = scanner_plan(&term_refs, lexer_conf.global_flags)?;

    // Run every baked terminal through the standalone refusal seam (issue #280, bounty
    // RC10 + V1/V2). The in-process lexer lowers lookaround into the DFA backend, so a
    // lookaround grammar (e.g. `python.STRING`) builds fine in-process; the standalone
    // runtime compiles each baked group on the *plain* `regex` crate, which cannot host
    // lookaround (RC10), `\Z` (V1), or an oversized bounded repeat (V2). Without this
    // check those patterns are baked verbatim and the generated parser panics at
    // `Regex::new(...).expect("baked scanner regex is valid")`. Reject at bake time with
    // a clear, categorized compile-time error instead of shipping a panicking artifact.
    check_standalone_regex_hostable(&plan, &term_refs, lexer_conf.global_flags)?;

    let symbol_names = (0..table.symbols.len())
        .map(|i| {
            table
                .symbols
                .name(crate::grammar::SymbolId(i as u32))
                .to_string()
        })
        .collect();

    let rules = table
        .rules
        .iter()
        .map(|r| BakedRule {
            origin: r.origin.0,
            len: r.expansion.len() as u32,
            tree_name: r.tree_name.clone(),
            transparent: r.transparent,
            expand1: r.options.expand1,
            has_alias: r.alias.is_some(),
            keep_all: r.options.keep_all_tokens,
            filter_pos: r.filter_pos.clone(),
            placeholder_count: r.options.placeholder_count as u32,
            nones_before: r.options.nones_before.iter().map(|&n| n as u32).collect(),
            is_start: r.is_start,
        })
        .collect();

    // The in-process `ParseTable` is already sparse `(id, …)` rows (#367), the same
    // `&[(u32, Action)]` shape the standalone runtime bakes, so the bake just clones
    // the rows — no dense-matrix sparsification step. The rows are id-ascending (the
    // build flattens them from a `BTreeMap`), the order the bake expects.
    let action = table.action.clone();
    let goto = table.goto.clone();

    let mut start_states: Vec<(String, u32)> = table
        .start_states
        .iter()
        .map(|(id, st)| (table.symbols.name(*id).to_string(), *st as u32))
        .collect();
    start_states.sort();

    let start_default = options
        .start
        .first()
        .cloned()
        .unwrap_or_else(|| "start".to_string());

    let scan_groups = plan
        .groups
        .iter()
        .map(|(id, rx)| (id.0, rx.clone()))
        .collect();

    // Entries keep their definition order — case-insensitive keywords are
    // retyped first-match-wins, so the order is semantic; only the outer list
    // is sorted (by regex-terminal id) for a deterministic bake.
    let mut unless: Vec<(u32, Vec<(String, bool, u32)>)> = plan
        .unless
        .iter()
        .map(|(re_id, entries)| {
            let entries: Vec<(String, bool, u32)> = entries
                .iter()
                .map(|e| (e.value.clone(), e.ci, e.keyword.0))
                .collect();
            (re_id.0, entries)
        })
        .collect();
    unless.sort();

    let mut ignore: Vec<u32> = lexer_conf.ignore.iter().map(|s| s.0).collect();
    ignore.sort_unstable();

    Ok(Baked {
        n_terminals: table.n_terminals as u32,
        symbol_names,
        rules,
        action,
        goto,
        start_states,
        start_default,
        global_prefix: plan.global_prefix.clone(),
        scan_groups,
        unless,
        ignore,
        propagate_positions: options.propagate_positions,
    })
}

/// The shared runtime driver, embedded verbatim into each generated parser.
const RUNTIME_SRC: &str = include_str!("runtime.rs");

/// Generate self-contained Rust source for a standalone parser of `grammar_src`.
///
/// The returned string is a complete `.rs` file: write it next to a crate that
/// depends on `regex` and call the generated `Parser::new().parse(text)`.
///
/// Errors if the grammar fails to load/build, or if the requested configuration
/// is not supported by the standalone backend (non-LALR parser, or a postlex
/// hook — see the module docs).
pub fn generate(grammar_src: &str, options: &LarkOptions) -> Result<String, LarkError> {
    let baked = bake(grammar_src, options)?;

    let mut out = String::new();
    emit_header(&mut out, grammar_src);
    // The shared driver (its leading `//!` module-doc block stripped — the generated
    // file has its own header, and an inner doc comment mid-module would not compile).
    out.push_str(runtime_body());
    out.push('\n');
    emit_data(&mut out, &baked);
    emit_glue(&mut out);
    // Close the wrapping `mod parser` opened by the header and re-export its public
    // surface, so the file works both compiled directly (crate root) and `include!`d
    // into another module.
    out.push_str(
        "}\n\n#[allow(unused_imports)]\npub use parser::{\n    Child, Meta, OutputBuilder, OutputContext, ParseTree, Parser, Token, Tree,\n};\n",
    );
    Ok(out)
}

/// [`RUNTIME_SRC`] with its leading `//!` doc block (and the blank lines around it)
/// removed, so it can be pasted after the generated header and the baked data.
fn runtime_body() -> &'static str {
    let mut offset = 0;
    for line in RUNTIME_SRC.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("//!") || trimmed.is_empty() {
            offset += line.len() + 1; // +1 for the '\n'
        } else {
            break;
        }
    }
    &RUNTIME_SRC[offset..]
}

/// A Rust string literal for `s` (`{:?}` produces valid, fully-escaped source).
fn lit(s: &str) -> String {
    format!("{s:?}")
}

fn emit_header(out: &mut String, grammar_src: &str) {
    out.push_str(
        "// @generated by `lark-rs generate-parser` — DO NOT EDIT.\n\
         //\n\
         // A self-contained LALR parser. Depends only on the `regex` crate and the\n\
         // Rust standard library — not on lark-rs. Drop it into any crate that has\n\
         // `regex` as a dependency and call `Parser::new().parse(text)`.\n\
         //\n\
         // Source grammar:\n",
    );
    for line in grammar_src.lines() {
        // Trim so a blank grammar line emits `//`, not `//   ` — trailing
        // whitespace trips the repo's pre-commit fixers on the committed
        // fixture parsers.
        out.push_str(if line.trim_end().is_empty() {
            "//"
        } else {
            "//   "
        });
        out.push_str(line.trim_end());
        out.push('\n');
    }
    // Everything lives in an inner module carrying an *outer* `#[allow]` — an
    // inner `#![allow]` would be rejected when the file is `include!`d into another
    // module (macro-expanded inner attributes are not permitted there).
    out.push_str("\n#[allow(dead_code, unused_parens, clippy::all)]\npub mod parser {\n");
}

fn emit_data(out: &mut String, baked: &Baked) {
    out.push_str("\n// ── baked grammar tables ──\nstatic DATA: GrammarData = GrammarData {\n");
    let _ = writeln!(out, "    n_terminals: {},", baked.n_terminals);

    // Symbol names, indexed by id.
    out.push_str("    symbol_names: &[\n");
    for name in &baked.symbol_names {
        let _ = writeln!(out, "        {},", lit(name));
    }
    out.push_str("    ],\n");

    // Rules.
    out.push_str("    rules: &[\n");
    for r in &baked.rules {
        let filter: String = r
            .filter_pos
            .iter()
            .map(|b| b.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        let nones: String = r
            .nones_before
            .iter()
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        let _ = writeln!(
            out,
            "        RuleData {{ origin: {}, len: {}, tree_name: {}, transparent: {}, expand1: {}, has_alias: {}, keep_all: {}, filter_pos: &[{}], placeholder_count: {}, nones_before: &[{}], is_start: {} }},",
            r.origin,
            r.len,
            lit(&r.tree_name),
            r.transparent,
            r.expand1,
            r.has_alias,
            r.keep_all,
            filter,
            r.placeholder_count,
            nones,
            r.is_start,
        );
    }
    out.push_str("    ],\n");

    // ACTION table — one sparse row per state, terminals ascending.
    out.push_str("    action: &[\n");
    for row in &baked.action {
        out.push_str("        &[");
        for (i, (term, action)) in row.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            let a = match action {
                Action::Shift(s) => format!("Action::Shift({s})"),
                Action::Reduce(r) => format!("Action::Reduce({r})"),
                Action::Accept => "Action::Accept".to_string(),
            };
            let _ = write!(out, "({term}, {a})");
        }
        out.push_str("],\n");
    }
    out.push_str("    ],\n");

    // GOTO table — sparse (nonterminal index, next state) per state.
    out.push_str("    goto: &[\n");
    for row in &baked.goto {
        out.push_str("        &[");
        for (i, (nt, next)) in row.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            let _ = write!(out, "({nt}, {next})");
        }
        out.push_str("],\n");
    }
    out.push_str("    ],\n");

    // Start states (name → state), already sorted by name in `bake`.
    out.push_str("    start_states: &[");
    for (i, (name, st)) in baked.start_states.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        let _ = write!(out, "({}, {})", lit(name), st);
    }
    out.push_str("],\n");
    let _ = writeln!(out, "    start_default: {},", lit(&baked.start_default));

    // Lexer: global prefix, scanner alternation, unless map, ignore set.
    let _ = writeln!(out, "    global_prefix: {},", lit(&baked.global_prefix));

    out.push_str("    scan_groups: &[\n");
    for (id, rx) in &baked.scan_groups {
        let _ = writeln!(out, "        ({}, {}),", id, lit(rx));
    }
    out.push_str("    ],\n");

    // unless: sorted by regex id in `bake`; entries stay in definition order.
    out.push_str("    unless: &[\n");
    for (re_id, entries) in &baked.unless {
        let _ = write!(out, "        ({re_id}, &[");
        for (i, (v, ci, kw)) in entries.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            let _ = write!(out, "({}, {}, {})", lit(v), ci, kw);
        }
        out.push_str("]),\n");
    }
    out.push_str("    ],\n");

    let ig_list: String = baked
        .ignore
        .iter()
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let _ = writeln!(out, "    ignore: &[{ig_list}],");

    let _ = writeln!(
        out,
        "    propagate_positions: {},",
        baked.propagate_positions
    );

    out.push_str("};\n");
}

/// The per-file glue tying the baked `DATA` to the shared runtime's `Parser`.
fn emit_glue(out: &mut String) {
    out.push_str(
        "\nimpl Parser {\n    /// Construct the parser for this baked grammar.\n    pub fn new() -> Parser {\n        Parser::from_data(&DATA)\n    }\n}\n\nimpl Default for Parser {\n    fn default() -> Self {\n        Parser::new()\n    }\n}\n",
    );
}

#[cfg(test)]
mod tests {
    use super::runtime::{Action, GrammarData, ParseTree, Parser, RuleData};

    // A hand-built `GrammarData` for the trivial grammar `start: "a"`, so the shared
    // runtime is exercised directly as Rust (independently of the code generator and
    // the round-trip fixtures). Layout: terminals $END(0), A(1); non-terminals
    // $root_start(2), start(3). State 0 shifts A→1; state 1 reduces `start: A` (rule
    // 0); after GOTO to the start, $END accepts.
    static TRIVIAL: GrammarData = GrammarData {
        n_terminals: 2,
        symbol_names: &["$END", "A", "$root_start", "start"],
        rules: &[
            RuleData {
                origin: 3,
                len: 1,
                tree_name: "start",
                transparent: false,
                expand1: false,
                has_alias: false,
                keep_all: false,
                filter_pos: &[true], // a literal "a" is filtered out of the tree
                placeholder_count: 0,
                nones_before: &[],
                is_start: false,
            },
            RuleData {
                origin: 2,
                len: 1,
                tree_name: "$root_start",
                transparent: false,
                expand1: false,
                has_alias: false,
                keep_all: false,
                filter_pos: &[false],
                placeholder_count: 0,
                nones_before: &[],
                is_start: true,
            },
        ],
        // state 0: shift A → state 1, then GOTO start → state 2.
        // state 1: reduce rule 0 (`start: A`) on $END.
        // state 2: accept on $END.
        action: &[
            &[(1, Action::Shift(1))],
            &[(0, Action::Reduce(0))],
            &[(0, Action::Accept)],
        ],
        goto: &[&[(1, 2)], &[], &[]],
        start_states: &[("start", 0)],
        start_default: "start",
        global_prefix: "",
        scan_groups: &[(1, "a")],
        unless: &[],
        ignore: &[],
        propagate_positions: false,
    };

    #[test]
    fn runtime_parses_with_hand_built_data() {
        let parser = Parser::from_data(&TRIVIAL);
        let tree = parser.parse("a").expect("parses");
        // The "a" literal is filtered, leaving an empty `start` node.
        assert!(matches!(&tree, ParseTree::Tree(t) if t.data == "start" && t.children.is_empty()));
        assert_eq!(tree.to_string(), "Tree(start, [])");
    }

    #[test]
    fn runtime_reports_errors() {
        let parser = Parser::from_data(&TRIVIAL);
        assert!(parser.parse("b").is_err(), "unexpected character");
        assert!(parser.parse("aa").is_err(), "trailing input");
        assert!(parser.parse("").is_err(), "empty input");
    }

    /// #151 pin for the runtime's own `Tree`: deep enough that the derived
    /// `Drop`/`Clone` glue would overflow the default 8 MB test stack, so a
    /// crash here means the manual worklist impls were lost.
    #[test]
    fn runtime_tree_drop_and_clone_are_iterative() {
        use super::runtime::{Child, Meta, Tree};
        let mut t = Tree {
            data: "leaf".to_string(),
            children: vec![],
            meta: Meta::default(),
        };
        for _ in 0..200_000 {
            t = Tree {
                data: "nest".to_string(),
                children: vec![Child::Tree(t)],
                meta: Meta::default(),
            };
        }
        let copy = t.clone();
        drop(t);
        drop(copy);
    }

    /// #529: a default standalone `Meta` is `empty: true` with no position fields —
    /// a position-less default has no span, the semantic convention Python Lark
    /// follows (`Meta.empty` defaults `True`). The derived `Default` would (wrongly)
    /// give `empty: false`.
    #[test]
    fn runtime_meta_default_is_empty_with_no_position() {
        let m = super::runtime::Meta::default();
        assert!(
            m.empty,
            "a default Meta must be empty (no positioned child)"
        );
        assert_eq!(m.line, None);
        assert_eq!(m.column, None);
        assert_eq!(m.end_line, None);
        assert_eq!(m.end_column, None);
        assert_eq!(m.start_pos, None);
        assert_eq!(m.end_pos, None);
    }

    // ─── Standalone compliance bank (#86) ─────────────────────────────────────
    //
    // `runtime.rs` is a parallel re-expression of the in-process LALR reduce /
    // tree-shaping driver. To widen the drift net beyond the two round-trip
    // fixtures (json, arithmetic), replay the strip-mined Python-Lark compliance
    // bank through the standalone runtime and compare to the *same captured oracle
    // trees* `test_compliance.rs` uses — the same XFAIL-burndown discipline as the
    // LALR (`xfail.json`) and Earley (`earley_xfail.json`) banks.
    //
    // The runtime is exercised over the bank's grammars by baking each (the shared
    // `bake`, the very data a generated parser would ship) and leaking it to
    // `'static` so `runtime::Parser` can run it — no per-grammar codegen/compile.
    // Standalone is LALR + basic-lexer only, so contextual-lexer / strict-collision
    // grammars naturally diverge and are allow-listed, to be burned down under #86.

    use super::{bake, Baked, LarkOptions, ParserAlgorithm};
    use serde_json::Value;
    use std::collections::BTreeSet;
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use std::path::PathBuf;

    fn compliance_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/oracles/compliance")
    }

    fn load_json(name: &str) -> Option<Value> {
        let text = std::fs::read_to_string(compliance_dir().join(name)).ok()?;
        Some(serde_json::from_str(&text).expect("valid JSON"))
    }

    fn string_set(name: &str) -> BTreeSet<String> {
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
        use crate::grammar::terminal::flags;
        let start = match &rec["start"] {
            Value::String(s) => vec![s.clone()],
            Value::Array(a) => a
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect(),
            _ => vec!["start".to_string()],
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
            maybe_placeholders: rec["maybe_placeholders"].as_bool().unwrap_or(true),
            keep_all_tokens: rec["keep_all_tokens"].as_bool().unwrap_or(false),
            strict: rec["strict"].as_bool().unwrap_or(false),
            g_regex_flags,
            ..Default::default()
        }
    }

    /// Leak a [`Baked`] to `'static` so the runtime (which takes `&'static
    /// GrammarData`, matching a generated parser's baked `static`) can run it.
    /// Test-only: the leak is bounded by the bank size and freed at process exit.
    fn leak_grammar_data(b: &Baked) -> &'static GrammarData {
        fn leak_str(s: &str) -> &'static str {
            Box::leak(s.to_string().into_boxed_str())
        }

        let symbol_names: Vec<&'static str> = b.symbol_names.iter().map(|s| leak_str(s)).collect();
        let rules: Vec<RuleData> = b
            .rules
            .iter()
            .map(|r| RuleData {
                origin: r.origin,
                len: r.len,
                tree_name: leak_str(&r.tree_name),
                transparent: r.transparent,
                expand1: r.expand1,
                has_alias: r.has_alias,
                keep_all: r.keep_all,
                filter_pos: &*Box::leak(r.filter_pos.clone().into_boxed_slice()),
                placeholder_count: r.placeholder_count,
                nones_before: &*Box::leak(r.nones_before.clone().into_boxed_slice()),
                is_start: r.is_start,
            })
            .collect();
        let action: Vec<&'static [(u32, Action)]> = b
            .action
            .iter()
            .map(|row| {
                let row: Vec<(u32, Action)> = row
                    .iter()
                    .map(|(t, a)| {
                        let a = match a {
                            super::Action::Shift(s) => Action::Shift(*s as u32),
                            super::Action::Reduce(r) => Action::Reduce(*r as u32),
                            super::Action::Accept => Action::Accept,
                        };
                        (*t, a)
                    })
                    .collect();
                &*Box::leak(row.into_boxed_slice())
            })
            .collect();
        let goto: Vec<&'static [(u32, u32)]> = b
            .goto
            .iter()
            .map(|row| &*Box::leak(row.clone().into_boxed_slice()))
            .collect();
        let start_states: Vec<(&'static str, u32)> = b
            .start_states
            .iter()
            .map(|(n, s)| (leak_str(n), *s))
            .collect();
        let scan_groups: Vec<(u32, &'static str)> = b
            .scan_groups
            .iter()
            .map(|(id, rx)| (*id, leak_str(rx)))
            .collect();
        let unless: Vec<(u32, &'static [(&'static str, bool, u32)])> = b
            .unless
            .iter()
            .map(|(id, entries)| {
                let entries: Vec<(&'static str, bool, u32)> = entries
                    .iter()
                    .map(|(v, ci, kw)| (leak_str(v), *ci, *kw))
                    .collect();
                (*id, &*Box::leak(entries.into_boxed_slice()))
            })
            .collect();

        Box::leak(Box::new(GrammarData {
            n_terminals: b.n_terminals,
            symbol_names: Box::leak(symbol_names.into_boxed_slice()),
            rules: Box::leak(rules.into_boxed_slice()),
            action: Box::leak(action.into_boxed_slice()),
            goto: Box::leak(goto.into_boxed_slice()),
            start_states: Box::leak(start_states.into_boxed_slice()),
            start_default: leak_str(&b.start_default),
            global_prefix: leak_str(&b.global_prefix),
            scan_groups: Box::leak(scan_groups.into_boxed_slice()),
            unless: Box::leak(unless.into_boxed_slice()),
            ignore: Box::leak(b.ignore.clone().into_boxed_slice()),
            propagate_positions: b.propagate_positions,
        }))
    }

    // ── runtime-tree vs oracle-JSON comparison (mirrors common::tree_matches_oracle,
    //    but over the runtime's own tree types). No `_ambig` — standalone is LALR.
    fn rt_matches(result: &ParseTree, oracle: &Value) -> bool {
        match result {
            ParseTree::Tree(t) => oracle["type"].as_str() == Some("tree") && rt_tree(t, oracle),
            ParseTree::Token(tok) => {
                oracle["type"].as_str() == Some("token") && rt_token(tok, oracle)
            }
            // A bare-`None` root (`?start: [A]` lone-placeholder collapse, #289/#382):
            // mirrors the `Child::None` oracle shape (Python's `None` result).
            ParseTree::None => oracle["type"].as_str() == Some("unknown"),
        }
    }
    fn rt_tree(t: &super::runtime::Tree, oracle: &Value) -> bool {
        if t.data != oracle["data"].as_str().unwrap_or("?") {
            return false;
        }
        let children = oracle["children"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        t.children.len() == children.len()
            && t.children
                .iter()
                .zip(children)
                .all(|(c, oc)| rt_child(c, oc))
    }
    fn rt_child(c: &super::runtime::Child, oracle: &Value) -> bool {
        match c {
            super::runtime::Child::Tree(st) => {
                oracle["type"].as_str() == Some("tree") && rt_tree(st, oracle)
            }
            super::runtime::Child::Token(tok) => {
                oracle["type"].as_str() == Some("token") && rt_token(tok, oracle)
            }
            super::runtime::Child::None => oracle["type"].as_str() == Some("unknown"),
        }
    }
    fn rt_token(tok: &super::runtime::Token, oracle: &Value) -> bool {
        tok.type_ == oracle["token_type"].as_str().unwrap_or("?")
            && tok.value == oracle["value"].as_str().unwrap_or("?")
    }

    /// Build a standalone parser for `grammar` and parse `input`, catching panics
    /// (a handful of bank grammars abort deep in table/regex construction).
    fn try_standalone(grammar: &str, opts: &LarkOptions, input: &str) -> Option<ParseTree> {
        catch_unwind(AssertUnwindSafe(|| {
            let baked = bake(grammar, opts).ok()?;
            let data = leak_grammar_data(&baked);
            Parser::from_data(data).parse(input).ok()
        }))
        .ok()
        .flatten()
    }

    fn can_bake(grammar: &str, opts: &LarkOptions) -> bool {
        catch_unwind(AssertUnwindSafe(|| bake(grammar, opts).is_ok())).unwrap_or(false)
    }

    // ─── #298: standalone runs the in-process config-legality gate ─────────────
    //
    // The standalone front door (`bake`) is LALR + basic-lexer only, but a caller
    // can still *set* an illegal `lexer`/`ambiguity` on the LALR-only path. The
    // in-process API rejects those via `validate_config` (#273 / N5/N6); standalone
    // used to silently accept them — a more-permissive, unfalsifiable asymmetry
    // (ADR-0017). Oracle here is the EXISTING in-process gate: the same illegal
    // config must be rejected at standalone bake the same way `build_frontend`
    // rejects it. Negative control: a LEGAL config must still bake (no over-reject).

    /// A trivial, fully bakeable LALR grammar for the gate tests.
    fn gate_grammar() -> &'static str {
        "start: \"a\"\n"
    }

    /// In-process oracle: does the in-process front door accept this config for
    /// `gate_grammar`? (We compare standalone's bake verdict to this.)
    fn in_process_accepts(opts: &LarkOptions) -> bool {
        crate::Lark::new(gate_grammar(), opts.clone()).is_ok()
    }

    #[test]
    fn standalone_rejects_illegal_lexer_like_in_process() {
        // `{parser: lalr, lexer: dynamic}` — the dynamic lexer is not in lalr's
        // allowed set, so the in-process gate rejects it. Standalone must too.
        let opts = LarkOptions {
            parser: ParserAlgorithm::Lalr,
            lexer: crate::LexerType::Dynamic,
            start: vec!["start".to_string()],
            ..Default::default()
        };
        assert!(
            !in_process_accepts(&opts),
            "oracle precondition: in-process API must reject lalr+dynamic"
        );
        assert!(
            bake(gate_grammar(), &opts).is_err(),
            "#298: standalone bake must reject lalr+dynamic to match the in-process gate"
        );
    }

    #[test]
    fn standalone_rejects_explicit_ambiguity_like_in_process() {
        // `ambiguity: Explicit` on lalr — Python (and our in-process gate) rejects
        // disambiguation on lalr. Standalone must too.
        let opts = LarkOptions {
            parser: ParserAlgorithm::Lalr,
            ambiguity: crate::Ambiguity::Explicit,
            start: vec!["start".to_string()],
            ..Default::default()
        };
        assert!(
            !in_process_accepts(&opts),
            "oracle precondition: in-process API must reject lalr+ambiguity=explicit"
        );
        assert!(
            bake(gate_grammar(), &opts).is_err(),
            "#298: standalone bake must reject lalr+ambiguity=explicit to match the in-process gate"
        );
    }

    #[test]
    fn standalone_accepts_legal_config_negative_control() {
        // Negative control: a LEGAL standalone config (lalr + contextual lexer,
        // resolve ambiguity) must still bake — the gate must not over-reject.
        let opts = LarkOptions {
            parser: ParserAlgorithm::Lalr,
            lexer: crate::LexerType::Contextual,
            start: vec!["start".to_string()],
            ..Default::default()
        };
        assert!(
            in_process_accepts(&opts),
            "oracle precondition: in-process API accepts lalr+contextual"
        );
        assert!(
            bake(gate_grammar(), &opts).is_ok(),
            "#298: a legal standalone config must still bake (no over-reject)"
        );
    }

    /// Regression pin (bounty H12, fixed): the baked standalone runtime collapses an
    /// `expand1` (`?rule`) down to a lone placeholder-`None` child the way the
    /// in-process tree builder does (the RC9 carve-out in
    /// `parsers/tree_builder.rs`). `runtime::shape`'s expand1 arm previously carried
    /// an extra `&& !matches!(children[0], Child::None)` guard, so a lone `None` was
    /// wrapped as `Tree(w, [None])` instead of being spliced as a bare `None`; core
    /// lark-rs AND Python Lark both yield `Tree(start, [None])`, so the standalone
    /// bake diverged from both — falsifying the "byte-faithful to core by
    /// construction" standalone contract. The runtime now mirrors the RC9 lone-`None`
    /// carve-out, so this case agrees with core/Python.
    #[test]
    fn standalone_expand1_lone_none_collapses_like_core() {
        let opts = LarkOptions {
            parser: ParserAlgorithm::Lalr,
            start: vec!["start".to_string()],
            maybe_placeholders: true,
            ..Default::default()
        };
        let tree = try_standalone("start: w \"x\"\n?w: [A]\nA: \"a\"\n", &opts, "x")
            .expect("standalone parses \"x\"");
        // Python/core: Tree(start, [None]) — the lone-None `?w` collapses to a
        // bare None spliced into the parent.
        let ParseTree::Tree(t) = &tree else {
            panic!("H12: expected a tree, got {tree:?}");
        };
        assert_eq!(t.data, "start");
        assert_eq!(
            t.children.len(),
            1,
            "H12: start should have exactly one child (the collapsed None)"
        );
        assert!(
            matches!(t.children[0], super::runtime::Child::None),
            "H12: expand1 must collapse the lone placeholder to a bare None; \
             standalone wrapped it as Tree(w, [None]) instead"
        );
    }

    /// XFAIL (bounty V-H7-1, round h7): the standalone runtime's private `ParseTree`
    /// enum (`runtime.rs`) has only `Tree`/`Token` — it never grew the `None` variant
    /// ADR-0033/#382 added to the public API + the in-process backends + the bindings
    /// (its consumer list omits `src/standalone/runtime.rs`). So a `?start` rule that
    /// collapses to a lone placeholder-`None` (`?start: [A]` on empty input,
    /// `maybe_placeholders=true`) reaches `run()`'s `Action::Accept` arm as an
    /// `Inline([None])` and falls into its `_ => Err("accept with empty value stack")`
    /// fallback. Python Lark (basic lexer **and** its own `standalone` tool) and every
    /// in-process lark-rs backend return a bare `None` here — so the bake errors on
    /// input the oracle accepts, falsifying the "byte-faithful to core" standalone
    /// contract. This is the #289/#382 lone-`None` root cause surfacing on the unfixed
    /// standalone surface (the H6 "standalone clean" verdict missed the None-root path,
    /// whose H12 pin only exercised a None-as-*child*). Today this test fails because
    /// the parse returns `Err`; the fix adds a `ParseTree::None` variant to the runtime
    /// and an `Inline([None]) => Ok(None)` Accept arm, after which the parse returns
    /// `Ok(ParseTree::None)`. Drop the `#[ignore]` then to make it a regression guard.
    #[test]
    fn standalone_none_root_returns_none_like_core() {
        let opts = LarkOptions {
            parser: ParserAlgorithm::Lalr,
            start: vec!["start".to_string()],
            maybe_placeholders: true,
            ..Default::default()
        };
        let baked = bake("?start: [A]\nA: \"a\"\n", &opts).expect("V-H7-1: bakes");
        let data = leak_grammar_data(&baked);
        let result = catch_unwind(AssertUnwindSafe(|| Parser::from_data(data).parse("")));
        // Python (basic) and in-process lark-rs both return a bare None for the
        // lone-placeholder `?start:[A]` on empty input. The standalone runtime cannot
        // represent a None root, so today the parse returns Err; once the runtime grows
        // a ParseTree::None variant the parse returns Ok(ParseTree::None). (We assert
        // Ok rather than match the variant because the variant does not yet exist to
        // name in this test.)
        match result {
            Ok(Ok(_tree)) => { /* fixed: returns Ok (the bare-None root) */ }
            Ok(Err(e)) => panic!(
                "V-H7-1: standalone errored on a None-root parse Python/core accept as \
                 None (expected Ok(ParseTree::None)): {e}"
            ),
            Err(_) => panic!("V-H7-1: standalone panicked on a None-root parse"),
        }
    }

    // ─── #457: propagate_positions meta parity with the in-process LALR engine ──
    //
    // The oracle is the in-process **basic-lexer** LALR parser with
    // `propagate_positions=true` (the standalone runtime *is* the basic lexer, so
    // comparing against the contextual lexer would conflate the two). The standalone
    // runtime's `Tree.meta` must be byte-identical to it — the #402 semantics (a
    // node's `meta` spans its rule's *pre-filter* children, so rule-filtered
    // punctuation — e.g. a `"("`/`")"` terminal shifted onto the value stream and
    // dropped by token-filtering — still bounds a container) — and `Token` spans
    // must be **character** indices (#278), not byte offsets, on non-ASCII input.
    // Note: `%ignore` tokens are *not* shifted onto the parser's value stream, so
    // ignored whitespace generally does not bound a node's meta; only the inter-token
    // span absorbed into a filtered terminal's own start/end participates.

    /// Recursively assert the standalone runtime tree's `meta` equals the oracle's.
    fn assert_meta_eq(oracle: &crate::tree::Tree, mine: &super::runtime::Tree, input: &str) {
        assert_eq!(oracle.data, mine.data, "{input:?}: node name");
        let (o, m) = (&oracle.meta, &mine.meta);
        assert_eq!(
            o.line(),
            m.line.map(|v| v as usize),
            "{input:?} {}: line",
            oracle.data
        );
        assert_eq!(
            o.column(),
            m.column.map(|v| v as usize),
            "{input:?} {}: column",
            oracle.data
        );
        assert_eq!(
            o.end_line(),
            m.end_line.map(|v| v as usize),
            "{input:?} {}: end_line",
            oracle.data
        );
        assert_eq!(
            o.end_column(),
            m.end_column.map(|v| v as usize),
            "{input:?} {}: end_column",
            oracle.data
        );
        assert_eq!(
            o.start_pos(),
            m.start_pos.map(|v| v as usize),
            "{input:?} {}: start_pos",
            oracle.data
        );
        assert_eq!(
            o.end_pos(),
            m.end_pos.map(|v| v as usize),
            "{input:?} {}: end_pos",
            oracle.data
        );
        assert_eq!(o.empty, m.empty, "{input:?} {}: empty", oracle.data);
        assert_eq!(
            oracle.children.len(),
            mine.children.len(),
            "{input:?} {}: child count",
            oracle.data
        );
        for (oc, mc) in oracle.children.iter().zip(&mine.children) {
            match (oc, mc) {
                (crate::tree::Child::Tree(ot), super::runtime::Child::Tree(mt)) => {
                    assert_meta_eq(ot, mt, input)
                }
                (crate::tree::Child::Token(ot), super::runtime::Child::Token(mt)) => {
                    // Token spans must agree too (the meta widening reads them).
                    assert_eq!(ot.value, mt.value, "{input:?}: token value");
                    assert_eq!(
                        ot.line(),
                        mt.line as usize,
                        "{input:?} {}: token line",
                        ot.value
                    );
                    assert_eq!(
                        ot.column(),
                        mt.column as usize,
                        "{input:?} {}: token column",
                        ot.value
                    );
                    assert_eq!(
                        ot.end_line(),
                        mt.end_line as usize,
                        "{input:?} {}: token end_line",
                        ot.value
                    );
                    assert_eq!(
                        ot.end_column(),
                        mt.end_column as usize,
                        "{input:?} {}: token end_column",
                        ot.value
                    );
                    assert_eq!(
                        ot.start_pos(),
                        mt.start_pos as usize,
                        "{input:?} {}: token start_pos",
                        ot.value
                    );
                    assert_eq!(
                        ot.end_pos(),
                        mt.end_pos as usize,
                        "{input:?} {}: token end_pos",
                        ot.value
                    );
                }
                (crate::tree::Child::None, super::runtime::Child::None) => {}
                (oc, mc) => panic!("{input:?}: child shape mismatch {oc:?} vs {mc:?}"),
            }
        }
    }

    #[test]
    fn standalone_meta_matches_in_process_lalr() {
        // (grammar, inputs). Each grammar's container nodes span its rule-filtered
        // punctuation (the `"("`/`")"`/`"["`/`","`/`"]"` terminals dropped by
        // token-filtering after being shifted), so the #402 pre-filter widening is
        // load-bearing. The grammars also carry `%ignore` whitespace, but those
        // tokens never reach the value stream and so do not bound a node's meta; they
        // only exercise that the widening still matches the oracle in their presence.
        // The last case is non-ASCII to pin char-index (#278) `start_pos`/`end_pos`.
        let cases: &[(&str, &[&str])] = &[
            (
                "start: \"(\" NUMBER \")\"\nNUMBER: /[0-9]+/\n%ignore \" \"\n",
                &["(42)", "( 42 )", "(7)"],
            ),
            (
                "start: pair+\npair: \"[\" NUMBER \",\" NUMBER \"]\"\n\
                 NUMBER: /[0-9]+/\n%ignore /\\s+/\n",
                &["[1,2]", "[1, 2] [3, 4]", "[1,\n2]"],
            ),
            (
                "start: \"\u{ab}\" WORD \"\u{bb}\"\nWORD: /\\w+/\n",
                &["\u{ab}h\u{e9}llo\u{bb}"],
            ),
        ];

        for (grammar, inputs) in cases {
            let opts = LarkOptions {
                parser: ParserAlgorithm::Lalr,
                lexer: crate::LexerType::Basic,
                start: vec!["start".to_string()],
                propagate_positions: true,
                ..Default::default()
            };

            // Oracle: in-process basic-lexer LALR with propagate_positions.
            let oracle = crate::Lark::new(grammar, opts.clone()).expect("oracle builds");

            // Standalone: bake (now accepts propagate_positions — #457), leak, run.
            let baked = bake(grammar, &opts).expect("standalone bakes propagate_positions");
            assert!(
                baked.propagate_positions,
                "#457: bake must thread propagate_positions into the baked data"
            );
            let data = leak_grammar_data(&baked);
            let parser = Parser::from_data(data);

            for input in *inputs {
                let oracle_tree = match oracle.parse(input) {
                    Ok(crate::ParseTree::Tree(t)) => t,
                    other => panic!("{input:?}: oracle did not return a Tree: {other:?}"),
                };
                let mine = match parser.parse(input) {
                    Ok(ParseTree::Tree(t)) => t,
                    other => panic!("{input:?}: standalone did not return a Tree: {other:?}"),
                };
                assert_meta_eq(&oracle_tree, &mine, input);
            }
        }
    }

    /// Replays the full strip-mined Python-Lark bank through the shared
    /// standalone `runtime` (#86), under the same XFAIL discipline as the other
    /// banks. The `standalone_xfail.json` entries are **basic-lexer-incompatible**
    /// grammars: their oracles were captured under the contextual lexer, and
    /// Python's own *basic* lexer rejects/mis-types the same inputs the
    /// basic-only standalone runtime does (verified directly — e.g. bank 105,
    /// `!start: "a"i "a"`, where Python-basic errors on all four inputs). Bank
    /// 105's `parse:105:1` joined the list when `"a"i` was reclassified
    /// `PatternRe` → `PatternStr`-with-`i`: the old representation routed `"a"`
    /// through an `unless` embed+retype Python never performs, which happened to
    /// produce the contextual oracle's answer on that one input. Losing the
    /// accidental pass is the cost of agreeing with Python's basic lexer.
    #[test]
    fn standalone_compliance_bank() {
        let Some(records) = load_json("bank.json") else {
            eprintln!("compliance bank.json not found — run tools/extract_lark_compliance.py");
            return;
        };
        let records = records.as_array().expect("bank is an array");
        let skip = string_set("skip.json");

        std::panic::set_hook(Box::new(|_| {}));

        let mut failures: BTreeSet<String> = BTreeSet::new();
        let mut total = 0usize;

        for (ri, rec) in records.iter().enumerate() {
            let grammar = rec["grammar"].as_str().unwrap_or("");
            if skip.contains(grammar) {
                continue;
            }
            let opts = record_options(rec);

            // construct-error parity: Python raised at construction; the standalone
            // backend must refuse to bake too.
            if rec["construct_error"].as_bool().unwrap_or(false) {
                total += 1;
                if can_bake(grammar, &opts) {
                    failures.insert(format!("construct:{ri}"));
                }
                continue;
            }

            let cases = rec["cases"].as_array().map(Vec::as_slice).unwrap_or(&[]);
            if cases.is_empty() {
                continue;
            }
            let buildable = can_bake(grammar, &opts);

            for (ci, case) in cases.iter().enumerate() {
                total += 1;
                let input = case["input"].as_str().unwrap_or("");
                let should_parse = case["should_parse"].as_bool().unwrap_or(false);
                let parsed = if buildable {
                    try_standalone(grammar, &opts, input)
                } else {
                    None
                };
                let agree = match (should_parse, &parsed) {
                    (true, Some(tree)) => rt_matches(tree, &case["tree"]),
                    (true, None) => false,
                    (false, None) => true,
                    (false, Some(_)) => false,
                };
                if !agree {
                    failures.insert(format!("parse:{ri}:{ci}"));
                }
            }
        }

        let xfail = string_set("standalone_xfail.json");
        let passing = total - failures.len();
        let pct = if total == 0 {
            100.0
        } else {
            100.0 * passing as f64 / total as f64
        };
        eprintln!(
            "standalone compliance: {passing}/{total} agree with oracle ({pct:.1}%); \
             {} known-XFAIL",
            xfail.len()
        );

        if std::env::var("LARK_STANDALONE_WRITE_XFAIL").is_ok() {
            let list: Vec<&String> = failures.iter().collect();
            let path = compliance_dir().join("standalone_xfail.json");
            std::fs::write(&path, serde_json::to_string_pretty(&list).unwrap() + "\n")
                .expect("write standalone_xfail.json");
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
                "note: {} standalone XFAIL entries now pass — consider regenerating \
                 standalone_xfail.json",
                fixed.len()
            );
        }
        assert!(
            regressions.is_empty(),
            "standalone compliance regressions ({} newly failing, not in standalone_xfail.json):\n{}",
            regressions.len(),
            regressions
                .iter()
                .take(40)
                .map(|s| format!("  - {s}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }

    // ─── #234 (C9): semantic output through the standalone runtime seam ─────────
    //
    // The standalone runtime grew a value-parametric `OutputBuilder` seam
    // (`runtime::parse_into`, the mirror of `Lark::parse_into` / the in-process
    // `shape_reduction`), so a generated standalone parser can drive a semantic
    // backend that never materializes the runtime's own tree — the C9 done-when.
    //
    // Oracle: the **Python transformer** value + trace, captured by
    // `tools/generate_transformer_oracles.py` into
    // `tests/fixtures/oracles/transformer/cases.json` (C1). We compare against each
    // config's **post-parse** `value`/`trace` (not the `embedded` block), exactly as
    // the in-process `parse_into` gate does (`test_transformer_oracle.rs`'s
    // `check_case_config_embedded` — C7b): the standalone `parse_into` shaping is the
    // same-engine mirror of `Lark::parse_into`, which applies `Discard` uniformly
    // (dropping it from *every* callback's result — the engine sweep) and fires
    // `__default_token__`, matching Python's post-parse `.transform(tree)`. Python's
    // *embedded* path leaves a rule-callback `Discard` sentinel in place and skips
    // `__default_token__` — a Python-lark quirk our engine deliberately does not
    // reproduce (ADR-0038 §2; the same choice the in-process seam made). Standalone
    // is LALR + basic-lexer only, so we run the `lalr_basic` config and skip any
    // fixture whose grammar the basic lexer cannot bake (e.g. `unless_keyword_retype`,
    // which needs the contextual lexer); the survivors are the same action space
    // C1/C3 exercises in-process.
    //
    // This is a *differential* against the same Python oracle the in-process C3/C7
    // gate uses — not a hand-written expectation — so a divergence in the baked
    // shaping (filtering / expand1 / transparent / placeholders / Discard) fails
    // loudly. The transformer backend below is baked-runtime-only (it implements the
    // runtime's `OutputBuilder`, not the crate's), so it exercises the actual seam a
    // generated parser ships.

    use super::runtime::{Meta as RtMeta, OutputBuilder, OutputContext, Token as RtToken};

    /// A transformed semantic value, mirroring `_serialize_value` in the generator
    /// (and the crate-side `TransformValue::to_json`) so a runtime value's JSON is
    /// directly comparable to the fixture's Python value JSON.
    #[derive(Clone, Debug)]
    enum TVal {
        Null,
        Bool(bool),
        Int(i64),
        Float(f64),
        Str(String),
        Tok { token_type: String, value: String },
        List(Vec<TVal>),
        Dict(Vec<(String, TVal)>),
        Tree { data: String, children: Vec<TVal> },
    }

    impl TVal {
        fn to_json(&self) -> Value {
            match self {
                TVal::Null => Value::Null,
                TVal::Bool(b) => Value::Bool(*b),
                TVal::Int(i) => Value::from(*i),
                TVal::Float(f) => Value::from(*f),
                TVal::Str(s) => Value::from(s.clone()),
                TVal::Tok { token_type, value } => serde_json::json!({
                    "type": "token", "token_type": token_type, "value": value,
                }),
                TVal::List(items) => Value::Array(items.iter().map(TVal::to_json).collect()),
                TVal::Dict(entries) => {
                    let mut map = serde_json::Map::new();
                    for (k, v) in entries {
                        map.insert(k.clone(), v.to_json());
                    }
                    Value::Object(map)
                }
                TVal::Tree { data, children } => serde_json::json!({
                    "type": "tree", "data": data,
                    "children": children.iter().map(TVal::to_json).collect::<Vec<_>>(),
                }),
            }
        }

        /// Python's `str(value)` for the values the str-joining specs apply it to.
        fn py_str(&self) -> String {
            match self {
                TVal::Null => "None".to_string(),
                TVal::Bool(true) => "True".to_string(),
                TVal::Bool(false) => "False".to_string(),
                TVal::Int(i) => i.to_string(),
                TVal::Float(f) => py_float_str(*f),
                TVal::Str(s) => s.clone(),
                TVal::Tok { value, .. } => value.clone(),
                other => format!("{other:?}"),
            }
        }
    }

    fn py_float_str(f: f64) -> String {
        if f.is_finite() && f == f.trunc() && f.abs() < 1e16 {
            format!("{f:.1}")
        } else {
            format!("{f}")
        }
    }

    #[derive(Clone, Debug)]
    enum Maybe {
        Keep(TVal),
        Discard,
    }

    #[derive(Clone, Debug)]
    struct SpecAction {
        kind: String,
        value: Option<Value>,
        sep: Option<String>,
        key: Option<String>,
    }

    impl SpecAction {
        fn from_json(spec: &Value) -> SpecAction {
            let obj = spec.as_object().expect("action spec is an object");
            SpecAction {
                kind: obj["action"].as_str().expect("action.kind").to_string(),
                value: obj.get("value").cloned(),
                sep: obj.get("sep").and_then(|v| v.as_str()).map(str::to_string),
                key: obj.get("key").and_then(|v| v.as_str()).map(str::to_string),
            }
        }
    }

    fn literal(v: &Value) -> TVal {
        match v {
            Value::Null => TVal::Null,
            Value::Bool(b) => TVal::Bool(*b),
            Value::Number(n) => n
                .as_i64()
                .map(TVal::Int)
                .unwrap_or_else(|| TVal::Float(n.as_f64().unwrap())),
            Value::String(s) => TVal::Str(s.clone()),
            Value::Array(a) => TVal::List(a.iter().map(literal).collect()),
            Value::Object(o) => {
                TVal::Dict(o.iter().map(|(k, v)| (k.clone(), literal(v))).collect())
            }
        }
    }

    #[derive(Clone, Debug, PartialEq)]
    struct TraceEntry {
        kind: String,
        name: String,
    }

    fn apply_rule_action(action: &SpecAction, data: &str, children: Vec<TVal>) -> Maybe {
        match action.kind.as_str() {
            "identity" => Maybe::Keep(TVal::Tree {
                data: data.to_string(),
                children,
            }),
            "return_value" => Maybe::Keep(literal(action.value.as_ref().expect("value"))),
            "sum_children" => Maybe::Keep(reduce_numeric(&children, Num::Int(0), Num::add)),
            "product_children" => Maybe::Keep(reduce_numeric(&children, Num::Int(1), Num::mul)),
            "join_children" => {
                let sep = action.sep.as_deref().unwrap_or("");
                Maybe::Keep(TVal::Str(
                    children
                        .iter()
                        .map(TVal::py_str)
                        .collect::<Vec<_>>()
                        .join(sep),
                ))
            }
            "concat_children" => Maybe::Keep(TVal::Str(
                children
                    .iter()
                    .map(TVal::py_str)
                    .collect::<Vec<_>>()
                    .concat(),
            )),
            "first_child" => Maybe::Keep(children.into_iter().next().unwrap_or(TVal::Null)),
            "wrap_list" => Maybe::Keep(TVal::List(children)),
            "wrap_dict" => Maybe::Keep(TVal::Dict(vec![(
                action.key.clone().expect("key"),
                TVal::List(children),
            )])),
            "discard" => Maybe::Discard,
            other => panic!("unknown rule action: {other}"),
        }
    }

    fn apply_token_action(action: &SpecAction, ty: &str, text: &str) -> Maybe {
        match action.kind.as_str() {
            "identity" => Maybe::Keep(TVal::Tok {
                token_type: ty.to_string(),
                value: text.to_string(),
            }),
            "int_value" => Maybe::Keep(TVal::Int(text.parse().expect("int"))),
            "float_value" => Maybe::Keep(TVal::Float(text.parse().expect("float"))),
            "upper" => Maybe::Keep(TVal::Str(text.to_uppercase())),
            "lower" => Maybe::Keep(TVal::Str(text.to_lowercase())),
            "prefix" => {
                let p = action
                    .value
                    .as_ref()
                    .and_then(|v| v.as_str())
                    .expect("prefix value");
                Maybe::Keep(TVal::Str(format!("{p}{text}")))
            }
            "stringify" => Maybe::Keep(TVal::Str(text.to_string())),
            "return_value" => Maybe::Keep(literal(action.value.as_ref().expect("value"))),
            "discard" => Maybe::Discard,
            other => panic!("unknown token action: {other}"),
        }
    }

    #[derive(Clone, Copy)]
    enum Num {
        Int(i64),
        Float(f64),
    }
    impl Num {
        fn add(self, o: Num) -> Num {
            match (self, o) {
                (Num::Int(a), Num::Int(b)) => Num::Int(a + b),
                (a, b) => Num::Float(a.f() + b.f()),
            }
        }
        fn mul(self, o: Num) -> Num {
            match (self, o) {
                (Num::Int(a), Num::Int(b)) => Num::Int(a * b),
                (a, b) => Num::Float(a.f() * b.f()),
            }
        }
        fn f(self) -> f64 {
            match self {
                Num::Int(i) => i as f64,
                Num::Float(f) => f,
            }
        }
        fn into_val(self) -> TVal {
            match self {
                Num::Int(i) => TVal::Int(i),
                Num::Float(f) => TVal::Float(f),
            }
        }
    }
    fn reduce_numeric(children: &[TVal], init: Num, op: impl Fn(Num, Num) -> Num) -> TVal {
        let mut acc = init;
        for c in children {
            acc = op(
                acc,
                match c {
                    TVal::Int(i) => Num::Int(*i),
                    TVal::Float(f) => Num::Float(*f),
                    other => panic!("expected numeric child, got {other:?}"),
                },
            );
        }
        acc.into_val()
    }

    /// A Python-`Transformer`-shaped semantic backend, driven **during the parse**
    /// through the standalone runtime's `OutputBuilder` seam. Keyed by callback name
    /// (rule alias/origin — the `tree_name` the engine resolves) and terminal name.
    struct RtTransformBuilder {
        rule_actions: std::collections::HashMap<String, SpecAction>,
        token_actions: std::collections::HashMap<String, SpecAction>,
        default_rule: Option<SpecAction>,
        default_token: Option<SpecAction>,
        visit_tokens: bool,
        trace: Vec<TraceEntry>,
    }

    impl RtTransformBuilder {
        fn from_case(case: &Value) -> RtTransformBuilder {
            let parse_map = |field: &str| {
                case[field]
                    .as_object()
                    .expect("action map")
                    .iter()
                    .map(|(k, v)| (k.clone(), SpecAction::from_json(v)))
                    .collect()
            };
            let parse_default = |field: &str| {
                let v = &case[field];
                (!v.is_null()).then(|| SpecAction::from_json(v))
            };
            RtTransformBuilder {
                rule_actions: parse_map("rule_actions"),
                token_actions: parse_map("token_actions"),
                default_rule: parse_default("default_rule"),
                default_token: parse_default("default_token"),
                visit_tokens: case["visit_tokens"].as_bool().unwrap_or(true),
                trace: Vec::new(),
            }
        }
    }

    impl<'i> OutputBuilder<'i> for RtTransformBuilder {
        type Value = Maybe;

        fn token(&mut self, token: RtToken, _input: &'i str, _ctx: &OutputContext) -> Maybe {
            let tok_val = TVal::Tok {
                token_type: token.type_.clone(),
                value: token.value.clone(),
            };
            if !self.visit_tokens {
                return Maybe::Keep(tok_val);
            }
            if let Some(a) = self.token_actions.get(&token.type_).cloned() {
                self.trace.push(TraceEntry {
                    kind: "token".to_string(),
                    name: token.type_.clone(),
                });
                apply_token_action(&a, &token.type_, &token.value)
            } else if let Some(a) = self.default_token.clone() {
                self.trace.push(TraceEntry {
                    kind: "default_token".to_string(),
                    name: token.type_.clone(),
                });
                apply_token_action(&a, &token.type_, &token.value)
            } else {
                // Engine token materialization with no visible callback (RFC §5).
                Maybe::Keep(tok_val)
            }
        }

        fn reduce(
            &mut self,
            rule_idx: usize,
            children: &mut Vec<Maybe>,
            _meta: &RtMeta,
            ctx: &OutputContext,
        ) -> Maybe {
            // The engine already dropped `is_discard` children, so every survivor is
            // Keep.
            let values: Vec<TVal> = std::mem::take(children)
                .into_iter()
                .map(|m| match m {
                    Maybe::Keep(v) => v,
                    Maybe::Discard => unreachable!("engine drops discarded children before reduce"),
                })
                .collect();
            let name = ctx.callback_name(rule_idx);
            if let Some(a) = self.rule_actions.get(name).cloned() {
                self.trace.push(TraceEntry {
                    kind: "rule".to_string(),
                    name: name.to_string(),
                });
                apply_rule_action(&a, name, values)
            } else if let Some(a) = self.default_rule.clone() {
                self.trace.push(TraceEntry {
                    kind: "default_rule".to_string(),
                    name: name.to_string(),
                });
                apply_rule_action(&a, name, values)
            } else {
                Maybe::Keep(TVal::Tree {
                    data: name.to_string(),
                    children: values,
                })
            }
        }

        fn is_discard(&self, value: &Maybe) -> bool {
            matches!(value, Maybe::Discard)
        }

        fn placeholder(&mut self, _ctx: &OutputContext) -> Maybe {
            Maybe::Keep(TVal::Null)
        }
    }

    fn maybe_to_json(m: &Maybe) -> Value {
        match m {
            Maybe::Keep(v) => v.to_json(),
            Maybe::Discard => Value::Null,
        }
    }

    fn transformer_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/oracles/transformer")
    }

    /// C9 headline gate: replay every C1 transformer fixture through the standalone
    /// runtime's `parse_into` seam and assert the value **and** trace equal the
    /// Python (embedded-transformer) oracle. Standalone is LALR + basic-lexer only.
    /// Every fixture whose `lalr_basic` oracle status is `"ok"` (Python built, parsed,
    /// and transformed it under the basic lexer) MUST bake, parse, and match here —
    /// a bake/parse/mismatch is a HARD FAILURE, never a skip (see the coverage assert
    /// below). The only legitimate skip is a `status != "ok"` config, which carries
    /// no value/trace to compare — a data property of the oracle, not a standalone
    /// failure.
    #[test]
    fn standalone_semantic_output_matches_transformer_oracle() {
        // The transformer oracle is committed and kept fresh under CI oracle-freshness,
        // so a missing/unreadable cases.json is a HARD FAILURE, never a silent pass.
        let cases_path = transformer_dir().join("cases.json");
        let text = std::fs::read_to_string(&cases_path).unwrap_or_else(|e| {
            panic!(
                "transformer cases.json unreadable at {} ({e}) — the committed oracle \
                 must exist; run generate_transformer_oracles.py",
                cases_path.display()
            )
        });
        let data: Value = serde_json::from_str(&text).expect("valid transformer fixture JSON");
        let cases = data["cases"].as_array().expect("cases array");

        let mut checked = 0usize;
        // `expected` = the number of fixtures whose lalr_basic config is "ok" (i.e.
        // carries a value/trace to compare). Only those are comparable through the
        // standalone seam; every one of them MUST run — see the coverage assert below.
        let mut expected = 0usize;
        let mut non_comparable: Vec<String> = Vec::new();
        for case in cases {
            let name = case["name"].as_str().unwrap();
            let grammar = case["grammar"].as_str().unwrap();
            let cfg = &case["configs"]["lalr_basic"];
            // Only status="ok" configs carry a value/trace to compare against. A
            // non-ok config is a data property of the oracle (nothing to compare),
            // not a standalone failure — that is the ONLY legitimate skip here.
            if cfg["status"].as_str() != Some("ok") {
                non_comparable.push(format!("{name} (config status != ok)"));
                continue;
            }
            expected += 1;

            let popts = &case["parser_options"];
            let opts = LarkOptions {
                parser: ParserAlgorithm::Lalr,
                lexer: crate::LexerType::Basic,
                start: vec!["start".to_string()],
                keep_all_tokens: popts["keep_all_tokens"].as_bool().unwrap_or(false),
                maybe_placeholders: popts["maybe_placeholders"].as_bool().unwrap_or(false),
                ..Default::default()
            };

            // For an "ok" fixture a bake Err or panic is a genuine regression the
            // gate MUST surface — not a skip. (The oracle recorded this grammar as
            // parseable under lalr_basic, so the standalone basic-lexer backend must
            // bake it.)
            let baked = bake(grammar, &opts)
                .unwrap_or_else(|e| panic!("{name}: standalone bake failed: {e}"));
            let leaked = leak_grammar_data(&baked);
            let parser = Parser::from_data(leaked);

            let input = case["input"].as_str().unwrap();
            let mut builder = RtTransformBuilder::from_case(case);
            let root = match parser.parse_into(input, &mut builder) {
                Ok(r) => r,
                Err(e) => panic!("{name}: standalone parse_into failed: {e}"),
            };

            // Value parity vs the Python transformer oracle (post-parse semantics —
            // the standalone `parse_into` contract, see the module note above).
            let got_value = maybe_to_json(&root);
            let want_value = &cfg["value"];
            assert_eq!(
                &got_value, want_value,
                "{name}: standalone semantic VALUE != Python transformer oracle"
            );

            // Trace parity (ordered visible callbacks).
            let got_trace = Value::Array(
                builder
                    .trace
                    .iter()
                    .map(|e| serde_json::json!({"kind": e.kind, "name": e.name}))
                    .collect(),
            );
            let want_trace = &cfg["trace"];
            assert_eq!(
                &got_trace, want_trace,
                "{name}: standalone semantic TRACE != Python transformer oracle"
            );

            checked += 1;
        }

        eprintln!(
            "standalone semantic output: {checked}/{expected} ok fixtures matched the \
             Python transformer oracle; {} non-comparable (status != ok) [{}]",
            non_comparable.len(),
            non_comparable.join(", ")
        );
        assert!(
            expected > 0,
            "no ok transformer fixtures to exercise through the standalone seam"
        );
        // Exact coverage: every ok fixture must have been driven through parse_into
        // and asserted. A skip that silently dropped an ok fixture would show up here.
        assert_eq!(
            checked, expected,
            "not every ok transformer fixture ran through the standalone seam \
             ({checked} checked != {expected} expected)"
        );
    }
}
