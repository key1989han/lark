//! Lexer implementations: BasicLexer and ContextualLexer.
//!
//! BasicLexer: one combined alternation regex over all terminals, scanning the
//!             input left-to-right.
//!
//! ContextualLexer: at each parser state, only attempts the terminals that are
//!                  valid according to the LALR action table. This is Lark's key
//!                  innovation for LALR parsing — the parser table tells the lexer
//!                  which terminals to try, resolving terminal conflicts that would
//!                  otherwise need hand-written lexer states.
//!
//! Both share a combined scanner behind the [`ScannerBackend`] seam — the
//! `regex`-crate [`scanner::Scanner`] or the default `regex-automata`
//! [`dfa::DfaScanner`]. The alternation uses leftmost-first semantics, identical
//! to Python `re` — so terminal *order* decides ties, exactly as in Python Lark.
//! Order is `(priority desc, max_width desc, pattern-length desc, name asc)`.
//!
//! On top of that, the scanner implements Lark's **"unless" keyword retyping**
//! (`_create_unless` in Python Lark): a string terminal whose value is fully
//! matched by a regex terminal of the same priority (e.g. the keyword `if` inside
//! the identifier pattern `CNAME`) is *removed* from the alternation, and the
//! regex match is retyped back to the keyword when the matched text equals it.
//! This is what makes `if` lex as `IF` while `iffy` stays `NAME`.
//!
//! Every matched terminal is identified by its interned [`SymbolId`]; the parser
//! dispatches on that id directly. The token's name is carried only for display.
//!
//! Module layout (one concern per file):
//!
//! ```text
//! plan        ScannerPlan: selection, Python-style ordering, `unless` retyping
//! pattern     flag-wrapper algebra (the loader's baked `(?is:…)` and its inverse)
//! route       THE refusal seam: lower a regex-rejected terminal or scope-error
//! guard       compiled boundary/lookbehind guards + their compilation context
//! scanner     the `regex`-crate combined-alternation backend (+ side-probes)
//! dfa         the `regex-automata` multi-pattern DFA backend (the default)
//! fence       the fence-idiom matcher (tag-echo heredocs / bracket arguments)
//! dynamic     per-terminal matching for Earley's dynamic lexer
//! collision   strict-mode regex-collision + zero-width construction checks
//! ```

mod collision;
mod dfa;
mod dynamic;
mod fence;
mod guard;
mod pattern;
mod plan;
mod route;
mod scanner;
#[cfg(test)]
mod tests;

pub use collision::{check_regex_collisions, check_zero_width_terminals};
pub use dynamic::DynamicMatcher;
pub(crate) use pattern::strip_whole_pattern_flag_wrapper;
pub use plan::{check_standalone_regex_hostable, scanner_plan, ScannerPlan, UnlessEntry};

use std::collections::HashMap;

use dfa::DfaScanner;
use scanner::Scanner;

use crate::error::{GrammarError, ParseError};
use crate::grammar::intern::SymbolId;
use crate::grammar::terminal::TerminalDef;
use crate::tree::{checked_pos, Token};

/// Account the forward-skip cost of one per-position scan attempt for the
/// deterministic lexer-scaling gate ([`crate::perf::lexer_scan_steps`]).
/// `match_start` is where the engine's leftmost match (searched *at or after*
/// `pos`) actually began, or `None` on a miss. The recorded cost is the number of
/// bytes the search skipped *past* `pos` before reporting that match, plus one for
/// the attempt itself.
///
/// A miss is charged a flat `1`, deliberately: from the return value alone an
/// anchored (`\G`) search and an unanchored one are indistinguishable on a no-match
/// (both yield `None`), even though the unanchored one scanned to end-of-input to
/// get there. Charging the miss its true scan length would therefore falsely flag
/// an *anchored* scanner as quadratic. So the pathology is made observable from the
/// other side: a workload that contains a *sparse* match means the unanchored
/// search reports a far-ahead `start` (the skip we count) at every position before
/// it, while the anchored search keeps missing at `pos` — exactly the
/// `tests/test_lexer_scaling.rs` shape. Compiles to nothing without `perf-counters`.
#[inline]
fn record_scan_skip(pos: usize, match_start: Option<usize>) {
    let skip = match match_start {
        Some(start) => start.saturating_sub(pos) as u64,
        None => 0,
    };
    crate::perf::add_lexer_scan_steps(skip + 1);
}

