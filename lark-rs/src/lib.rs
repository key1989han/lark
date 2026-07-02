pub mod error;
pub mod grammar;
pub mod lexer;
pub mod lookaround;
pub mod parsers;
pub mod perf;
pub mod postlex;
pub mod reconstruct;
pub mod standalone;
pub mod tree;

pub use error::{GrammarError, LarkError, ParseError, RecoveredTree, RecoveryAction};
pub use grammar::{
    load_grammar, lower,
    rule::{Rule, RuleOptions},
    symbol::{NonTerminal, Symbol, Terminal},
    terminal::TerminalDef,
    CompiledGrammar, CompiledRule, Grammar, SymbolId, SymbolKind, SymbolTable,
};
pub use lexer::{BasicLexer, ContextualLexer, DynamicMatcher, Lexer, LexerBackend, LexerConf};
pub use lookaround::classify::{
    classify, lower_terminal, lower_terminal_dotall, route_terminal, route_terminal_dotall,
    Classification, Classifier, DeclineReason, DefaultClassifier, LookaroundIssue, Lowered,
    LoweringRoute, Rejection, Scope, ShapeClass, Verdict,
};
pub use lookaround::lower::{GuardSpec, LookbehindGuard, LowerDecline, LoweredBranch};
pub use parsers::{
    basic_lexer_conf, lalr, EarleyParser, InteractiveParser, LexFailure, OutputBuilder,
    OutputContext, ParseTable, ParserConf, RecoveryContext, TokenSource,
};
#[cfg(feature = "span-tree")]
pub use parsers::{SpanBranch, SpanNode, SpanToken, SpanTreeBuilder};
pub use postlex::Indenter;
pub use reconstruct::{ReconstructError, Reconstructor};
pub use standalone::generate as generate_standalone;
pub use tree::{Child, Meta, ParseTree, Token, Tree};

/// Main entry point — mirrors Python's `Lark(grammar, parser=..., lexer=...)`
pub struct Lark {
    pub grammar: Grammar,
    frontend: parsers::ParsingFrontend,
}

/// `Lark` is `Send` (build on one thread, parse on another; `Mutex<Lark>` is
/// `Sync`) but deliberately not `Sync` — the scanners hold `RefCell`/`OnceCell`
/// scratch. The `Send` half rides the driver box's `Send` supertrait
/// (`parsers::ParserDriver`), which a refactor could silently drop; this pin
/// fails the build if it ever does (PR #146 review).
const _: () = {
    fn assert_send<T: Send>() {}
    #[allow(dead_code)]
    fn pin() {
        assert_send::<Lark>();
    }
};

impl Lark {
    pub fn new(grammar_text: &str, options: LarkOptions) -> Result<Self, LarkError> {
        let grammar = grammar::load_grammar_with_sources(
            grammar_text,
            &options.start,
            options.maybe_placeholders,
            options.keep_all_tokens,
            options.base_path.clone(),
            options.import_sources.clone(),
        )?;
        let frontend = parsers::build_frontend(&grammar, &options)?;
        Ok(Lark { grammar, frontend })
    }

    /// Parse `text` from the default start symbol.
    ///
    /// Returns a [`ParseTree`] — normally a [`Tree`], but a `?start` rule that
    /// collapses via expand1 to a single token yields that bare [`Token`], and a
    /// `?start` whose sole alternative is an absent `[...]` placeholder under
    /// `maybe_placeholders=true` (e.g. `?start: [A]` on empty input) collapses to
    /// [`ParseTree::None`] — Python Lark's literal bare-`None` result (#289). All
    /// three cases mirror Python Lark.
    pub fn parse(&self, text: &str) -> Result<ParseTree, ParseError> {
        self.frontend.parse(text, None)
    }

    pub fn parse_with_start(&self, text: &str, start: &str) -> Result<ParseTree, ParseError> {
        self.frontend.parse(text, Some(start))
    }

    /// Parse `input` directly into an [`OutputBuilder`]'s values, without building a
    /// generic [`ParseTree`] (#232, C7). The engine drives the builder during the
    /// parse — a shifted terminal calls [`token`](OutputBuilder::token), a completed
    /// reduction calls [`reduce`](OutputBuilder::reduce) over its already-shaped
    /// children — mirroring Python Lark's embedded transformer. `parse()` is exactly
    /// this with the built-in tree backend.
    ///
    /// Supported on `parser='lalr'` with the basic or contextual lexer (ADR-0029
    /// fork 4); Earley/CYK stay post-parse and every other configuration returns a
    /// typed [`LarkError`]. A `SpanTree`/custom builder may borrow `&input`, tying
    /// its values' lifetime to `input`.
    pub fn parse_into<'i, B: OutputBuilder<'i>>(
        &self,
        input: &'i str,
        builder: &mut B,
    ) -> Result<B::Value, LarkError> {
        self.frontend.parse_into(input, None, builder)
    }

