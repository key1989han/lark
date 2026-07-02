use thiserror::Error;

use crate::tree::ParseTree;

#[derive(Debug, Error)]
pub enum LarkError {
    #[error("Grammar error: {0}")]
    Grammar(#[from] GrammarError),
    #[error("Parse error: {0}")]
    Parse(#[from] ParseError),
}

#[derive(Debug, Error, Clone)]
pub enum GrammarError {
    #[error("Undefined terminal: {name}")]
    UndefinedTerminal { name: String },
    #[error("Undefined rule: {name}")]
    UndefinedRule { name: String },
    #[error("Duplicate definition: {name}")]
    DuplicateDefinition { name: String },
    #[error("Invalid regex pattern '{pattern}': {reason}")]
    InvalidRegex { pattern: String, reason: String },
    #[error("Grammar syntax error at line {line}, column {col}: {msg}")]
    SyntaxError {
        line: usize,
        col: usize,
        msg: String,
    },
    #[error("Import not found: {path}")]
    ImportNotFound { path: String },
    #[error("Grammar has unresolvable LALR conflicts:\n{report}")]
    Conflict { report: String },
    /// A lookaround terminal the lexer refused, categorized by the two-category scope
    /// taxonomy (`docs/LOOKAROUND_SCOPE.md`): [`Scope::OutOfScope`] is a by-design
    /// non-goal, [`Scope::NotYetImplemented`] a conservative rejection of an
    /// in-principle-lowerable pattern. The typed fields are the scope scoreboard's
    /// contract (`tests/test_lookaround_scope.rs`); `msg` is the user-facing text
    /// built by `classify::scope_message`.
    ///
    /// [`Scope::OutOfScope`]: crate::lookaround::classify::Scope::OutOfScope
    /// [`Scope::NotYetImplemented`]: crate::lookaround::classify::Scope::NotYetImplemented
    #[error("{msg}")]
    LookaroundScope {
        /// The terminal's name.
        terminal: String,
        /// The offending assertion source (rejections) or the pattern (declines).
        subject: String,
        scope: crate::lookaround::classify::Scope,
        issue: crate::lookaround::classify::LookaroundIssue,
        msg: String,
    },
    #[error("{msg}")]
    Other { msg: String },
}

#[derive(Debug, Error, Clone)]
pub enum ParseError {
    #[error("Unexpected character {ch:?} at line {line}, column {col}\nExpected: {expected}")]
    UnexpectedCharacter {
        ch: char,
        line: usize,
        col: usize,
        pos: usize,
        expected: String,
    },
    #[error(
        "Unexpected token {token:?} at line {line}, column {col}\nExpected one of: {expected:?}"
    )]
    UnexpectedToken {
        token: String,
        token_type: String,
        line: usize,
        col: usize,
        expected: Vec<String>,
    },
    #[error("Unexpected end of input at line {line}, column {col}\nExpected: {expected:?}")]
    UnexpectedEof {
        line: usize,
        col: usize,
        expected: Vec<String>,
    },
    /// A postlex hook (e.g. an [`Indenter`](crate::postlex::Indenter)) rejected the
    /// token stream — most commonly a dedent that does not match any open
    /// indentation level (Python Lark's `DedentError`).
    #[error("Postlex error: {msg}")]
    Postlex { msg: String },
}

/// What an `on_error` recovery handler wants to do after inspecting the error
/// (issue #223).
///
/// Replaces the old `bool` return (`true` = delete, `false` = stop) with explicit
/// semantics: the handler can delete the offending token, resume after feeding
/// corrective tokens into the [`RecoveryContext`], or stop recovery entirely.
///
/// [`RecoveryContext`]: crate::parsers::lalr::RecoveryContext
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryAction {
    /// Delete the offending token and retry the next one in the same parser
    /// state — the old `true` behavior (single-token-deletion recovery).
    Delete,
    /// The handler has fed corrective tokens through the [`RecoveryContext`],
    /// advancing the parser state. The errored token is **dropped** (matching
    /// Python Lark's `resume_parse()`) and the *next* token is parsed in the
    /// handler's new state. At `$END`, the sentinel is retried (there is no
    /// next token). A no-progress guard prevents infinite loops: if the handler
    /// did not feed any tokens, the recovery loop treats it as `Stop`.
    ///
    /// [`RecoveryContext`]: crate::parsers::lalr::RecoveryContext
    Resume,
    /// Stop recovery — no derivation is produced (`tree: None`). The old
    /// `false` behavior.
    Stop,
}