/// Whether a lexing call materializes an owned `Token.value: String` or emits a
/// value-less span token (C8.1 #582).
///
/// The span-emitting path (`parse_span`) borrows the token's text out of the parse
/// `input` in [`SpanTreeBuilder::token`](crate::parsers::SpanTreeBuilder) via the
/// token's char positions, so the lexer's owned `value` copy is pure waste there —
/// [`TokenValueMode::Span`] skips the `value.to_string()` allocation entirely
/// (leaving `Token.value` empty) and, unlike [`TokenValueMode::Owned`], does not
/// charge [`crate::perf::add_lexer_token_value_bytes`]. `Owned` is the default
/// `parse()`/`parse_into` path and is byte-identical to before.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TokenValueMode {
    /// Materialize `Token.value` as an owned `String` (default; counts the alloc).
    Owned,
    /// Emit a value-less span token — no `value` allocation, no lexer counter charge.
    Span,
}

/// Build the `Token.value` string for a matched terminal under `mode`. In
/// [`TokenValueMode::Owned`] this is `value.to_string()` and its byte length is
/// charged to the lexer-side counter (C8.1 #582); in [`TokenValueMode::Span`] it is
/// an empty (non-allocating) `String` and nothing is charged — the value is
/// recovered as an `&input` slice downstream.
#[inline]
fn token_value(value: &str, mode: TokenValueMode) -> String {
    match mode {
        TokenValueMode::Owned => {
            crate::perf::add_lexer_token_value_bytes(value.len() as u64);
            value.to_string()
        }
        TokenValueMode::Span => String::new(),
    }
}

/// Dense per-terminal-id `%ignore` flags (indexed by `SymbolId::index()`): the
/// per-token ignore check is an array read, not a SipHash set probe (perf spike
/// 2026-07-01). Sized to cover both the terminal ids and the ignore ids.
fn dense_ignore(conf: &LexerConf) -> Vec<bool> {
    let max = conf
        .terminals
        .iter()
        .map(|(id, _)| id.index())
        .chain(conf.ignore.iter().map(|id| id.index()))
        .max()
        .unwrap_or(0);
    let mut ignore = vec![false; max + 1];
    for id in &conf.ignore {
        ignore[id.index()] = true;
    }
    ignore
}

// ─── Configuration ────────────────────────────────────────────────────────────

/// Which engine backs the per-position match (`match_at`). Selects between
/// the two combined-scanner implementations behind the single [`ScannerBackend`]
/// seam, with no behavioral difference — both reproduce Lark's leftmost-first
/// selection, `unless` retyping, and lookaround side-probes byte-for-byte (the L0
/// differential oracle in `tests/test_scanner_differential.rs` is the contract).
///
///   * [`Regex`](LexerBackend::Regex) — the original `regex`-crate combined
///     alternation with capture groups (see [`scanner::Scanner`]).
///   * [`Dfa`](LexerBackend::Dfa) — a `regex-automata` multi-pattern DFA
///     (`docs/LEXER_DFA_PLAN.md`, phase L1; see [`dfa::DfaScanner`]). This is now the
///     default: the L0 differential oracle proves it lexes byte-identically to the
///     `regex` Scanner over the full bank + JSON + python/lark corpora, and it is
///     faster on the all-plain common path (`benches/lex_backends`, `BENCH.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LexerBackend {
    /// The `regex`-crate combined-alternation scanner (the original engine).
    Regex,
    /// The `regex-automata` multi-pattern DFA scanner (phase L1). The default.
    #[default]
    Dfa,
}

#[derive(Debug, Clone)]
pub struct LexerConf {
    /// Terminal id paired with its definition.
    pub terminals: Vec<(SymbolId, TerminalDef)>,
    /// Terminal ids to discard after matching (from `%ignore`).
    pub ignore: Vec<SymbolId>,
    /// Global regex flags (Lark's `g_regex_flags`) applied to every terminal in
    /// the combined scanner regex. Zero leaves each terminal's own flags as-is.
    pub global_flags: u32,
    /// Which combined-scanner engine to build (see [`LexerBackend`]). Defaults to
    /// the `regex-automata` [`dfa::DfaScanner`]; the original `regex` Scanner is
    /// opt-in.
    pub backend: LexerBackend,
}

impl LexerConf {
    pub fn new(terminals: Vec<(SymbolId, TerminalDef)>, ignore: Vec<SymbolId>) -> Self {
        LexerConf {
            terminals,
            ignore,
            global_flags: 0,
            backend: LexerBackend::default(),
        }
    }

    /// Set the global regex flags (builder-style) for `g_regex_flags` support.
    pub fn with_global_flags(mut self, flags: u32) -> Self {
        self.global_flags = flags;
        self
    }

    /// Select the combined-scanner backend (builder-style). The default is the
    /// `regex-automata` [`dfa::DfaScanner`]; choosing [`LexerBackend::Regex`] swaps
    /// back to the original `regex`-crate [`scanner::Scanner`] without changing any
    /// lexing semantics. Both refuse the same patterns with the same categorized
    /// scope errors (`docs/LOOKAROUND_SCOPE.md`); a lowered lookaround terminal
    /// rides the shared DFA branches there and a per-terminal side-probe here (the
    /// TEST-ONLY `fancy-oracle` feature swaps the probe for the historical fancy
    /// reference).
    pub fn with_backend(mut self, backend: LexerBackend) -> Self {
        self.backend = backend;
        self
    }