    /// [`parse_into`](Self::parse_into) from an explicit start symbol.
    pub fn parse_into_with_start<'i, B: OutputBuilder<'i>>(
        &self,
        input: &'i str,
        start: &str,
        builder: &mut B,
    ) -> Result<B::Value, LarkError> {
        self.frontend.parse_into(input, Some(start), builder)
    }

    /// Parse `input` into a zero-copy [`SpanNode`] — the borrowed-span output
    /// backend (#233, C8; experimental, `--features span-tree`, ADR-0029 fork 3).
    ///
    /// Unlike [`parse`](Self::parse), which materializes an owned
    /// [`Tree`]/[`Token`], this builds no `Tree` and copies no token value: a node's
    /// label borrows the grammar (`'g`) and a token's value borrows `input` (`'i`),
    /// so the result lives as long as both. [`SpanNode::materialize`] projects it
    /// back to the exact [`ParseTree`] `parse()` returns. Supported on
    /// `parser='lalr'` with the basic or contextual lexer (ADR-0029 fork 4); every
    /// other configuration returns a typed [`LarkError`], exactly as
    /// [`parse_into`](Self::parse_into).
    ///
    /// [`SpanNode`]: crate::parsers::span_tree::SpanNode
    /// [`SpanNode::materialize`]: crate::parsers::span_tree::SpanNode::materialize
    #[cfg(feature = "span-tree")]
    pub fn parse_span<'i, 'g>(
        &'g self,
        input: &'i str,
    ) -> Result<crate::parsers::span_tree::SpanNode<'i, 'g>, LarkError> {
        self.frontend.parse_span(input, None)
    }

    /// [`parse_span`](Self::parse_span) from an explicit start symbol.
    #[cfg(feature = "span-tree")]
    pub fn parse_span_with_start<'i, 'g>(
        &'g self,
        input: &'i str,
        start: &str,
    ) -> Result<crate::parsers::span_tree::SpanNode<'i, 'g>, LarkError> {
        self.frontend.parse_span(input, Some(start))
    }

    /// Parse `input` onto a flat [`TapeTree`] — the tape output backend prototype
    /// (#243 C8c; **experimental, internal-only**, `--features tape-tree`).
    ///
    /// The whole parse is appended to two flat arrays (amortized growth — O(log n)
    /// allocator calls per parse on the output side): no `Tree` is built, no token
    /// value or label is copied. [`TapeTree::materialize`] projects the tape back
    /// to the exact [`ParseTree`] `parse()` returns (the ADR-0026 relative oracle).
    /// Same support boundary as [`parse_into`](Self::parse_into).
    ///
    /// [`TapeTree`]: crate::parsers::tape::TapeTree
    /// [`TapeTree::materialize`]: crate::parsers::tape::TapeTree::materialize
    #[cfg(feature = "tape-tree")]
    pub fn parse_tape<'i, 'g>(
        &'g self,
        input: &'i str,
    ) -> Result<crate::parsers::tape::TapeTree<'i, 'g>, LarkError> {
        self.frontend.parse_tape(input, None)
    }

    /// Parse with built-in panic-mode error recovery (issues #43, #94).
    ///
    /// Instead of aborting on the first parse error, the parser deletes the
    /// offending token and continues (single-token-deletion recovery), returning a
    /// [`RecoveredTree`]: `tree` is `Some` when recovery reached a valid parse and
    /// `None` when it could not (premature `$END`), plus every error recovered
    /// from. This is exactly Python Lark's `parse(text, on_error=lambda e: True)`
    /// (which likewise re-raises at premature `$END`, our `None`).
    ///
    /// Every LALR configuration supports recovery — basic or contextual lexer, with
    /// or without a postlex (Indenter) hook (issue #94). Only Earley/CYK return an
    /// error. See [`RecoveredTree`] for the tree (`Option`) and error-node semantics.
    pub fn parse_with_recovery(&self, text: &str) -> Result<RecoveredTree, LarkError> {
        self.parse_on_error(text, |_, _| RecoveryAction::Delete)
    }

