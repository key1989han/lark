//! Interactive LALR parser (issues #168, #222).
//!
//! A driveable parser: feed tokens one at a time, inspect which terminals the
//! parser would accept next, fork an independent cursor, and resume automated
//! parsing. It ports the **oracle-backed subset** of Python Lark's
//! `InteractiveParser` (`lark/parsers/lalr_interactive_parser.py`) — `feed_token`,
//! `accepts`, `feed_eof`, `exhaust_lexer`, `resume`, `copy` (here `fork`),
//! `pretty`, `result` — plus one ergonomic wrapper, [`feed`](InteractiveParser::feed)
//! `(name, value)` over `feed_token`. The shared operations are differentially
//! tested against Python (`tests/test_interactive.rs`).
//!
//! It is a *view* onto the shared state machine: every mutation goes through
//! [`ParserStack::feed_token`](super::lalr::ParserStack::feed_token), the same
//! reduce/shift loop the batch and recovering drivers use (ADR-0015) — there is no
//! second parser here.
//!
//! **Lazy lexing.** Like Python, the lexer is driven *as the caller drives the
//! parser*, not up front: `parse_interactive` over broken editor text succeeds, and
//! an un-lexable character surfaces only when `exhaust_lexer`/`resume` reaches it.
//! Manual `feed`/`feed_token` inject caller-supplied tokens and ignore the lexer.
//!
//! **Two lexer paths** (issue #222): the basic lexer lexes with the global terminal
//! set (all terminals at every position); the contextual lexer narrows terminals
//! by the live parser state (via `ContextualLexer::next_token`) and falls back to
//! the root (full-terminal) scanner on a per-state miss — exactly the machinery
//! the contextual recovery source uses (#166). A grammar whose contextual lexer
//! is load-bearing (overlapping terminals disambiguated only by state) must use
//! the contextual path; the basic path would mis-tokenize under `exhaust_lexer`.

use crate::error::ParseError;
use crate::grammar::intern::SymbolId;
use crate::lexer::{BasicLexer, ContextualLexer};
use crate::tree::{ParseTree, Token};

use super::lalr::{Feed, LalrParser, ParserStack};

/// Which lexer drives the lazy `exhaust_lexer`/`resume` path.
enum LexerKind<'a> {
    /// Basic (global) lexer: lexes one token at a time with all terminals.
    Basic(&'a BasicLexer),
    /// Contextual lexer: narrows terminals by the current parser state (#222).
    Contextual(&'a ContextualLexer),
}

impl Clone for LexerKind<'_> {
    fn clone(&self) -> Self {
        match self {
            LexerKind::Basic(l) => LexerKind::Basic(l),
            LexerKind::Contextual(l) => LexerKind::Contextual(l),
        }
    }
}

/// A driveable LALR parse in progress. Obtained from
/// [`Lark::parse_interactive`](crate::Lark::parse_interactive).
///
/// Borrows the parser (and lexer) it was created from, so it lives no longer than
/// the [`Lark`](crate::Lark). The raw state/value stacks are deliberately *not*
/// public — callers drive the machine through [`feed`](Self::feed) /
/// [`accepts`](Self::accepts), never by poking the stack.
pub struct InteractiveParser<'a> {
    parser: &'a LalrParser,
    lexer: LexerKind<'a>,
    stack: ParserStack,
    /// Owned input, lexed lazily from a hand-tracked cursor (avoids a
    /// self-referential borrow of a `LexerState`). `line`/`col` are 1-based to match
    /// [`LexerState`](crate::lexer::LexerState). `pos` is the **byte** offset (drives
    /// regex slicing); `char_pos` is the **character** index a token's
    /// `start_pos`/`end_pos` carry (Python parity, #278) — they diverge on non-ASCII
    /// input, so the cursor must advance `pos` by byte length, never by `end_pos`.
    text: String,
    pos: usize,
    char_pos: usize,
    line: usize,
    col: usize,
    /// The finished tree once a `$END` feed reached ACCEPT (Python's `.result`).
    result: Option<ParseTree>,
}