    /// id → name table for token display. Dense (indexed by `SymbolId::index()`)
    /// so the per-token name lookup is an array read, not a SipHash map probe
    /// (perf spike 2026-07-01) — terminal ids sit in `[0, n_terminals)` by
    /// construction (`intern.rs`).
    fn names(&self) -> Vec<String> {
        let max = self
            .terminals
            .iter()
            .map(|(id, _)| id.index())
            .max()
            .unwrap_or(0);
        let mut names = vec![String::new(); max + 1];
        for (id, t) in &self.terminals {
            names[id.index()] = t.name.clone();
        }
        names
    }
}

// ─── Lexer trait ─────────────────────────────────────────────────────────────

pub trait Lexer {
    /// Lex the full input text, returning all tokens (ignoring filtered types).
    fn lex(&self, text: &str) -> Result<Vec<Token>, ParseError>;
}

// ─── ScannerBackend: the match_at seam over the two combined-scanner engines ───

/// The single insertion point both [`BasicLexer`] and the per-state
/// [`ContextualLexer`] funnel every token through: `match_at(text, pos) ->
/// Option<(SymbolId, &str)>`. It wraps whichever combined-scanner engine
/// [`LexerConf::backend`] selected, so the lexers never branch on the engine and a
/// new backend lands behind this one seam (`docs/LEXER_DFA_PLAN.md`).
///
/// Static dispatch (an enum, not a trait object) keeps the hot per-position call a
/// direct branch — this runs once per token on the contextual lexer's pull path.
enum ScannerBackend {
    /// The `regex`-crate combined-alternation scanner (today's engine).
    Regex(Scanner),
    /// The `regex-automata` multi-pattern DFA scanner (phase L1).
    Dfa(DfaScanner),
}

impl ScannerBackend {
    /// Build the backend named by `backend` over the candidate terminals. Both
    /// engines reproduce Lark's selection byte-for-byte (the L0 differential oracle,
    /// `tests/test_scanner_differential.rs`, is the contract).
    fn build(
        terminals: &[(SymbolId, &TerminalDef)],
        global_flags: u32,
        backend: LexerBackend,
    ) -> Result<ScannerBackend, GrammarError> {
        match backend {
            LexerBackend::Regex => Ok(ScannerBackend::Regex(Scanner::build(
                terminals,
                global_flags,
            )?)),
            LexerBackend::Dfa => Ok(ScannerBackend::Dfa(DfaScanner::build(
                terminals,
                global_flags,
            )?)),
        }
    }

    /// Match a single token starting exactly at `pos` — the seam every lexer uses.
    #[inline]
    fn match_at<'t>(&self, text: &'t str, pos: usize) -> Option<(SymbolId, &'t str)> {
        match self {
            ScannerBackend::Regex(s) => s.match_at(text, pos),
            ScannerBackend::Dfa(s) => s.match_at(text, pos),
        }
    }
}

// ─── BasicLexer ──────────────────────────────────────────────────────────────

/// Scans the whole input with a single combined regex over all terminals.
pub struct BasicLexer {
    scanner: ScannerBackend,
    names: Vec<String>,
    /// Dense per-terminal-id ignore flags (indexed by `SymbolId::index()`), so
    /// the per-token `%ignore` check is an array read, not a SipHash set probe
    /// (perf spike 2026-07-01).
    ignore: Vec<bool>,
}

impl BasicLexer {
    pub fn new(conf: &LexerConf) -> Result<Self, GrammarError> {
        let refs: Vec<(SymbolId, &TerminalDef)> =
            conf.terminals.iter().map(|(id, t)| (*id, t)).collect();
        let scanner = ScannerBackend::build(&refs, conf.global_flags, conf.backend)?;
        Ok(BasicLexer {
            scanner,
            names: conf.names(),
            ignore: dense_ignore(conf),
        })
    }

    /// The single token the combined scanner matches starting **exactly** at byte
    /// `pos` — the terminal id (after `unless` retyping) and the matched slice — or
    /// `None` if nothing matches there. This is the raw `match_at` seam without the
    /// streaming loop or `%ignore` handling; it lets the L2 lowering harness probe a
    /// terminal's anchored match at a position without lexing the whole input
    /// (`tests/common/lowering.rs`).
    pub fn match_at<'t>(&self, text: &'t str, pos: usize) -> Option<(SymbolId, &'t str)> {
        self.scanner.match_at(text, pos)
    }
}

