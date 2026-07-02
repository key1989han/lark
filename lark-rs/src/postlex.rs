//! Post-lexer hooks: stream transforms applied between the lexer and the parser.
//!
//! A postlex hook receives the full token stream the lexer produced and yields a
//! transformed stream before it reaches the parser. The canonical use is Python's
//! significant-whitespace syntax: an [`Indenter`] watches newline tokens and
//! injects synthetic `INDENT` / `DEDENT` tokens so an otherwise context-free
//! grammar can express block structure.
//!
//! The injected tokens are `%declare`d terminals (they have no lexer pattern of
//! their own — see [`crate::grammar::terminal::TerminalDef::declared`]). The
//! postlex resolves their interned ids from the [`SymbolTable`] so the parser
//! dispatches on them exactly as it would a lexed token.
//!
//! This is a direct port of Python Lark's `lark.indenter.Indenter`; the algorithm
//! (paren tracking, tab expansion, end-of-stream dedent flush) matches it
//! token-for-token so the oracle trees are identical.

use std::collections::VecDeque;

use crate::error::{GrammarError, ParseError};
use crate::grammar::intern::{SymbolId, SymbolTable};
use crate::tree::{PosInt, Token};

/// The subset of a [`Token`]'s position a synthetic token borrows. Copied off the
/// triggering token so the hot loop never clones a whole `Token` just to remember
/// "where the last one was" for the end-of-stream DEDENT flush.
#[derive(Clone, Copy)]
struct Pos {
    line: PosInt,
    column: PosInt,
    end_line: PosInt,
    end_column: PosInt,
    start_pos: PosInt,
    end_pos: PosInt,
}

impl Pos {
    fn of(t: &Token) -> Self {
        Pos {
            line: t.line,
            column: t.column,
            end_line: t.end_line,
            end_column: t.end_column,
            start_pos: t.start_pos,
            end_pos: t.end_pos,
        }
    }
}

/// Injects `INDENT` / `DEDENT` tokens based on indentation, mirroring Python
/// Lark's `Indenter`.
///
/// The terminal *names* are configurable so one `Indenter` covers any grammar:
///   * `nl_type` — the newline terminal whose trailing whitespace measures indent;
///   * `indent_type` / `dedent_type` — the `%declare`d terminals to inject;
///   * `open_paren_types` / `close_paren_types` — bracket terminals inside which
///     indentation is ignored (no INDENT/DEDENT is emitted while nested);
///   * `tab_len` — how many columns a tab counts as.
#[derive(Debug, Clone)]
pub struct Indenter {
    pub nl_type: String,
    pub open_paren_types: Vec<String>,
    pub close_paren_types: Vec<String>,
    pub indent_type: String,
    pub dedent_type: String,
    pub tab_len: usize,
}

impl Default for Indenter {
    /// Python Lark's `PythonIndenter` defaults.
    fn default() -> Self {
        Indenter {
            nl_type: "_NEWLINE".to_string(),
            open_paren_types: vec!["LPAR".into(), "LSQB".into(), "LBRACE".into()],
            close_paren_types: vec!["RPAR".into(), "RSQB".into(), "RBRACE".into()],
            indent_type: "_INDENT".to_string(),
            dedent_type: "_DEDENT".to_string(),
            tab_len: 8,
        }
    }
}