/// The result of parsing with error recovery enabled (issue #43).
///
/// Rather than aborting on the first parse error, the recovering driver deletes
/// the offending token(s) and continues, so an editor / LSP gets *both* a tree to
/// work with and the diagnostics to surface. The two halves are:
///
///   * `tree` — `Some(tree)` only when recovery reached a normal ACCEPT (the
///     surviving tokens form a valid parse); this is a *real* derivation, identical
///     to what Python Lark's `parse(text, on_error=lambda e: True)` returns. When
///     recovery could **not** reach ACCEPT — a premature end of input (`$END`), or
///     `on_error` returning `false` mid-parse — it is `None`. lark-rs deliberately
///     does **not** fabricate a partial here (it once wrapped leftover value-stack
///     fragments under the start-symbol name, which is not a real derivation and a
///     caller cannot tell apart from a clean parse). `None` keeps the result
///     honest and matches Python's `recovered: false` behavior at `$END`, where
///     Python re-raises rather than returning a tree (issue #167; ADR-0019).
///   * `errors` — every error recovered from, in source order. These are the
///     "error nodes": each carries the offending token and its line/column. An
///     empty list means the input parsed cleanly with no recovery. A non-empty
///     `errors` with `tree: None` is the distinguishable partial: the parse failed
///     to complete, and the diagnostics say where.
///
/// lark-rs does not splice error nodes *inline* into the tree: an LR value stack
/// has no symbol/state slot for a synthetic node without a yacc-style `error`
/// production, which Lark's grammar syntax has no way to express. Surfacing the
/// recovered errors alongside the partial tree mirrors Python Lark's own model
/// exactly (its `on_error` recovery likewise drops the bad tokens from the tree
/// and leaves the caller to collect the errors).
#[derive(Debug, Clone)]
pub struct RecoveredTree {
    /// The recovered derivation, or `None` when recovery could not reach ACCEPT
    /// (premature `$END`, or `on_error` stopping the parse). See the type docs.
    pub tree: Option<ParseTree>,
    pub errors: Vec<ParseError>,
}

impl ParseError {
    /// The parser met a token it cannot act on. Builds an [`UnexpectedToken`]
    /// carrying the token's position and the caller-supplied `expected` set — or,
    /// when the token is the synthetic end-of-input terminal, the equivalent
    /// [`UnexpectedEof`]. The one constructor every backend funnels its
    /// bad-token reports through, so the END-vs-token split and the field
    /// shapes cannot drift between them. What `expected` contains stays the
    /// backend's call: LALR fills it from the parse table's action row; Earley
    /// and CYK have no comparable per-state set and pass an empty list.
    ///
    /// [`UnexpectedToken`]: ParseError::UnexpectedToken
    /// [`UnexpectedEof`]: ParseError::UnexpectedEof
    pub(crate) fn unexpected_token(
        token: &crate::tree::Token,
        expected: Vec<String>,
    ) -> ParseError {
        if token.type_id == crate::grammar::intern::SymbolId::END {
            ParseError::UnexpectedEof {
                line: token.line as usize,
                col: token.column as usize,
                expected,
            }
        } else {
            ParseError::UnexpectedToken {
                token: token.value.clone(),
                token_type: token.type_.clone(),
                line: token.line as usize,
                col: token.column as usize,
                expected,
            }
        }
    }

    /// Build an [`UnexpectedToken`] from a token and `expected` set, with **no**
    /// END→[`UnexpectedEof`] split. Unlike [`unexpected_token`](Self::unexpected_token),
    /// this always reports `UnexpectedToken` even for the synthetic end-of-input
    /// terminal — the recovery driver's `RecoveryContext::feed_token` rejects a fed
    /// `$END` as an unexpected *token* (completion is the recovery loop's job, not
    /// the handler's), so its bad-token reports must not collapse to `UnexpectedEof`.
    /// Centralizes the five identical constructions that path used to inline.
    ///
    /// [`UnexpectedToken`]: ParseError::UnexpectedToken
    /// [`UnexpectedEof`]: ParseError::UnexpectedEof
    pub(crate) fn unexpected_token_keep_end(
        token: &crate::tree::Token,
        expected: Vec<String>,
    ) -> ParseError {
        ParseError::UnexpectedToken {
            token: token.value.clone(),
            token_type: token.type_.clone(),
            line: token.line as usize,
            col: token.column as usize,
            expected,
        }
    }

    /// Unexpected end of input at a position (`0, 0` when no position is known —
    /// e.g. an unresolvable start symbol or CYK's uniform "parsing failed").
    pub(crate) fn unexpected_eof(line: usize, col: usize, expected: Vec<String>) -> ParseError {
        ParseError::UnexpectedEof {
            line,
            col,
            expected,
        }
    }

    /// Format a snippet of the input around the error position.
    pub fn get_context(text: &str, pos: usize, span: usize) -> String {
        let start = pos.saturating_sub(span);
        let end = (pos + span).min(text.len());
        let snippet = &text[start..end];
        let arrow_pos = pos - start;
        format!("{}\n{}^", snippet, " ".repeat(arrow_pos))
    }
}