/// A running source cursor (byte offset + **character** offset + 1-based
/// line/column), advanced one matched span or one skipped character at a time.
/// Used by [`BasicLexer::lex`] so the newline-aware position bookkeeping lives in
/// exactly one place.
///
/// `pos` is the **byte** offset — it drives all regex slicing (`text[pos..]`) and
/// loop control. `char_pos` is the **character** index, which is what a `Token`'s
/// `start_pos`/`end_pos` carry: Python Lark's `start_pos`/`end_pos` are char
/// indices, not byte offsets, and the two diverge on any non-ASCII input (#278,
/// bounty N8). Both advance together in [`feed`](Self::feed).
struct LexCursor {
    pos: usize,
    char_pos: usize,
    line: usize,
    col: usize,
}

impl LexCursor {
    fn new() -> Self {
        LexCursor {
            pos: 0,
            char_pos: 0,
            line: 1,
            col: 1,
        }
    }

    /// Advance the cursor over `value` (a matched terminal slice or a skipped
    /// run of text), tracking newlines and the character count.
    fn feed(&mut self, value: &str) {
        for ch in value.chars() {
            if ch == '\n' {
                self.line += 1;
                self.col = 1;
            } else {
                self.col += 1;
            }
            self.char_pos += 1;
        }
        self.pos += value.len();
    }
}

impl BasicLexer {
    /// Build a [`Token`] for a matched terminal `id`/`value` starting at `start`,
    /// advancing `cur` past it. Returns `None` when the matched terminal is an
    /// `%ignore` type (it still advances the cursor, but produces no token).
    fn make_token(
        &self,
        cur: &mut LexCursor,
        id: SymbolId,
        value: &str,
        mode: TokenValueMode,
    ) -> Option<Token> {
        // `start_pos`/`end_pos` are **character** indices (Python parity, #278) —
        // `cur.char_pos`, not the byte offset `cur.pos`.
        let start_pos = cur.char_pos;
        let start_line = cur.line;
        let start_col = cur.col;
        cur.feed(value);
        if self.is_ignored(id) {
            return None;
        }
        Some(Token {
            type_id: id,
            type_: self.names[id.index()].clone(),
            value: token_value(value, mode),
            line: checked_pos(start_line),
            column: checked_pos(start_col),
            end_line: checked_pos(cur.line),
            end_column: checked_pos(cur.col),
            start_pos: checked_pos(start_pos),
            end_pos: checked_pos(cur.char_pos),
        })
    }

    /// Lex the single token the combined scanner matches **exactly** at byte offset
    /// `pos` (1-based `line`/`col` carry the position into the produced token). The
    /// returned token may be an `%ignore` terminal — the caller decides whether to
    /// keep or skip it via [`is_ignored`](Self::is_ignored) — so this does *not*
    /// advance past ignored runs and never mutates any state. `Err(())` when no
    /// terminal matches here (an un-lexable character the caller recovers from); the
    /// caller stops the loop at end of input before calling this.
    ///
    /// This is the lazy, one-token-at-a-time seam the recovering basic-lexer postlex
    /// source ([`crate::parsers::BasicRecovering`]) drives, so a char skip can be
    /// interleaved with the streaming indenter + parser resume exactly as Python
    /// Lark's lazy `lexer → PostLexConnector → parser` pipeline does — rather than
    /// the eager [`lex_recovering`](Self::lex_recovering) which skips every
    /// un-lexable character up front, before the indenter runs. It mirrors
    /// [`ContextualLexer::next_token`](ContextualLexer::next_token)'s contract: no
    /// state, ignored tokens surfaced to the caller.
    ///
    /// [`lex_recovering`]: Self::lex_recovering
    pub fn next_token_at(
        &self,
        text: &str,
        pos: usize,
        char_pos: usize,
        line: usize,
        col: usize,
    ) -> Result<Token, ()> {
        match self.scanner.match_at(text, pos) {
            Some((id, value)) => {
                let mut cur = LexCursor {
                    pos,
                    char_pos,
                    line,
                    col,
                };
                // Character indices for `start_pos`/`end_pos` (#278), byte `pos`
                // stays the scanner cursor.
                let start_pos = cur.char_pos;
                let start_line = cur.line;
                let start_col = cur.col;
                cur.feed(value);
                Ok(Token {
                    type_id: id,
                    type_: self.names[id.index()].clone(),
                    value: token_value(value, TokenValueMode::Owned),
                    line: checked_pos(start_line),
                    column: checked_pos(start_col),
                    end_line: checked_pos(cur.line),
                    end_column: checked_pos(cur.col),
                    start_pos: checked_pos(start_pos),
                    end_pos: checked_pos(cur.char_pos),
                })
            }
            None => Err(()),
        }
    }