impl Indenter {
    /// Check, at build time, that every terminal name this Indenter *requires*
    /// resolves to a grammar symbol — closing the silent-misparse footgun where a
    /// typo'd `nl_type` (or an undeclared INDENT/DEDENT) turns the hook into a
    /// no-op and the grammar quietly mis-parses instead of erroring.
    ///
    /// `open_paren_types` / `close_paren_types` are intentionally *not* required to
    /// exist: Python Lark treats an unknown bracket name as simply never matching,
    /// and the built-in defaults (`LPAR`/`LSQB`/`LBRACE`) name brackets a given
    /// grammar may legitimately not define. Validating them would reject those
    /// valid setups, so we match Python's leniency there.
    pub fn validate(&self, symbols: &SymbolTable) -> Result<(), GrammarError> {
        if symbols.id(&self.nl_type).is_none() {
            return Err(GrammarError::Other {
                msg: format!(
                    "postlex Indenter nl_type {:?} is not a terminal in the grammar — \
                     the Indenter measures indentation off it",
                    self.nl_type
                ),
            });
        }
        for (label, name) in [
            ("indent_type", &self.indent_type),
            ("dedent_type", &self.dedent_type),
        ] {
            if symbols.id(name).is_none() {
                return Err(GrammarError::Other {
                    msg: format!(
                        "postlex Indenter {label} {name:?} is not declared in the grammar \
                         (add `%declare {name}`)"
                    ),
                });
            }
        }
        Ok(())
    }

    /// Transform `tokens` (a fully-lexed stream ending in the synthetic `$END`
    /// token) by injecting INDENT/DEDENT around newline tokens.
    ///
    /// This is the **basic-lexer** path: the whole stream is materialized, so the
    /// indenter runs over a `Vec`. The **contextual lexer** drives the same
    /// [`IndenterStream`] machine one token at a time (issue #67); both inject a
    /// byte-identical INDENT/DEDENT stream because they share `feed`/`finish`.
    ///
    /// `symbols` resolves the `%declare`d `indent_type` / `dedent_type` names to
    /// the interned ids the parser dispatches on; a missing one is a configuration
    /// error (the grammar never declared it).
    pub fn process(
        &self,
        tokens: Vec<Token>,
        symbols: &SymbolTable,
    ) -> Result<Vec<Token>, ParseError> {
        let mut stream = IndenterStream::new(self, symbols)?;
        // The synthetic `$END` is held back so the end-of-stream dedent flush lands
        // *before* it, exactly where Python Lark's generator emits the trailing
        // DEDENTs (after the input loop, before the parser sees end of input).
        let mut end_token: Option<Token> = None;

        for tok in tokens {
            if tok.type_id == SymbolId::END {
                end_token = Some(tok);
                break;
            }
            stream.feed(tok)?;
        }
        stream.finish();

        let mut out: Vec<Token> = stream.out.into_iter().collect();
        out.push(end_token.unwrap_or_else(Token::end));
        Ok(out)
    }

    /// Build a synthetic token, borrowing its position from `pos` (the newline it
    /// was triggered by, or the last token for the EOF flush), like Python Lark's
    /// `Token.new_borrow_pos`. `None` (an empty stream) leaves zeroed positions.
    fn make_token(&self, id: SymbolId, name: &str, value: &str, pos: Option<Pos>) -> Token {
        let mut t = Token::new(name, value);
        t.type_id = id;
        if let Some(p) = pos {
            t.line = p.line;
            t.column = p.column;
            t.end_line = p.end_line;
            t.end_column = p.end_column;
            t.start_pos = p.start_pos;
            t.end_pos = p.end_pos;
        }
        t
    }

    /// Resolve a `%declare`d terminal's interned id. This duplicates the existence
    /// check in [`validate`](Self::validate); when `process` is reached through
    /// `Lark`/`build_frontend` that check has already passed, so this is a
    /// belt-and-suspenders guard for callers that invoke `process` directly.
    fn declared_id(&self, symbols: &SymbolTable, name: &str) -> Result<SymbolId, ParseError> {
        symbols.id(name).ok_or_else(|| ParseError::Postlex {
            msg: format!(
                "Indenter terminal {name:?} is not declared in the grammar (add `%declare {name}`)"
            ),
        })
    }
}