impl<'a> InteractiveParser<'a> {
    /// Build an interactive parser over the **basic** lexer (issue #168).
    pub(crate) fn new_basic(
        parser: &'a LalrParser,
        lexer: &'a BasicLexer,
        stack: ParserStack,
        text: String,
    ) -> Self {
        InteractiveParser {
            parser,
            lexer: LexerKind::Basic(lexer),
            stack,
            text,
            pos: 0,
            char_pos: 0,
            line: 1,
            col: 1,
            result: None,
        }
    }

    /// Build an interactive parser over the **contextual** lexer (issue #222).
    pub(crate) fn new_contextual(
        parser: &'a LalrParser,
        lexer: &'a ContextualLexer,
        stack: ParserStack,
        text: String,
    ) -> Self {
        InteractiveParser {
            parser,
            lexer: LexerKind::Contextual(lexer),
            stack,
            text,
            pos: 0,
            char_pos: 0,
            line: 1,
            col: 1,
            result: None,
        }
    }

    /// Feed one token, advancing through any REDUCEs to the next SHIFT or ACCEPT.
    /// Returns `Ok(Some(tree))` when this token drove ACCEPT (a `$END`), `Ok(None)`
    /// when it was shifted, and `Err` (the same `UnexpectedToken` a batch parse would
    /// raise) when the parser has no action for it. Mirrors Python's `feed_token`.
    ///
    /// **Dispatch is by terminal *name*** (`token.type_`), exactly as Python's
    /// `ParserState.feed_token` indexes `states[state][token.type]` — including
    /// `$END`, which is interned under that name. A caller-built
    /// `Token::new("NUMBER", "1")` therefore Just Works, and a foreign or mutated
    /// token's numeric `type_id` is **not trusted**: it is re-resolved by name.
    ///
    /// Once the parse has reached ACCEPT (`result().is_some()`) it is **finished**:
    /// further feeds error (with an empty expected set, matching `accepts() == []`).
    pub fn feed_token(&mut self, mut token: Token) -> Result<Option<ParseTree>, ParseError> {
        if self.result.is_some() {
            // Finished: nothing is acceptable after ACCEPT (see `accepts`).
            return Err(ParseError::unexpected_token(&token, Vec::new()));
        }
        match self.parser.table.symbols.id(&token.type_) {
            Some(id) => token.type_id = id,
            None => {
                return Err(ParseError::UnexpectedToken {
                    token: token.value.clone(),
                    token_type: token.type_.clone(),
                    line: token.line as usize,
                    col: token.column as usize,
                    expected: self.accepts(),
                })
            }
        }
        match self.stack.feed_token(&self.parser.table, &token) {
            Feed::Shifted => Ok(None),
            Feed::Accepted(tree) => {
                self.result = Some(tree.clone());
                Ok(Some(tree))
            }
            Feed::Error(e) => Err(e),
            Feed::NoAction => Err(self.parser.unexpected(self.stack.position(), &token)),
        }
    }

    /// Build and feed a token by terminal *name* — the form [`accepts`](Self::accepts)
    /// returns. A thin ergonomic wrapper over [`feed_token`](Self::feed_token)
    /// (`feed("NUMBER", "1")` == `feed_token(Token::new("NUMBER", "1"))`); the name is
    /// resolved by `feed_token`.
    pub fn feed(&mut self, terminal: &str, value: &str) -> Result<Option<ParseTree>, ParseError> {
        self.feed_token(Token::new(terminal, value))
    }

    /// The terminal names that would advance the parser from the current state,
    /// sorted and deterministic — the primary oracle comparand. Mirrors Python's
    /// `accepts()` (computed value-free here: only the state stack is simulated).
    /// Empty once the parse is **finished** (`result().is_some()`): after ACCEPT
    /// nothing more can be fed, so reporting `$END` as acceptable would be dishonest.
    pub fn accepts(&self) -> Vec<String> {
        if self.result.is_some() {
            return Vec::new();
        }
        self.stack.accepts(&self.parser.table)
    }