    /// Whether `id` is an `%ignore` terminal (whitespace/comments the parser skips).
    /// Mirrors [`ContextualLexer::is_ignored`](ContextualLexer::is_ignored) for the
    /// lazy basic-recovering source.
    pub fn is_ignored(&self, id: SymbolId) -> bool {
        self.ignore.get(id.index()).copied().unwrap_or(false)
    }

    /// The span-emitting analog of [`Lexer::lex`]: eager whole-stream lexing that
    /// builds **value-less** tokens (C8.1 #582) — the token positions still carry the
    /// span, but `Token.value` is empty and the lexer allocates no owned value string
    /// (`crate::perf::lexer_token_value_bytes` stays 0). Used by `parse_span` over the
    /// basic lexer; the borrowed text is recovered from `input` in the span builder.
    /// Byte-for-byte the same token stream as [`Lexer::lex`] except for the empty
    /// `value` field (identical `type_id`/`type_`/positions).
    pub(crate) fn lex_span(&self, text: &str) -> Result<Vec<Token>, ParseError> {
        self.lex_with_mode(text, TokenValueMode::Span)
    }

    fn lex_with_mode(&self, text: &str, mode: TokenValueMode) -> Result<Vec<Token>, ParseError> {
        let mut tokens = Vec::new();
        let mut cur = LexCursor::new();

        while cur.pos < text.len() {
            match self.scanner.match_at(text, cur.pos) {
                Some((id, value)) => {
                    if let Some(tok) = self.make_token(&mut cur, id, value, mode) {
                        tokens.push(tok);
                    }
                }
                None => {
                    let ch = text[cur.pos..].chars().next().unwrap();
                    return Err(ParseError::UnexpectedCharacter {
                        ch,
                        line: cur.line,
                        col: cur.col,
                        pos: cur.pos,
                        expected: "any token".to_string(),
                    });
                }
            }
        }

        // `$END` carries the char index, not the byte offset (#278).
        tokens.push(Token::end().with_position(cur.line, cur.col, cur.char_pos, cur.char_pos));
        Ok(tokens)
    }
}

impl Lexer for BasicLexer {
    fn lex(&self, text: &str) -> Result<Vec<Token>, ParseError> {
        // The default owned-value path (byte-identical to before): each token
        // materializes its `value: String` and charges the lexer-side counter.
        self.lex_with_mode(text, TokenValueMode::Owned)
    }
}

// ─── ContextualLexer ─────────────────────────────────────────────────────────

/// A lexer that narrows the candidate terminals to those valid in the current
/// LALR parser state. States with the same terminal set share one scanner
/// (Python Lark's `lexer_by_tokens` dedup — measured 4–5× fewer scanners on the
/// wild bank), and each scanner is built lazily on first use (Python's
/// `BasicLexer.scanner` property), so states an input never visits cost nothing.
/// Keyword/identifier disambiguation (the `unless` retyping) is still computed
/// per terminal-set, exactly as Python Lark builds one `TraditionalLexer` per
/// distinct set.
pub struct ContextualLexer {
    /// LALR state id → index into `scanners`. States whose terminal sets are
    /// equal map to the same index. State 0 is the root (fallback) entry.
    /// Dense parser-state → scanner-slot table (`u32::MAX` = no scanner for that
    /// state), so the per-token scanner lookup is an array read, not a SipHash map
    /// probe (perf spike 2026-07-01). Parser states are dense `[0, n_states)`.
    state_to_scanner: Vec<u32>,
    /// One entry per distinct terminal set, built lazily on first use.
    scanners: Vec<LazyScanner>,
    /// The root (fallback) scanner over the **full** terminal set, built lazily on
    /// first use — Python Lark's `ContextualLexer.root_lexer`. It is consulted only
    /// during error recovery (see [`Self::next_root_token`]): when the per-state
    /// scanner refuses a position, the root scanner says whether the input there is
    /// an out-of-context-but-valid *token* (deletable) or a genuinely un-lexable
    /// character. A normal (non-recovering) parse never touches it.
    root: LazyScanner,
    /// Owned terminal definitions the lazy builds draw from.
    terminals: HashMap<SymbolId, TerminalDef>,
    global_flags: u32,
    backend: LexerBackend,
    names: Vec<String>,
    /// Dense per-terminal-id ignore flags — see [`BasicLexer::ignore`].
    ignore: Vec<bool>,
}

/// A per-terminal-set scanner slot, built on first use. Single-threaded by
/// design ([`Lark`](crate::Lark) is not `Sync` — the `regex` backend already
/// holds a `RefCell` scratch buffer), so a plain `OnceCell` suffices.
struct LazyScanner {
    /// Sorted, deduped terminal ids — the dedup key. Scanner construction is
    /// order-independent ([`scanner_plan`] sorts by `(-priority, -len, name)`,
    /// a total order), so the set fully determines the scanner.
    term_ids: Vec<SymbolId>,
    cell: std::cell::OnceCell<ScannerBackend>,
}