/// The streaming core of the [`Indenter`].
///
/// Both postlex paths drive this one machine so they inject a byte-identical
/// INDENT/DEDENT stream:
///   * the **basic lexer** materializes the whole stream and `Indenter::process`
///     feeds it in one loop;
///   * the **contextual lexer** lexes lazily, so a `TokenSource` adapter feeds
///     real tokens one at a time and drains the injected ones between them
///     (issue #67).
///
/// `feed` consumes one real (non-`$END`) token and appends its postlex result —
/// the token itself (unless swallowed inside parens) plus any INDENT/DEDENT — to
/// the output queue. `finish` flushes the trailing DEDENTs at end of input. `pop`
/// drains the queue front. The machine borrows only the immutable [`Indenter`]
/// config; the interned ids are resolved once in [`new`](IndenterStream::new).
pub(crate) struct IndenterStream<'a> {
    cfg: &'a Indenter,
    indent_id: SymbolId,
    dedent_id: SymbolId,
    paren_level: usize,
    indent_stack: Vec<usize>,
    /// Position of the last real token seen, borrowed onto flushed DEDENTs
    /// (Python's `Token.new_borrow_pos(DEDENT, '', token)`). Only the position is
    /// kept, so the loop never clones a whole `Token` for the EOF flush.
    last_pos: Option<Pos>,
    out: VecDeque<Token>,
    /// Whether this stream drives error recovery (issue #94). It relaxes the
    /// unbalanced-close-bracket guard: Python's `_process` decrements `paren_level`
    /// and asserts `>= 0` *lazily*, on the iteration *after* it yields the close
    /// bracket — but during recovery the parser may delete that bracket and resume
    /// with a *fresh* generator, abandoning the old one *before* the decrement/assert
    /// runs. So an underflowing close bracket is yielded (the parser deletes it) and
    /// its decrement is never observed. We mirror that by saturating `paren_level` at
    /// 0 instead of erroring while recovering; a clean parse keeps the hard error.
    recovering: bool,
}

impl<'a> IndenterStream<'a> {
    pub(crate) fn new(cfg: &'a Indenter, symbols: &SymbolTable) -> Result<Self, ParseError> {
        Self::new_inner(cfg, symbols, false)
    }

    /// As [`new`](Self::new), but for error recovery (issue #94): relaxes the
    /// unbalanced-close-bracket guard (see the `recovering` field) so the streaming
    /// indenter matches Python Lark's generator-abandonment behavior on resume.
    pub(crate) fn new_recovering(
        cfg: &'a Indenter,
        symbols: &SymbolTable,
    ) -> Result<Self, ParseError> {
        Self::new_inner(cfg, symbols, true)
    }

    fn new_inner(
        cfg: &'a Indenter,
        symbols: &SymbolTable,
        recovering: bool,
    ) -> Result<Self, ParseError> {
        let indent_id = cfg.declared_id(symbols, &cfg.indent_type)?;
        let dedent_id = cfg.declared_id(symbols, &cfg.dedent_type)?;
        Ok(IndenterStream {
            cfg,
            indent_id,
            dedent_id,
            paren_level: 0,
            indent_stack: vec![0],
            last_pos: None,
            out: VecDeque::new(),
            recovering,
        })
    }

    /// Reset the indenter to its initial state for an error-recovery *resume*
    /// (issue #94). Python Lark's recovery re-invokes `Indenter.process` on each
    /// `resume_parse`, and `process` resets `indent_level = [0]` and
    /// `paren_level = 0` every call — so after the parser deletes an offending
    /// token, the *fresh* indenter generator continues pulling real tokens from the
    /// lexer's current position but with a cleared indent/paren stack. Reproducing
    /// that reset is what makes recovery over a postlex hook byte-for-byte faithful
    /// (a multi-deletion recovery otherwise diverges: the trailing DEDENTs that
    /// close a block are never re-emitted once the stack keeps resetting). The
    /// output queue is cleared too: any tokens the fresh generator hasn't re-derived
    /// yet must not leak across the resume boundary. The underlying lexer position
    /// is **not** touched — only the indenter's own bookkeeping.
    pub(crate) fn reset_for_resume(&mut self) {
        self.paren_level = 0;
        self.indent_stack.clear();
        self.indent_stack.push(0);
        self.out.clear();
    }