    /// Feed a synthetic `$END`, finishing the parse. Returns the tree if ACCEPT was
    /// reached. Mirrors Python's `feed_eof`.
    ///
    /// The `$END` position comes from the **lazy lexer cursor** (where lexing left
    /// off), *not* from the last manually-fed token — so after `exhaust_lexer` it is
    /// the end of input, and before any lexer drive it is the start (`1,1`).
    pub fn feed_eof(&mut self) -> Result<Option<ParseTree>, ParseError> {
        self.feed_token(self.eof_token())
    }

    /// Feed the rest of the token stream, **without** a `$END`; returns the tokens
    /// consumed. An un-lexable character raises here (Python's lazy
    /// `UnexpectedCharacters`), not at construction. Mirrors `exhaust_lexer`.
    ///
    /// For the **contextual** lexer (#222), each token is lexed at the *current*
    /// parser state (via `ContextualLexer::next_token`), narrowing to the terminals
    /// valid in that state — with root-lexer fallback on a per-state miss, exactly
    /// as the batch contextual parse does. The parser state changes after each
    /// `feed_token`, so the *next* lex uses the updated state.
    pub fn exhaust_lexer(&mut self) -> Result<Vec<Token>, ParseError> {
        let mut fed = Vec::new();
        loop {
            let token = self.next_lexed()?;
            if token.type_id == SymbolId::END {
                break; // never feed `$END` here; the cursor sits at end for feed_eof
            }
            let echo = token.clone();
            self.feed_token(token)?;
            fed.push(echo);
        }
        Ok(fed)
    }

    /// Resume fully-automated parsing to completion: feed the rest of the lexer, then
    /// a `$END`. Returns the finished tree. Mirrors Python's `resume_parse`.
    ///
    /// Consumes the cursor (`self`), since after resuming to `$END` there is nothing
    /// more to drive. Fork first (`p.fork().resume()`) if you need the cursor back.
    pub fn resume(mut self) -> Result<ParseTree, ParseError> {
        self.exhaust_lexer()?;
        match self.feed_eof()? {
            Some(tree) => Ok(tree),
            None => Err(ParseError::unexpected_eof(self.line, self.col, vec![])),
        }
    }