impl LazyScanner {
    fn get_or_build(
        &self,
        terminals: &HashMap<SymbolId, TerminalDef>,
        global_flags: u32,
        backend: LexerBackend,
    ) -> &ScannerBackend {
        self.cell.get_or_init(|| {
            let terms: Vec<(SymbolId, &TerminalDef)> = self
                .term_ids
                .iter()
                .map(|id| (*id, &terminals[id]))
                .collect();
            // Cannot fail: every terminal here was already routed/lowered by the
            // full-set validation build in `ContextualLexer::new`, and a subset
            // alternation introduces no new failure mode (`compute_unless` pairs
            // and DFA patterns are each a subset of the validated full set).
            ScannerBackend::build(&terms, global_flags, backend).expect(
                "per-state scanner build failed after the full-terminal validation \
                 build succeeded — this is a lark-rs bug",
            )
        })
    }
}

impl ContextualLexer {
    /// Build a contextual lexer.
    ///
    /// `state_terminals`: LALR state id → valid terminal ids.
    /// `always_accept`: terminals valid in every state (e.g. `%ignore`).
    pub fn new(
        conf: &LexerConf,
        state_terminals: &HashMap<usize, Vec<SymbolId>>,
        always_accept: Vec<SymbolId>,
    ) -> Result<Self, GrammarError> {
        let terminals: HashMap<SymbolId, TerminalDef> = conf.terminals.iter().cloned().collect();

        // Validate every terminal once, eagerly, by building (and discarding) the
        // full-terminal scanner — the per-state scanners are built lazily on first
        // use, and a grammar whose terminals the lexer refuses (the categorized
        // lookaround scope errors, `docs/LOOKAROUND_SCOPE.md`) must still fail at
        // construction time, not mid-parse. Python Lark's `ContextualLexer` does
        // the same: its eager `root_lexer` init validates every terminal. Pinned by
        // `tests/test_lookaround_scope.rs::scoreboard_rejects_every_case_with_its_category`
        // (every scope case through `Lark::new` on LALR × contextual).
        //
        // This refuses exactly what the per-state builds would have refused: the
        // loader prunes terminals no rule or `%ignore` references (its
        // `_remove_unused` port), so `conf.terminals` is precisely the union of
        // the state sets plus `always_accept` — there is no "unused but broken"
        // terminal this build newly rejects. The one genuinely new failure
        // surface is a combined-build resource limit (one automaton over the
        // union where the old code built only per-state subsets), which matches
        // the basic lexer's existing behavior on the same set.
        {
            let all: Vec<(SymbolId, &TerminalDef)> =
                conf.terminals.iter().map(|(id, t)| (*id, t)).collect();
            ScannerBackend::build(&all, conf.global_flags, conf.backend)?;
        }

        // The root (fallback) scanner over the full terminal set, built lazily —
        // Python Lark's `root_lexer`. Recovery consults it when a per-state scanner
        // refuses a position (see `next_root_token`); a normal parse never builds it.
        let mut root_ids: Vec<SymbolId> = conf.terminals.iter().map(|(id, _)| *id).collect();
        root_ids.sort_unstable();
        root_ids.dedup();
        let root = LazyScanner {
            term_ids: root_ids,
            cell: std::cell::OnceCell::new(),
        };

        let mut key_to_idx: HashMap<Vec<SymbolId>, usize> = HashMap::new();
        let mut scanners: Vec<LazyScanner> = Vec::new();
        let mut state_to_scanner: Vec<u32> = Vec::new();
        for (state_id, valid_ids) in state_terminals {
            let mut ids: Vec<SymbolId> = valid_ids
                .iter()
                .chain(always_accept.iter())
                .filter(|id| terminals.contains_key(id))
                .copied()
                .collect();
            ids.sort_unstable();
            ids.dedup();
            if ids.is_empty() {
                continue;
            }
            let idx = *key_to_idx.entry(ids.clone()).or_insert_with(|| {
                scanners.push(LazyScanner {
                    term_ids: ids,
                    cell: std::cell::OnceCell::new(),
                });
                scanners.len() - 1
            });
            if state_to_scanner.len() <= *state_id {
                state_to_scanner.resize(*state_id + 1, u32::MAX);
            }
            state_to_scanner[*state_id] = idx as u32;
        }

        Ok(ContextualLexer {
            state_to_scanner,
            scanners,
            root,
            terminals,
            global_flags: conf.global_flags,
            backend: conf.backend,
            names: conf.names(),
            ignore: dense_ignore(conf),
        })
    }