    /// Parse with a custom `on_error` handler (issue #223). The handler receives
    /// the error and a [`RecoveryContext`] exposing `accepts()`, `feed_token()`,
    /// and `feed()` — the oracle-backed interactive recovery operations. Return a
    /// [`RecoveryAction`]:
    ///
    /// * [`Delete`](RecoveryAction::Delete) — delete the offending token and resume.
    /// * [`Resume`](RecoveryAction::Resume) — drop the errored token and parse the
    ///   next one in the state the handler's feeds produced (at `$END`, retry the
    ///   sentinel). Matching Python Lark's `resume_parse()`. No feeds → `Stop`.
    /// * [`Stop`](RecoveryAction::Stop) — stop recovery; `tree: None`.
    pub fn parse_on_error(
        &self,
        text: &str,
        mut on_error: impl FnMut(&ParseError, &mut RecoveryContext<'_>) -> RecoveryAction,
    ) -> Result<RecoveredTree, LarkError> {
        self.frontend.parse_recovering(text, None, &mut on_error)
    }

    /// As [`parse_on_error`](Self::parse_on_error), from an explicit start symbol.
    pub fn parse_on_error_with_start(
        &self,
        text: &str,
        start: &str,
        mut on_error: impl FnMut(&ParseError, &mut RecoveryContext<'_>) -> RecoveryAction,
    ) -> Result<RecoveredTree, LarkError> {
        self.frontend
            .parse_recovering(text, Some(start), &mut on_error)
    }

    /// Begin an interactive parse (issues #168, #222).
    ///
    /// Returns an [`InteractiveParser`] — a driveable cursor into a live LALR
    /// parse. The caller feeds tokens one at a time (`feed`/`feed_token`),
    /// inspects which terminals the parser would accept (`accepts`), forks
    /// independent cursors, and resumes automated parsing (`resume`). The
    /// lexer is driven lazily, not up front, so the cursor can be created over
    /// incomplete or broken text.
    ///
    /// Supported on LALR without postlex, using the basic or contextual lexer.
    /// Other configurations (Earley, CYK, or LALR with a postlex/Indenter hook)
    /// return a typed error.
    ///
    /// The default start symbol is resolved against [`LarkOptions::start`] the
    /// same way Python Lark's `_verify_start` does (issue #251): with exactly one
    /// configured start it is used; with **more than one** configured start this
    /// returns an error (Python's `ConfigurationError`, "more than 1 possible
    /// start rule") — pick one explicitly via
    /// [`parse_interactive_with_start`](Self::parse_interactive_with_start). The
    /// choice is never a nondeterministic hash-map key.
    pub fn parse_interactive(&self, text: &str) -> Result<InteractiveParser<'_>, LarkError> {
        self.frontend.parse_interactive(text, None)
    }

    /// As [`parse_interactive`](Self::parse_interactive), from an explicit start symbol.
    pub fn parse_interactive_with_start(
        &self,
        text: &str,
        start: &str,
    ) -> Result<InteractiveParser<'_>, LarkError> {
        self.frontend.parse_interactive(text, Some(start))
    }
}

#[derive(Debug, Clone)]
pub struct LarkOptions {
    pub start: Vec<String>,
    pub parser: ParserAlgorithm,
    pub lexer: LexerType,
    pub ambiguity: Ambiguity,
    pub propagate_positions: bool,
    pub keep_all_tokens: bool,
    pub maybe_placeholders: bool,
    /// Reject grammars with shift/reduce conflicts (and same-priority regex
    /// terminal collisions) at construction time instead of silently resolving
    /// them. Mirrors Python Lark's `strict=True`.
    pub strict: bool,
    /// Global regex flags applied to every terminal pattern, as a bitset over
    /// [`grammar::terminal::flags`] (`IGNORECASE` etc.). Mirrors Python Lark's
    /// `g_regex_flags`. Zero (the default) leaves every terminal's own flags
    /// untouched.
    pub g_regex_flags: u32,
    /// Directory that relative `%import .module (...)` (and other non-`common`
    /// file imports) resolve against. Mirrors the base path Python Lark derives
    /// from the importing grammar's file. `None` (the default) means file imports
    /// cannot be resolved — only the bundled libraries (`common`, `python`,
    /// `unicode`, `lark`) are available, as when a grammar is built from an
    /// in-memory string with no source location.
    pub base_path: Option<std::path::PathBuf>,
    /// In-memory grammar sources for relative `%import .module (...)` resolution
    /// (the #47 follow-up): a map of virtual `/`-separated path (e.g. `"tokens.lark"`,
    /// `"dir/lib.lark"`) → grammar text. When `Some`, file imports resolve
    /// against this map *only* — the filesystem is never consulted — with
    /// `base_path` acting as an optional virtual prefix into the map. An imported
    /// grammar's own relative imports resolve against its virtual directory, so
    /// nested imports compose exactly as they do on disk. This is how
    /// environments without a filesystem (the WASM binding, #47) supply sibling
    /// grammars. `None` (the default) keeps the filesystem behavior above.
    pub import_sources: Option<std::sync::Arc<std::collections::HashMap<String, String>>>,
    /// Post-lexer hook applied to the token stream before it reaches the parser.
    /// Currently an [`Indenter`], which injects `%declare`d `INDENT` / `DEDENT`
    /// tokens for Python-style significant-whitespace grammars. Mirrors Python
    /// Lark's `postlex` option. Every parser honours it — LALR on both the basic
    /// and contextual lexer, Earley and CYK on the basic lexer (issue #78) — but
    /// never the dynamic lexer (no token stream exists to rewrite; Python Lark
    /// refuses that pairing too). `None` (the default) leaves the token stream
    /// untouched.
    pub postlex: Option<postlex::Indenter>,
    /// Which combined-scanner engine the lexer builds (see [`LexerBackend`]). This
    /// has **no** Lark equivalent — it selects between byte-for-byte equivalent
    /// scanner implementations (`docs/LEXER_DFA_PLAN.md`) and exists so the L0
    /// differential oracle can build the same grammar under both engines (under
    /// the TEST-ONLY `fancy-oracle` feature the `Regex` backend hosts the
    /// historical fancy-regex reference probes). Both backends refuse the same
    /// patterns with the same categorized errors (`docs/LOOKAROUND_SCOPE.md`).
    /// Defaults to the `regex-automata` DFA scanner.
    pub lexer_backend: LexerBackend,
}