    /// An independent cursor: feeds on the fork do not affect this one, or vice-versa.
    /// Mirrors Python's `copy()`. Cheap — `accepts()` already avoids cloning tree
    /// values; this clones the value stack only on an explicit branch.
    pub fn fork(&self) -> InteractiveParser<'a> {
        InteractiveParser {
            parser: self.parser,
            lexer: self.lexer.clone(),
            stack: self.stack.clone(),
            text: self.text.clone(),
            pos: self.pos,
            char_pos: self.char_pos,
            line: self.line,
            col: self.col,
            result: self.result.clone(),
        }
    }

    /// The finished tree once a `$END` feed reached ACCEPT (Python's `.result`).
    pub fn result(&self) -> Option<&ParseTree> {
        self.result.as_ref()
    }

    /// A short debug rendering of the current state and accepted terminals (Python's
    /// `pretty()` renders `choices()`; we render `accepts()` — debug only, not
    /// oracle-pinned).
    pub fn pretty(&self) -> String {
        format!(
            "InteractiveParser(state {}, accepts {:?})",
            self.stack.position(),
            self.accepts()
        )
    }

    // ─── Lazy lexer cursor ───────────────────────────────────────────────────

    /// The synthetic `$END` token at the current cursor (its position is where lexing
    /// left off — after `exhaust_lexer`, the end of input; before any drive, the
    /// start).
    fn eof_token(&self) -> Token {
        // `$END` carries the **character** index (Python parity, #278), not the byte
        // offset.
        Token::end().with_position(self.line, self.col, self.char_pos, self.char_pos)
    }

    /// Lex the next non-ignored token, advancing the cursor, or the positioned `$END`
    /// at end of input. `Err(UnexpectedCharacter)` at an un-lexable character.
    ///
    /// For the **contextual** lexer (#222), lexes at the current parser state via
    /// `ContextualLexer::next_token`, with root-lexer fallback on a per-state miss.
    /// For the **basic** lexer, lexes the global terminal set via
    /// `BasicLexer::next_token_at`.
    fn next_lexed(&mut self) -> Result<Token, ParseError> {
        loop {
            if self.pos >= self.text.len() {
                return Ok(self.eof_token());
            }
            match &self.lexer {
                LexerKind::Basic(lexer) => {
                    match lexer.next_token_at(
                        &self.text,
                        self.pos,
                        self.char_pos,
                        self.line,
                        self.col,
                    ) {
                        Ok(token) => {
                            // `end_pos` is a **char** index now (#278); advance the
                            // byte cursor by the matched span's byte length, the char
                            // cursor to `end_pos`.
                            self.pos += token.value.len();
                            self.char_pos = token.end_pos as usize;
                            self.line = token.end_line as usize;
                            self.col = token.end_column as usize;
                            if lexer.is_ignored(token.type_id) {
                                continue;
                            }
                            return Ok(token);
                        }
                        Err(()) => {
                            let ch = self.text[self.pos..].chars().next().unwrap();
                            return Err(ParseError::UnexpectedCharacter {
                                ch,
                                line: self.line,
                                col: self.col,
                                pos: self.pos,
                                expected: "any token".to_string(),
                            });
                        }
                    }
                }
                LexerKind::Contextual(lexer) => {
                    let parser_state = self.stack.position();
                    match lexer.next_token(
                        &self.text,
                        self.pos,
                        self.char_pos,
                        parser_state,
                        self.line,
                        self.col,
                    ) {
                        // Ignored terminal (whitespace, comment): advance and continue.
                        Ok(Some(token)) if lexer.is_ignored(token.type_id) => {
                            self.pos += token.value.len();
                            self.char_pos = token.end_pos as usize;
                            self.line = token.end_line as usize;
                            self.col = token.end_column as usize;
                        }
                        Ok(Some(token)) => {
                            self.pos += token.value.len();
                            self.char_pos = token.end_pos as usize;
                            self.line = token.end_line as usize;
                            self.col = token.end_column as usize;
                            return Ok(token);
                        }
                        // Per-state miss: try the root (full-terminal) scanner as a
                        // fallback, exactly as the contextual recovery source does
                        // (#166). A root match is an out-of-context-but-valid token
                        // we surface to the caller. A root miss is un-lexable.
                        //
                        // Root-fallback error semantics (#250, pinned by
                        // `test_interactive_error_oracle`). This mirrors
                        // `ContextualRecovering`, not `Contextual` — but it is **not**
                        // more permissive than a batch contextual parse. The fallback
                        // only decides *which lexer* surfaces a token; the parser's
                        // action table remains the sole authority on validity. So a
                        // globally-valid but state-invalid token (e.g. `}` in `a_part`
                        // state) is matched by the root scanner, fed, and rejected with
                        // the *same* `UnexpectedToken` the batch contextual parse raises
                        // (verified against Python Lark, whose contextual lexer likewise
                        // falls back to the full terminal set). A genuinely unlexable
                        // character misses even the root set and surfaces
                        // `UnexpectedCharacter`, again as batch does.
                        Ok(None) | Err(_) => {
                            if let Some(token) = lexer.next_root_token(
                                &self.text,
                                self.pos,
                                self.char_pos,
                                self.line,
                                self.col,
                            ) {
                                self.pos += token.value.len();
                                self.char_pos = token.end_pos as usize;
                                self.line = token.end_line as usize;
                                self.col = token.end_column as usize;
                                if lexer.is_ignored(token.type_id) {
                                    continue;
                                }
                                return Ok(token);
                            }
                            let ch = self.text[self.pos..].chars().next().unwrap();
                            return Err(ParseError::UnexpectedCharacter {
                                ch,
                                line: self.line,
                                col: self.col,
                                pos: self.pos,
                                expected: "any token".to_string(),
                            });
                        }
                    }
                }
            }
        }
    }
}