    #[inline]
    pub fn is_ignored(&self, id: SymbolId) -> bool {
        self.ignore.get(id.index()).copied().unwrap_or(false)
    }

    /// Build a [`Token`] for a `(id, value)` the scanner matched starting at byte
    /// offset `pos` / **character** index `char_pos`, with newline-aware end
    /// position. Shared by the per-state [`Self::next_token`] and the root-fallback
    /// [`Self::next_root_token`] so the two cannot drift on position bookkeeping.
    ///
    /// `start_pos`/`end_pos` carry the **character** indices (`char_pos`), matching
    /// Python Lark (#278, bounty N8); the byte offset `pos` is only the scanner
    /// cursor and never reaches the token.
    fn build_token(
        &self,
        id: SymbolId,
        value: &str,
        char_pos: usize,
        line: usize,
        col: usize,
        mode: TokenValueMode,
    ) -> Token {
        let (mut end_line, mut end_column) = (line, col);
        let mut nchars = 0usize;
        for ch in value.chars() {
            if ch == '\n' {
                end_line += 1;
                end_column = 1;
            } else {
                end_column += 1;
            }
            nchars += 1;
        }
        Token {
            type_id: id,
            // Span (value-less) tokens skip the name clone too: the span/tape
            // builders resolve names lazily from `type_id`, so the owned `type_`
            // is dead weight there — 1 alloc per token, including ignored ones
            // (perf spike 2026-07-02).
            type_: match mode {
                TokenValueMode::Owned => self.names[id.index()].clone(),
                TokenValueMode::Span => String::new(),
            },
            value: token_value(value, mode),
            line: checked_pos(line),
            column: checked_pos(col),
            end_line: checked_pos(end_line),
            end_column: checked_pos(end_column),
            start_pos: checked_pos(char_pos),
            end_pos: checked_pos(char_pos + nchars),
        }
    }

    /// Lex the next token at byte offset `pos` / character index `char_pos` given the
    /// current parser state. `mode` selects owned vs. value-less span tokens (C8.1
    /// #582); [`Self::next_token`] is the owned default, [`Self::next_token_span`] the
    /// span-emitting entry point.
    ///
    /// Returns the token together with its matched **byte length** (the scanner-
    /// cursor advance distance), so a caller can move past the token in O(1) using
    /// the end position the token already carries — [`build_token`](Self::build_token)
    /// walked the matched text once for line/col bookkeeping; without the byte
    /// length the caller would have to re-walk the same text
    /// ([`LexerState::advance_by_chars`]) just to re-derive it (perf spike
    /// 2026-07-02, the double per-token char scan). `$END` reports length 0.
    pub(crate) fn next_token_with_mode(
        &self,
        text: &str,
        pos: usize,
        char_pos: usize,
        state: usize,
        line: usize,
        col: usize,
        mode: TokenValueMode,
    ) -> Result<Option<(Token, usize)>, ParseError> {
        let slot = match self.state_to_scanner.get(state).copied() {
            Some(idx) if idx != u32::MAX => idx,
            // No scanner recorded for this state: fall back to state 0's
            // scanner (the pre-spike `HashMap` fallback, preserved verbatim).
            _ => match self.state_to_scanner.first().copied() {
                Some(idx) if idx != u32::MAX => idx,
                _ => return Ok(None),
            },
        };
        let scanner = self.scanners[slot as usize].get_or_build(
            &self.terminals,
            self.global_flags,
            self.backend,
        );

        if let Some((id, value)) = scanner.match_at(text, pos) {
            let nbytes = value.len();
            return Ok(Some((
                self.build_token(id, value, char_pos, line, col, mode),
                nbytes,
            )));
        }

        if pos >= text.len() {
            return Ok(Some((
                Token::end().with_position(line, col, char_pos, char_pos),
                0,
            )));
        }

        let ch = text[pos..].chars().next().unwrap();
        Err(ParseError::UnexpectedCharacter {
            ch,
            line,
            col,
            pos,
            expected: "valid token for this state".to_string(),
        })
    }

    /// The owned-value default path (byte-identical to before): materializes
    /// `Token.value` and charges the lexer-side value counter.
    pub fn next_token(
        &self,
        text: &str,
        pos: usize,
        char_pos: usize,
        state: usize,
        line: usize,
        col: usize,
    ) -> Result<Option<Token>, ParseError> {
        Ok(self
            .next_token_with_mode(text, pos, char_pos, state, line, col, TokenValueMode::Owned)?
            .map(|(tok, _)| tok))
    }