impl Default for LarkOptions {
    fn default() -> Self {
        LarkOptions {
            start: vec!["start".to_string()],
            // LALR is the only implemented backend (Earley is Phase 2). Python
            // Lark defaults to Earley, but here that would make the default
            // options fail to build, so we default to the working backend.
            parser: ParserAlgorithm::Lalr,
            lexer: LexerType::Auto,
            ambiguity: Ambiguity::Resolve,
            propagate_positions: false,
            keep_all_tokens: false,
            maybe_placeholders: false,
            strict: false,
            g_regex_flags: 0,
            base_path: None,
            import_sources: None,
            postlex: None,
            lexer_backend: LexerBackend::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParserAlgorithm {
    Earley,
    Lalr,
    Cyk,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LexerType {
    Auto,
    Basic,
    Contextual,
    Dynamic,
    DynamicComplete,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ambiguity {
    Resolve,
    Explicit,
    Forest,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_grammar_load_simple() {
        let grammar = grammar::load_grammar(
            "start: WORD\nWORD: /[a-z]+/\n",
            &["start".to_string()],
            false,
            false,
        )
        .unwrap();
        assert!(!grammar.rules.is_empty());
        assert!(grammar.terminals.iter().any(|t| t.name == "WORD"));
    }

    #[test]
    fn test_grammar_load_with_import() {
        let grammar = grammar::load_grammar(
            "%import common.WORD\nstart: WORD\n",
            &["start".to_string()],
            false,
            false,
        )
        .unwrap();
        assert!(grammar.terminals.iter().any(|t| t.name == "WORD"));
    }

    #[test]
    fn test_grammar_load_alternation() {
        let res = grammar::load_grammar(
            "start: \"hello\" | \"world\"\n",
            &["start".to_string()],
            false,
            false,
        );
        assert!(res.is_ok());
        let grammar = res.unwrap();
        assert!(grammar.rules.len() >= 2);
    }

    #[test]
    fn test_grammar_ebnf_operators() {
        // star, plus, optional
        let res = grammar::load_grammar(
            "start: item* sep item+\nitem: /[a-z]/\nsep: \",\"?\n",
            &["start".to_string()],
            false,
            false,
        );
        assert!(res.is_ok(), "Grammar load failed: {:?}", res.err());
    }

    #[test]
    fn test_repeated_ebnf_repetition_is_shared() {
        // Python Lark's `rules_cache`: structurally-identical EBNF repetition must
        // collapse to one set of helper rules, not a fresh one per occurrence.
        // Counting the distinct anonymous helper origins, writing `("," NAME)*`
        // twice must add *no* helpers over writing it once — the group, its
        // `+`-recurse core, and the `*` wrapper are all reused.
        fn anon_helper_count(src: &str) -> usize {
            let g = grammar::load_grammar(src, &["start".to_string()], false, false).unwrap();
            let mut names: Vec<&str> = g
                .rules
                .iter()
                .map(|r| r.origin.name.as_str())
                .filter(|n| n.starts_with("__anon"))
                .collect();
            names.sort_unstable();
            names.dedup();
            names.len()
        }
        let once = anon_helper_count("start: NAME (\",\" NAME)*\nNAME: /[a-z]+/\n");
        let twice = anon_helper_count(
            "start: a b\na: NAME (\",\" NAME)*\nb: NAME (\",\" NAME)*\nNAME: /[a-z]+/\n",
        );
        assert!(once > 0, "expected some anonymous helpers, got {once}");
        assert_eq!(
            once, twice,
            "the second `(\",\" NAME)*` must reuse the first's helpers (got {once} vs {twice})"
        );
    }

    #[test]
    fn test_leading_star_distribution_matches_oracle() {
        // Here `(NAME ";")*` is a *leading* repetition — it precedes `","` — so
        // since #97 it is distributed into each parent (`a: __plus "," "p" | ","
        // "p"`, sharing the `+`-recurse core `__plus`), exactly as Python Lark's
        // `SimplifyRule`. That is what lets the grammar build: before distribution,
        // the two `(NAME ";")*` nullable wrappers reduced `ε` on the same `","`
        // lookahead in a common state — an unresolvable reduce/reduce (the python.lark
        // failure). Distribution removes the wrapper entirely, so there is nothing to
        // collide. The test then pins that the distributed form narrows correctly:
        // "p"/"q" both lex as NAME, so distinguishing `a` from `b` after the shared
        // `__plus` loop is pure contextual narrowing, and the absent (zero-item) case
        // is the bare `"," "p"` alternative. Every accept/reject/tree below is
        // byte-identical to Python Lark 1.3.1 (`parser='lalr', lexer='contextual'`).
        let src = "start: a | b\n\
                   a: (NAME \";\")* \",\" \"p\"\n\
                   b: (NAME \";\")* \",\" \"q\"\n\
                   NAME: /[a-z]+/\n\
                   %ignore \" \"\n";
        let l = Lark::new(
            src,
            LarkOptions {
                parser: ParserAlgorithm::Lalr,
                lexer: LexerType::Contextual,
                start: vec!["start".to_string()],
                ..Default::default()
            },
        )
        .expect("leading `*` distributes into each parent, so the grammar builds (no R/R)");

        // The alternative `start` reduced to ("a" or "b"), or None if parse failed.
        let alt = |inp: &str| -> Option<String> {
            match l.parse(inp).ok()? {
                ParseTree::Tree(t) => match t.children.first()? {
                    Child::Tree(c) => Some(c.data.clone()),
                    _ => None,
                },
                _ => None,
            }
        };

        // Correct narrowing through the shared loop: the final "p"/"q" selects the
        // parent (incl. the zero-item case), and the NAME items land in the tree.
        assert_eq!(alt(", p").as_deref(), Some("a"));
        assert_eq!(alt("x ; , p").as_deref(), Some("a"));
        assert_eq!(alt(", q").as_deref(), Some("b"));
        assert_eq!(alt("x ; y ; , q").as_deref(), Some("b"));
        // Anti-leak: after the shared loop and its `,`, only "p"/"q" are valid — the
        // contextual lexer must not also admit NAME there (a follow-set over-merge
        // would). Python rejects `, x`; so must we.
        assert!(
            l.parse(", x").is_err(),
            "NAME must not be admitted after the ','"
        );
    }

    #[test]
    fn test_leading_nullable_is_distributed() {
        // #97: a *named nullable* EBNF helper (`X?`, `X*`, `[X]`) placed *before*
        // further symbols hides those symbols from the LR(0) closure — the dot
        // never advances past the helper until it ε-reduces, so the automaton
        // mispredicts and a shift/reduce conflict against an independently-reached
        // copy of the hidden path silently drops it. Here `awaited: A? atom` and
        // `assign: nm "=" nm` both reach `nm`/`NAME`; before the fix, the leading
        // `A?` helper made the start state shift `NAME` into the `assign`-only path
        // and a bare name (`"b"`) could not be parsed as `awaited` at all. With the
        // leading nullable distributed into `awaited: A atom | atom` (Python Lark's
        // `SimplifyRule`), both name-first paths merge and every input below parses
        // exactly as Python Lark 1.3.1 does (`parser='lalr', lexer='contextual'`).
        // `A`/`NAME` are disjoint so the contextual lexer's keyword retyping is not
        // involved — this isolates the LALR-table fix.
        let src = "start: assign | awaited\n\
                   assign: nm \"=\" nm\n\
                   awaited: A? atom\n\
                   atom: nm\n\
                   nm: NAME\n\
                   A: \"a\"\n\
                   NAME: /[b-z][a-z]*/\n\
                   %ignore \" \"\n";
        let l = Lark::new(
            src,
            LarkOptions {
                parser: ParserAlgorithm::Lalr,
                lexer: LexerType::Contextual,
                start: vec!["start".to_string()],
                ..Default::default()
            },
        )
        .expect("grammar with a leading nullable must build");

        // The rule under `start`, or None if the parse failed.
        let alt = |inp: &str| -> Option<String> {
            match l.parse(inp).ok()? {
                ParseTree::Tree(t) => match t.children.first()? {
                    Child::Tree(c) => Some(c.data.clone()),
                    _ => None,
                },
                _ => None,
            }
        };

        // The headline regression: a bare name reaches `awaited` (the leading-`A?`
        // path), not just the `assign` path. Pre-fix this errored.
        assert_eq!(alt("b").as_deref(), Some("awaited"));
        // The present form of the optional still parses.
        assert_eq!(alt("a b").as_deref(), Some("awaited"));
        // The independently-reached name-first path is unaffected.
        assert_eq!(alt("b = c").as_deref(), Some("assign"));
        // `a` alone has no following atom — rejected, exactly as Python.
        assert!(
            l.parse("a").is_err(),
            "`A?` alone, with no atom, must reject"
        );
    }

    #[test]
    fn test_leading_nullable_star_and_group_distribute() {
        // The same distribution must apply to a leading `*` and a leading optional
        // group `[...]` (without placeholders), not just `?`. Each grammar pairs a
        // rule whose leading nullable precedes `item` (also reachable bare) with an
        // independent `WORD`-first `plain` path, so the leading nullable's content
        // must be predicted directly at the start state. Both parse the absent and
        // present cases, matching Python Lark 1.3.1.
        let build = |src: &str| {
            Lark::new(
                src,
                LarkOptions {
                    parser: ParserAlgorithm::Lalr,
                    lexer: LexerType::Contextual,
                    start: vec!["start".to_string()],
                    ..Default::default()
                },
            )
            .expect("grammar with a leading nullable must build")
        };

        // Leading `*`.
        let star = build(
            "start: stars | plain\n\
             stars: B* item\n\
             plain: WORD \"=\" WORD\n\
             item: WORD\n\
             B: \"b\"\n\
             WORD: /[d-z]+/\n\
             %ignore \" \"\n",
        );
        assert!(
            star.parse("d").is_ok(),
            "`B* item` with `B*` absent must parse"
        );
        assert!(star.parse("b b d").is_ok(), "present `B*` must parse");
        assert!(star.parse("d = e").is_ok());

        // Leading optional group `[...]`.
        let opt = build(
            "start: opt | plain\n\
             opt: [C] item\n\
             plain: WORD \"=\" WORD\n\
             item: WORD\n\
             C: \"c\"\n\
             WORD: /[d-z]+/\n\
             %ignore \" \"\n",
        );
        assert!(
            opt.parse("d").is_ok(),
            "`[C] item` with `[C]` absent must parse"
        );
        assert!(opt.parse("c d").is_ok(), "present `[C]` must parse");
        assert!(opt.parse("d = e").is_ok());
    }

    #[test]
    #[ignore = "slow: builds python.lark's full LALR table; run with --ignored"]
    fn test_python_lark_parses_statements() {
        // #97 end-to-end: with leading nullables distributed (the key offender is
        // `?await_expr: AWAIT? atom_expr`), upstream `python.lark` not only *builds*
        // under LALR (see the build test below) but *parses* real statements —
        // expression, assignment, call, and a `def` with a suite — each of which
        // dies right after the first name without the fix. Trees verified against
        // Python Lark 1.3.1 (`PythonIndenter`, contextual lexer). (A bare `await`
        // keyword still mis-lexes as NAME via a *separate* contextual-lexer
        // keyword-retyping gap, unrelated to this LALR-table fix.)
        let src = include_str!("grammars/python.lark");
        let l = Lark::new(
            src,
            LarkOptions {
                parser: ParserAlgorithm::Lalr,
                lexer: LexerType::Contextual,
                start: vec!["file_input".to_string()],
                maybe_placeholders: true,
                postlex: Some(Indenter {
                    nl_type: "_NEWLINE".to_string(),
                    open_paren_types: vec!["LPAR".into(), "LSQB".into(), "LBRACE".into()],
                    close_paren_types: vec!["RPAR".into(), "RSQB".into(), "RBRACE".into()],
                    indent_type: "_INDENT".to_string(),
                    dedent_type: "_DEDENT".to_string(),
                    tab_len: 8,
                }),
                ..Default::default()
            },
        )
        .expect("python.lark must build under LALR");
        for inp in [
            "x = 1\n",
            "f(x)\n",
            "x\n",
            "def f(a, b):\n    return a + b\n",
        ] {
            assert!(
                l.parse(inp).is_ok(),
                "python.lark must parse {inp:?}, got {:?}",
                l.parse(inp).err()
            );
        }
    }

    #[test]
    #[ignore = "slow: builds python.lark's full LALR table; run with --ignored \
                (the fix is pinned fast by \
                test_non_final_maybe_distributes_under_placeholders)"]
    fn test_python_lark_star_param_after_positional() {
        // Issue #106, fixed: a `def`/lambda parameter list with a positional
        // parameter *before* a star-param — `def f(a, *b)` / `def f(a, **b)` —
        // parses, as in Python Lark 1.3.1 (`lalr`, `contextual`, `PythonIndenter`).
        //
        // The cause was `parameters`' *non-final* `["," SLASH ("," paramvalue)*]`
        // under `maybe_placeholders`: it was kept as a nullable Maybe helper
        // (placeholder distribution wasn't supported inline), which hid the
        // following `["," [starparams | kwparams]]` branch from the LR(0) closure
        // — the post-comma state's shift-over-reduce resolution silently dropped
        // the `*`/`**` lookahead (#97's dot-hiding disease, placeholder variant).
        // Fixed by distributing `[...]` under `maybe_placeholders` like Python's
        // `_EMPTY` markers (`RuleOptions::nones_before`), recursively. The fast
        // distilled pin lives in `tests/test_placeholders_and_priority.rs`
        // (`test_non_final_maybe_distributes_under_placeholders`); this test keeps
        // the original end-to-end witness on the real grammar.
        let src = include_str!("grammars/python.lark");
        let l = Lark::new(
            src,
            LarkOptions {
                parser: ParserAlgorithm::Lalr,
                lexer: LexerType::Contextual,
                start: vec!["file_input".to_string()],
                maybe_placeholders: true,
                postlex: Some(Indenter {
                    nl_type: "_NEWLINE".to_string(),
                    open_paren_types: vec!["LPAR".into(), "LSQB".into(), "LBRACE".into()],
                    close_paren_types: vec!["RPAR".into(), "RSQB".into(), "RBRACE".into()],
                    indent_type: "_INDENT".to_string(),
                    dedent_type: "_DEDENT".to_string(),
                    tab_len: 8,
                }),
                ..Default::default()
            },
        )
        .expect("python.lark must build under LALR");
        // Every def-site parameter-list shape Python Lark accepts (trees verified
        // byte-identical to Python Lark 1.3.1, including `None` placeholder
        // positions).
        for inp in [
            "def f(*b): pass\n",
            "def f(**b): pass\n",
            "def f(a, *b): pass\n",
            "def f(a, **b): pass\n",
            "def f(a, /, b, *c): pass\n",
            "def f(a, b=1, *args, **kwargs): pass\n",
            "def f(a, *, b): pass\n",
            "def f(a,): pass\n",
            "lambda a, *b: a\n",
        ] {
            assert!(
                l.parse(inp).is_ok(),
                "python.lark must parse {inp:?} (Python Lark does), got {:?}",
                l.parse(inp).err()
            );
        }
    }

    #[test]
    fn test_named_keyword_terminal_retypes_over_identifier() {
        // A named terminal defined as a single case-sensitive string literal
        // (`ASYNC: "async"`) must compile to `Pattern::Str`, like an inline literal
        // and like Python Lark's `PatternStr`, so it joins the contextual lexer's
        // keyword `unless` retyping. Otherwise it is a `Pattern::Re` that ties with
        // the overlapping `NAME` regex and loses, and `async` lexes as an identifier
        // — the bug that kept python.lark's `async`/`await` from parsing. Outcomes
        // are byte-identical to Python Lark 1.3.1 (`lalr`, `contextual`).
        let src = "start: stmt+\n\
                   ?stmt: kw_stmt | id_stmt\n\
                   kw_stmt: ASYNC NAME NL\n\
                   id_stmt: NAME NL\n\
                   ASYNC: \"async\"\n\
                   NAME: /[a-z]+/\n\
                   NL: /\\n/\n\
                   %ignore \" \"\n";
        let l = Lark::new(
            src,
            LarkOptions {
                parser: ParserAlgorithm::Lalr,
                lexer: LexerType::Contextual,
                start: vec!["start".to_string()],
                ..Default::default()
            },
        )
        .expect("build");
        let alt = |inp: &str| -> Option<String> {
            match l.parse(inp).ok()? {
                ParseTree::Tree(t) => match t.children.first()? {
                    Child::Tree(c) => Some(c.data.clone()),
                    _ => None,
                },
                _ => None,
            }
        };
        // `async` overlapping NAME is retyped to the keyword where the state expects
        // it, and stays an identifier-shaped token nowhere else.
        assert_eq!(alt("async foo\n").as_deref(), Some("kw_stmt"));
        assert_eq!(alt("foo\n").as_deref(), Some("id_stmt"));
        // `async` is a hard keyword (cannot be a bare NAME), exactly as in Python.
        assert!(l.parse("async\n").is_err());
        assert!(l.parse("foo bar\n").is_err());
    }

    #[test]
    #[ignore = "slow (~18s debug): builds python.lark's full LALR table; run with --ignored"]
    fn test_python_lark_builds_under_lalr() {
        // End-to-end witness for the EBNF-helper dedup: upstream `python.lark` has
        // many repeated `("," X)*` patterns that, without `rules_cache`-style
        // sharing, expand to duplicate nullable helpers and collide as
        // unresolvable reduce/reduce — so the LALR table cannot be built at all
        // (see issue #79). With sharing the table builds — and with #97/#100
        // (leading-nullable distribution) and the named-keyword-terminal
        // `PatternStr` fix (async/await) also landed, it now *parses* end-to-end
        // too, so this asserts both: the table builds and a multi-feature program
        // (class + decorator + `async def`/`await`/`async for` + a comprehension)
        // parses. Trees verified against Python Lark 1.3.1 (`PythonIndenter`,
        // contextual lexer).
        let src = include_str!("grammars/python.lark");
        let res = Lark::new(
            src,
            LarkOptions {
                parser: ParserAlgorithm::Lalr,
                lexer: LexerType::Contextual,
                start: vec!["file_input".to_string()],
                maybe_placeholders: true,
                postlex: Some(Indenter {
                    nl_type: "_NEWLINE".to_string(),
                    open_paren_types: vec!["LPAR".into(), "LSQB".into(), "LBRACE".into()],
                    close_paren_types: vec!["RPAR".into(), "RSQB".into(), "RBRACE".into()],
                    indent_type: "_INDENT".to_string(),
                    dedent_type: "_DEDENT".to_string(),
                    tab_len: 8,
                }),
                ..Default::default()
            },
        );
        let l = res.expect("python.lark must build under LALR");
        let prog = "@register\n\
                    class Account(Base):\n\
                    \x20   async def sync(self, source):\n\
                    \x20       async for chunk in source.stream():\n\
                    \x20           self.balance += await chunk.read()\n\
                    \x20       return [x for x in source if x is not None]\n";
        assert!(
            l.parse(prog).is_ok(),
            "python.lark must parse a multi-feature program, got {:?}",
            l.parse(prog).err()
        );
    }

    #[test]
    fn test_terminal_reference_is_inlined() {
        // A terminal may reference another terminal (defined in any order); the
        // referenced pattern is inlined and the referenced-only terminal is pruned.
        let grammar = grammar::load_grammar(
            "start: GREETING\nGREETING: HELLO | HI\nHELLO: \"hello\"\nHI: \"hi\"\n",
            &["start".to_string()],
            false,
            false,
        )
        .unwrap();
        let names: Vec<&str> = grammar.terminals.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"GREETING"), "GREETING kept: {names:?}");
        assert!(!names.contains(&"HELLO"), "HELLO inlined+pruned: {names:?}");
        assert!(!names.contains(&"HI"), "HI inlined+pruned: {names:?}");
    }

    #[test]
    fn test_cyclic_terminal_is_rejected() {
        // Terminals denote regular languages, so a reference cycle is an error
        // (Python Lark raises GrammarError too) — not a hang or stack overflow.
        let res = grammar::load_grammar(
            "start: A\nA: \"a\" B\nB: \"b\" A\n",
            &["start".to_string()],
            false,
            false,
        );
        assert!(
            matches!(res, Err(GrammarError::Other { .. })),
            "got {res:?}"
        );
    }

    #[test]
    fn test_undefined_symbol_is_rejected() {
        // Python Lark rejects a grammar that references an undefined symbol when the
        // grammar is compiled (`GrammarError("... used but not defined")`), rather
        // than deferring to a confusing parse-time failure. The loader's
        // use-before-definition pass matches that: an undefined uppercase reference
        // is an `UndefinedTerminal`, an undefined lowercase one an `UndefinedRule`.
        let res = grammar::load_grammar("start: UNDEFINED\n", &["start".to_string()], false, false);
        assert!(
            matches!(res, Err(GrammarError::UndefinedTerminal { .. })),
            "expected undefined-terminal rejection, got {res:?}"
        );
        let res = grammar::load_grammar("start: undefined\n", &["start".to_string()], false, false);
        assert!(
            matches!(res, Err(GrammarError::UndefinedRule { .. })),
            "expected undefined-rule rejection, got {res:?}"
        );
    }

    #[test]
    fn test_grammar_ignore_directive() {
        let grammar = grammar::load_grammar(
            "start: WORD\nWORD: /[a-z]+/\n%ignore /[ \\t]+/\n",
            &["start".to_string()],
            false,
            false,
        )
        .unwrap();
        assert!(!grammar.ignore.is_empty());
    }
}