    /// Feed one real (non-`$END`) token. Appends its postlex result to the queue.
    pub(crate) fn feed(&mut self, tok: Token) -> Result<(), ParseError> {
        let cur = Pos::of(&tok);
        self.last_pos = Some(cur);
        // Decide the paren-depth delta from the token's type *before* it may be
        // moved into the queue; apply it *after* yielding, matching Python's order
        // (handle_NL / yield first, then adjust paren_level). A newline is never a
        // bracket, so its delta is always 0.
        let delta: i32 = if self.cfg.open_paren_types.iter().any(|t| t == &tok.type_) {
            1
        } else if self.cfg.close_paren_types.iter().any(|t| t == &tok.type_) {
            -1
        } else {
            0
        };

        if tok.type_ == self.cfg.nl_type {
            self.handle_nl(&tok)?;
        } else {
            self.out.push_back(tok); // moved, not cloned
        }

        match delta {
            1 => self.paren_level += 1,
            -1 => {
                self.paren_level = match self.paren_level.checked_sub(1) {
                    Some(n) => n,
                    // Underflow. During recovery this close bracket has already been
                    // yielded (the parser deletes it and resumes with a fresh
                    // generator), so Python never reaches its lazy `>= 0` assert —
                    // saturate at 0 and carry on. A clean parse keeps the hard error.
                    None if self.recovering => 0,
                    None => {
                        return Err(ParseError::Postlex {
                            msg: format!(
                                "unbalanced closing bracket at line {}, column {}",
                                cur.line, cur.column
                            ),
                        })
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// End of input: close every still-open indentation level, queueing a DEDENT
    /// for each. Must be called exactly once, after the last [`feed`](Self::feed).
    pub(crate) fn finish(&mut self) {
        while self.indent_stack.len() > 1 {
            self.indent_stack.pop();
            self.out.push_back(self.cfg.make_token(
                self.dedent_id,
                &self.cfg.dedent_type,
                "",
                self.last_pos,
            ));
        }
    }

    /// Drain the next ready token, or `None` if the queue is currently empty.
    pub(crate) fn pop(&mut self) -> Option<Token> {
        self.out.pop_front()
    }

    /// Handle a newline token: emit it (unless inside parens), then push INDENT or
    /// pop DEDENT(s) to match the new indentation depth.
    fn handle_nl(&mut self, token: &Token) -> Result<(), ParseError> {
        // Inside brackets indentation is insignificant: swallow the newline whole,
        // exactly as Python Lark's `handle_NL` returns without yielding.
        if self.paren_level > 0 {
            return Ok(());
        }

        self.out.push_back(token.clone());

        // Indentation is the whitespace after the *last* newline in the token.
        let indent_str = token
            .value
            .rsplit_once('\n')
            .map(|(_, after)| after)
            .unwrap_or(&token.value);
        let indent =
            indent_str.matches(' ').count() + indent_str.matches('\t').count() * self.cfg.tab_len;

        let pos = Some(Pos::of(token));
        let top = *self.indent_stack.last().expect("indent stack never empty");
        if indent > top {
            self.indent_stack.push(indent);
            self.out.push_back(self.cfg.make_token(
                self.indent_id,
                &self.cfg.indent_type,
                indent_str,
                pos,
            ));
        } else {
            while indent < *self.indent_stack.last().expect("indent stack never empty") {
                self.indent_stack.pop();
                self.out.push_back(self.cfg.make_token(
                    self.dedent_id,
                    &self.cfg.dedent_type,
                    indent_str,
                    pos,
                ));
            }
            if indent != *self.indent_stack.last().expect("indent stack never empty") {
                return Err(ParseError::Postlex {
                    msg: format!(
                        "Unexpected dedent to column {}. Expected dedent to {}",
                        indent,
                        self.indent_stack.last().unwrap()
                    ),
                });
            }
        }
        Ok(())
    }
}