    /// The span-emitting path (C8.1 #582): builds a **value-less** token (empty
    /// `Token.value`, no owned-value allocation, no lexer counter charge). The
    /// borrowed text is recovered from the parse `input` in the span builder. Byte-for-byte
    /// the same token as [`Self::next_token`] except for the empty `value`.
    pub(crate) fn next_token_span(
        &self,
        text: &str,
        pos: usize,
        char_pos: usize,
        state: usize,
        line: usize,
        col: usize,
    ) -> Result<Option<Token>, ParseError> {
        Ok(self
            .next_token_with_mode(text, pos, char_pos, state, line, col, TokenValueMode::Span)?
            .map(|(tok, _)| tok))
    }

    /// Match the next token at `pos` against the **root** (full) terminal set —
    /// Python Lark's `ContextualLexer.lex` exception branch, which falls back to
    /// `root_lexer.next_token` when the per-state lexer refuses a position. Used
    /// only during error recovery (see the contextual recovery source): a `Some`
    /// here means the input at `pos` is an out-of-context-but-valid token (the
    /// parser will surface it as an `UnexpectedToken` the recovery loop can delete),
    /// while `None` means even the full set can't lex `pos` — a genuinely un-lexable
    /// character to be skipped. Unlike [`Self::next_token`] this never returns an
    /// `$END` token or an error; the caller handles both end-of-input and the miss.
    pub fn next_root_token(
        &self,
        text: &str,
        pos: usize,
        char_pos: usize,
        line: usize,
        col: usize,
    ) -> Option<Token> {
        let scanner = self
            .root
            .get_or_build(&self.terminals, self.global_flags, self.backend);
        scanner.match_at(text, pos).map(|(id, value)| {
            self.build_token(id, value, char_pos, line, col, TokenValueMode::Owned)
        })
    }
}

// ─── LexerState: tracks position during incremental lexing ───────────────────

/// Mutable state threaded through contextual lexing.
///
/// `pos` is the **byte** offset (drives regex slicing); `char_pos` is the
/// **character** index that a token's `start_pos`/`end_pos` carry (Python parity,
/// #278). Both advance together in [`advance_by`](Self::advance_by).
pub struct LexerState<'a> {
    pub text: &'a str,
    pub pos: usize,
    pub char_pos: usize,
    pub line: usize,
    pub col: usize,
}

impl<'a> LexerState<'a> {
    pub fn new(text: &'a str) -> Self {
        LexerState {
            text,
            pos: 0,
            char_pos: 0,
            line: 1,
            col: 1,
        }
    }

    pub fn is_done(&self) -> bool {
        self.pos >= self.text.len()
    }

    /// Advance `n` bytes, walking the consumed text so line/col and the character
    /// index stay newline-aware (columns count characters, not bytes).
    pub fn advance_by(&mut self, n: usize) {
        for ch in self.text[self.pos..self.pos + n].chars() {
            if ch == '\n' {
                self.line += 1;
                self.col = 1;
            } else {
                self.col += 1;
            }
            self.char_pos += 1;
        }
        self.pos += n;
    }

    /// Advance past the next `nchars` characters, walking the consumed text so byte
    /// `pos` and line/col stay in sync. The value-length-independent analog of
    /// [`advance_by`](Self::advance_by): a span-emitting lexer path (C8.1 #582) builds
    /// value-less tokens whose byte length is unavailable from `Token.value`, so the
    /// [`TokenSource`](crate::parsers::TokenSource) advances by the token's char count
    /// (`end_pos - start_pos`) instead. Identical net effect to `advance_by(value.len())`
    /// for an owned token, since a token spans exactly `end_pos - start_pos` characters.
    pub fn advance_by_chars(&mut self, nchars: usize) {
        for _ in 0..nchars {
            let ch = match self.text[self.pos..].chars().next() {
                Some(ch) => ch,
                None => break,
            };
            if ch == '\n' {
                self.line += 1;
                self.col = 1;
            } else {
                self.col += 1;
            }
            self.char_pos += 1;
            self.pos += ch.len_utf8();
        }
    }

    /// O(1) cursor advance past a just-lexed token (perf spike 2026-07-02): the
    /// token already carries its newline-aware end position — `build_token`
    /// walked the matched text exactly once — so re-walking the same bytes
    /// ([`advance_by_chars`](Self::advance_by_chars)) only re-derives what the
    /// token knows. `nbytes` is the matched byte length the scanner reported
    /// ([`ContextualLexer::next_token_with_mode`]). Identical net effect to
    /// `advance_by_chars(end_pos - start_pos)` for any token the contextual
    /// lexer built at the current cursor.
    #[inline]
    pub(crate) fn advance_past(&mut self, tok: &Token, nbytes: usize) {
        debug_assert_eq!(tok.start_pos as usize, self.char_pos);
        self.pos += nbytes;
        self.char_pos = tok.end_pos as usize;
        self.line = tok.end_line as usize;
        self.col = tok.end_column as usize;
    }
}
