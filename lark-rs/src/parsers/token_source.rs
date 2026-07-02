//! The lexer⇄parser interface.
//!
//! A [`TokenSource`] yields the next actionable token for the parser's current
//! lexer context, hiding *how* that token is produced. Two implementations cover
//! the LALR frontends:
//!
//!   * [`PreLexed`] replays a token vector produced up front by the basic lexer.
//!   * [`Contextual`] lexes lazily, one token at a time, asking the contextual
//!     lexer for only the terminals valid in the current parser state.
//!
//! Collapsing both behind one trait lets the LALR driver ([`LalrParser::run`])
//! be a single state-machine loop instead of two near-identical ones, and gives
//! a future Earley driver a ready-made input interface.
//!
//! [`LalrParser::run`]: super::lalr::LalrParser

use crate::error::ParseError;
use crate::grammar::intern::SymbolId;
use crate::lexer::{BasicLexer, ContextualLexer, LexerState, TokenValueMode};
use crate::postlex::{Indenter, IndenterStream};
use crate::tree::Token;

/// The number of characters a token spans (`end_pos - start_pos`, both char indices,
/// #278). Used to advance the lexer cursor independently of `Token.value`, so a
/// span-emitting (value-less) token advances the same distance an owned one would
/// (C8.1 #582).
#[inline]
fn token_char_len(tok: &Token) -> usize {
    (tok.end_pos - tok.start_pos) as usize
}

/// The token source could not tokenize the input at the current position.
///
/// It carries only what the *lexer* knows (the offending character and where it
/// is); the parser turns this into a full error, enriched with the terminals it
/// expected in the current state — knowledge only the parser has.
pub struct LexFailure {
    pub ch: char,
    pub line: usize,
    pub col: usize,
    /// Byte offset of the un-lexable character. Used by character-level recovery to
    /// build a position-carrying `UnexpectedCharacter`; the non-recovering
    /// `lex_failure` path ignores it.
    pub pos: usize,
}

/// Why a [`TokenSource`] could not yield the next token.
///
/// Most sources only fail at the lexer level ([`LexFailure`]), which the parser
/// enriches with its expected-terminal set. A *postlex* source can also fail with
/// an already-formed [`ParseError`] — e.g. the indenter rejecting a bad dedent
/// column — which the parser propagates verbatim.
pub enum SourceError {
    Lex(LexFailure),
    Postlex(ParseError),
}

impl From<LexFailure> for SourceError {
    fn from(f: LexFailure) -> Self {
        SourceError::Lex(f)
    }
}

/// Yields tokens to the parser for a given lexer context.
///
/// `state` is the lexer-context key: for the LALR parser it is the current
/// parser state, which the contextual lexer uses to narrow the candidate
/// terminals. [`PreLexed`] ignores it.
pub trait TokenSource {
    /// The current token to act on. It is *not* consumed — a REDUCE re-reads the
    /// same token, while a SHIFT consumes it via [`advance`](Self::advance).
    /// Ignored terminals (whitespace, comments) are skipped transparently. At end
    /// of input, yields the synthetic `$END` token.
    fn peek(&mut self, state: usize) -> Result<Token, SourceError>;

    /// The current token's terminal id — the only field the LALR dispatch loop
    /// needs per action lookup. Default: full peek (a clone on caching sources);
    /// the hot sources override this to read the cached token in place, so the
    /// batch driver's REDUCE loop never clones a token (perf spike 2026-07-01).
    fn peek_type(&mut self, state: usize) -> Result<SymbolId, SourceError> {
        Ok(self.peek(state)?.type_id)
    }

    /// Consume the current token and return it **by move** (called on SHIFT by the
    /// batch driver). Default: clone-peek + advance, correct for every source; the
    /// hot sources override it to hand the cached token out without a clone.
    fn take_current(&mut self, state: usize) -> Result<Token, SourceError> {
        let tok = self.peek(state)?;
        self.advance();
        Ok(tok)
    }

    /// Consume the current token (called on SHIFT).
    fn advance(&mut self);

    /// Character-level recovery hook: skip the one un-lexable character that caused
    /// the most recent [`SourceError::Lex`] and resume lexing after it (issue #93).
    /// Only a lazily-lexing recovery source ([`ContextualRecovering`]) can hit a
    /// `Lex` failure mid-stream and therefore overrides this; every other source
    /// (the pre-lexed basic-recovery stream already has its un-lexable characters
    /// skipped up front) never surfaces a `Lex` failure to the recovery loop, so the
    /// default is unreachable and panics rather than silently spinning.
    fn skip_char(&mut self) {
        unreachable!("skip_char called on a source that does not lex lazily during recovery");
    }

    /// Recovery resume hook (issue #94): called by the recovering parse loop right
    /// after it deletes an offending token, mirroring Python Lark's `resume_parse`.
    /// A plain token source has nothing to do (the next token is just the next one),
    /// so the default is a no-op. A *postlex* source ([`PostlexContextual`])
    /// overrides it to reset its indenter — Python re-invokes `Indenter.process` on
    /// each resume, which clears `indent_level`/`paren_level`; reproducing that is
    /// what keeps recovery over an Indenter byte-for-byte faithful.
    fn on_delete(&mut self) {}
}

// ─── PreLexed: replay a fully lexed token vector ──────────────────────────────

/// A source backed by a token vector the basic lexer produced up front.
pub struct PreLexed {
    tokens: std::vec::IntoIter<Token>,
    current: Option<Token>,
}

impl PreLexed {
    pub fn new(tokens: Vec<Token>) -> Self {
        PreLexed {
            tokens: tokens.into_iter(),
            current: None,
        }
    }
}

impl TokenSource for PreLexed {
    fn peek(&mut self, _state: usize) -> Result<Token, SourceError> {
        if self.current.is_none() {
            // Past the end, keep yielding `$END` (the driver stops at ACCEPT).
            self.current = Some(self.tokens.next().unwrap_or_else(Token::end));
        }
        Ok(self.current.clone().unwrap())
    }

    fn peek_type(&mut self, _state: usize) -> Result<SymbolId, SourceError> {
        if self.current.is_none() {
            self.current = Some(self.tokens.next().unwrap_or_else(Token::end));
        }
        Ok(self.current.as_ref().unwrap().type_id)
    }

    fn take_current(&mut self, _state: usize) -> Result<Token, SourceError> {
        if self.current.is_none() {
            self.current = Some(self.tokens.next().unwrap_or_else(Token::end));
        }
        Ok(self.current.take().unwrap())
    }

    fn advance(&mut self) {
        self.current = None;
    }
}

// ─── Contextual: lex lazily, narrowing terminals by parser state ──────────────

/// A source that lexes lazily with the contextual lexer, attempting only the
/// terminals valid in the current parser state. The lexed token is cached and
/// reused across REDUCEs (which do not consume input) until a SHIFT advances it.
pub struct Contextual<'a> {
    lexer: &'a ContextualLexer,
    state: LexerState<'a>,
    current: Option<Token>,
    /// Owned vs. span (value-less) token emission (C8.1 #582). `parse_span` uses
    /// [`Self::new_span`] so the lexer allocates no owned `Token.value`.
    mode: TokenValueMode,
}

impl<'a> Contextual<'a> {
    pub fn new(text: &'a str, lexer: &'a ContextualLexer) -> Self {
        Contextual::with_mode(text, lexer, TokenValueMode::Owned)
    }

    /// A span-emitting contextual source (C8.1 #582): lexes value-less tokens so the
    /// lexer allocates no owned `Token.value` on the `parse_span` path. Positions are
    /// unchanged; the span builder recovers each value as an `&input` slice.
    pub fn new_span(text: &'a str, lexer: &'a ContextualLexer) -> Self {
        Contextual::with_mode(text, lexer, TokenValueMode::Span)
    }

    fn with_mode(text: &'a str, lexer: &'a ContextualLexer, mode: TokenValueMode) -> Self {
        Contextual {
            lexer,
            state: LexerState::new(text),
            current: None,
            mode,
        }
    }

    /// Lex the next non-ignored token for `parser_state`, or the `$END` token at
    /// end of input.
    ///
    /// On a per-state scanner miss this falls back to the **root** (full-terminal)
    /// scanner — Python Lark's `ContextualLexer.lex` exception branch — exactly as
    /// [`ContextualRecovering`] does, and for the same reason: it is what tells a
    /// *state-invalid-but-globally-valid* token apart from a *genuinely un-lexable*
    /// character (issue #346). A root match is an out-of-context token; we yield it
    /// so the parser surfaces it as an `UnexpectedToken` (matching Python's batch
    /// `l_ctx.parse` — e.g. `}` in an `a_part` state raises `UnexpectedToken`,
    /// pinned by `tests/test_interactive.rs::contextual_state_invalid_token_rbrace`).
    /// Only when the root scanner *also* misses is it a real `LexFailure`, which the
    /// non-recovering driver renders as `UnexpectedCharacter`
    /// ([`lex_failure`](super::lalr::LalrParser)). Without this fallback the driver
    /// could not distinguish the two cases and mis-classified every state miss.
    fn lex_next(&mut self, parser_state: usize) -> Result<Token, LexFailure> {
        loop {
            if self.state.is_done() {
                return Ok(Token::end().with_position(
                    self.state.line,
                    self.state.col,
                    self.state.char_pos,
                    self.state.char_pos,
                ));
            }
            let matched = match self.mode {
                TokenValueMode::Owned => self.lexer.next_token(
                    self.state.text,
                    self.state.pos,
                    self.state.char_pos,
                    parser_state,
                    self.state.line,
                    self.state.col,
                ),
                // Span (value-less) emission — no owned `Token.value` allocation.
                TokenValueMode::Span => self.lexer.next_token_span(
                    self.state.text,
                    self.state.pos,
                    self.state.char_pos,
                    parser_state,
                    self.state.line,
                    self.state.col,
                ),
            };
            match matched {
                // Ignored terminal (whitespace, comment): consume and keep going.
                // Advance by the token's char span, not `value.len()` — a span token
                // carries no value bytes (C8.1 #582).
                Ok(Some(tok)) if self.lexer.is_ignored(tok.type_id) => {
                    self.state.advance_by_chars(token_char_len(&tok));
                }
                Ok(Some(tok)) => return Ok(tok),
                // No scanner for this state, or no terminal valid here: fall back to
                // the root (full-terminal) scanner. A root match is an
                // out-of-context-but-valid token — yield it so the parser raises
                // `UnexpectedToken`; a root miss is a genuinely un-lexable character,
                // surfaced as `LexFailure` → `UnexpectedCharacter`. The root token is
                // returned without an `is_ignored` check for the same reason as
                // `ContextualRecovering::lex_next`: every state's scanner already
                // includes the `%ignore` terminals (`always_accept`), so an ignored
                // terminal always matches in the per-state branch above and control
                // never reaches here for one.
                Ok(None) | Err(_) => {
                    if let Some(tok) = self.lexer.next_root_token(
                        self.state.text,
                        self.state.pos,
                        self.state.char_pos,
                        self.state.line,
                        self.state.col,
                    ) {
                        return Ok(tok);
                    }
                    let ch = self.state.text[self.state.pos..].chars().next().unwrap();
                    return Err(LexFailure {
                        ch,
                        line: self.state.line,
                        col: self.state.col,
                        pos: self.state.pos,
                    });
                }
            }
        }
    }
}

impl<'a> TokenSource for Contextual<'a> {
    fn peek(&mut self, state: usize) -> Result<Token, SourceError> {
        if self.current.is_none() {
            self.current = Some(self.lex_next(state)?);
        }
        Ok(self.current.clone().unwrap())
    }

    fn peek_type(&mut self, state: usize) -> Result<SymbolId, SourceError> {
        if self.current.is_none() {
            self.current = Some(self.lex_next(state)?);
        }
        Ok(self.current.as_ref().unwrap().type_id)
    }

    fn take_current(&mut self, state: usize) -> Result<Token, SourceError> {
        if self.current.is_none() {
            self.current = Some(self.lex_next(state)?);
        }
        let tok = self.current.take().unwrap();
        // Advance by the token's char span (`end_pos - start_pos`), not
        // `value.len()`: a span token (C8.1 #582) carries no value bytes.
        self.state.advance_by_chars(token_char_len(&tok));
        Ok(tok)
    }

    fn advance(&mut self) {
        if let Some(tok) = self.current.take() {
            // Advance by the token's char span (`end_pos - start_pos`), not
            // `value.len()`: a span token (C8.1 #582) carries no value bytes.
            self.state.advance_by_chars(token_char_len(&tok));
        }
    }
}

// ─── ContextualRecovering: contextual lexing with root-lexer recovery (#166) ──

/// A recovery-aware contextual [`TokenSource`]: it lexes contextually like
/// [`Contextual`], but when the per-state scanner refuses a position it consults
/// the contextual lexer's **root** (full-terminal) scanner — Python Lark's
/// `ContextualLexer.lex` exception branch — to decide what the failure *means*:
///
///   * the root scanner matches a token → the input is an out-of-context-but-valid
///     token; yield it so the parser surfaces it as an `UnexpectedToken` the
///     recovery loop can *delete* (Python's `raise UnexpectedToken(...)`);
///   * the root scanner also misses → a genuinely un-lexable character; record an
///     `UnexpectedCharacter`, consult `on_error`, and skip exactly one character
///     before resuming (Python's re-raised `UnexpectedCharacters`).
///
/// This recovers over the *contextual* token stream, so a grammar whose contextual
/// lexer is load-bearing (overlapping terminals disambiguated only by parser state)
/// recovers to the same tree a clean contextual parse would have produced — the
/// fix for issue #166. The character-skips this source records and the token
/// deletions [`run_recovering`](super::lalr::LalrParser) records both flow into the
/// one `errors` list, exactly as the basic-lexer recovery path already does.
pub struct ContextualRecovering<'a> {
    lexer: &'a ContextualLexer,
    state: LexerState<'a>,
    current: Option<Token>,
}

impl<'a> ContextualRecovering<'a> {
    pub fn new(text: &'a str, lexer: &'a ContextualLexer) -> Self {
        ContextualRecovering {
            lexer,
            state: LexerState::new(text),
            current: None,
        }
    }

    /// Lex the next actionable token for `parser_state`, applying contextual
    /// scanning with root-lexer fallback. The two outcomes the recovery loop acts
    /// on:
    ///
    ///   * `Ok(token)` — a real token, or (via the root scanner) an
    ///     out-of-context-but-valid one the parser will delete, or `$END`;
    ///   * `Err(LexFailure)` — a genuinely un-lexable character (neither the
    ///     per-state nor the root scanner matched), which the recovery loop records
    ///     and resolves by [`skip_char`](Self::skip_char).
    fn lex_next(&mut self, parser_state: usize) -> Result<Token, LexFailure> {
        loop {
            if self.state.is_done() {
                return Ok(Token::end().with_position(
                    self.state.line,
                    self.state.col,
                    self.state.char_pos,
                    self.state.char_pos,
                ));
            }
            match self.lexer.next_token(
                self.state.text,
                self.state.pos,
                self.state.char_pos,
                parser_state,
                self.state.line,
                self.state.col,
            ) {
                // Ignored terminal (whitespace, comment): consume and keep going.
                Ok(Some(tok)) if self.lexer.is_ignored(tok.type_id) => {
                    self.state.advance_by(tok.value.len());
                }
                Ok(Some(tok)) => return Ok(tok),
                // No scanner for this state, or no terminal valid here: fall back to
                // the root (full-terminal) lexer — Python's `ContextualLexer.lex`
                // except-branch. A root match is an out-of-context-but-valid token
                // the parser will surface as `UnexpectedToken` and the recovery loop
                // will delete; a root miss is a genuinely un-lexable character.
                //
                // The root token is returned without an `is_ignored` check, and that
                // is sound only because every state's scanner already includes the
                // `%ignore` terminals (they ride `always_accept`, set in
                // `build_lalr`): an ignored terminal therefore always matches in the
                // per-state branch above, so control never reaches here for one. If
                // that invariant ever changes, this branch must filter ignored types.
                Ok(None) | Err(_) => {
                    if let Some(tok) = self.lexer.next_root_token(
                        self.state.text,
                        self.state.pos,
                        self.state.char_pos,
                        self.state.line,
                        self.state.col,
                    ) {
                        return Ok(tok);
                    }
                    let ch = self.state.text[self.state.pos..].chars().next().unwrap();
                    return Err(LexFailure {
                        ch,
                        line: self.state.line,
                        col: self.state.col,
                        pos: self.state.pos,
                    });
                }
            }
        }
    }
}

impl TokenSource for ContextualRecovering<'_> {
    fn peek(&mut self, state: usize) -> Result<Token, SourceError> {
        if self.current.is_none() {
            self.current = Some(self.lex_next(state)?);
        }
        Ok(self.current.clone().unwrap())
    }

    fn advance(&mut self) {
        if let Some(tok) = self.current.take() {
            self.state.advance_by(tok.value.len());
        }
    }

    /// Skip exactly one character at the current position and drop the cached
    /// lookahead, so the next [`peek`](TokenSource::peek) re-lexes from the
    /// character after the un-lexable one (the recovery loop's char-level skip,
    /// issue #93 — Python's `s.line_ctr.feed(text[p:p+1])`). `peek` only ever
    /// surfaces a `Lex` failure when neither the per-state nor the root scanner
    /// matched, so there is always a character here to skip.
    fn skip_char(&mut self) {
        if let Some(ch) = self.state.text[self.state.pos..].chars().next() {
            self.state.advance_by(ch.len_utf8());
        }
        self.current = None;
    }
}

// ─── BasicRecovering: lazy basic (global) lexing with char-skip recovery ──────

/// A recovery-aware **basic-lexer** [`TokenSource`]: it lexes the global terminal
/// set lazily, one token at a time, surfacing a [`SourceError::Lex`] at an
/// un-lexable position rather than skipping it up front. It is the basic-lexer
/// analogue of [`ContextualRecovering`], and exists so the basic-lexer postlex
/// recovery path can interleave char skips with the streaming indenter + parser
/// resume — Python Lark's lazy `lexer → PostLexConnector → parser` pipeline resets
/// the indenter on a char skip too, which an eager "skip every char up front, then
/// indent once" model cannot reproduce.
pub struct BasicRecovering<'a> {
    lexer: &'a BasicLexer,
    state: LexerState<'a>,
    current: Option<Token>,
}

impl<'a> BasicRecovering<'a> {
    pub fn new(text: &'a str, lexer: &'a BasicLexer) -> Self {
        BasicRecovering {
            lexer,
            state: LexerState::new(text),
            current: None,
        }
    }

    /// Lex the next non-ignored token, or `$END` at end of input; `Err(LexFailure)`
    /// at an un-lexable character the recovery loop records and skips. Like
    /// [`Contextual::lex_next`], an `%ignore` token is consumed (the cursor advances)
    /// and scanning continues; the returned real token is *not* consumed (the cursor
    /// still sits at its start) — [`advance`](TokenSource::advance) moves past it.
    fn lex_next(&mut self) -> Result<Token, LexFailure> {
        loop {
            if self.state.is_done() {
                return Ok(Token::end().with_position(
                    self.state.line,
                    self.state.col,
                    self.state.char_pos,
                    self.state.char_pos,
                ));
            }
            match self.lexer.next_token_at(
                self.state.text,
                self.state.pos,
                self.state.char_pos,
                self.state.line,
                self.state.col,
            ) {
                Ok(tok) if self.lexer.is_ignored(tok.type_id) => {
                    self.state.advance_by(tok.value.len());
                }
                Ok(tok) => return Ok(tok),
                Err(()) => {
                    let ch = self.state.text[self.state.pos..].chars().next().unwrap();
                    return Err(LexFailure {
                        ch,
                        line: self.state.line,
                        col: self.state.col,
                        pos: self.state.pos,
                    });
                }
            }
        }
    }
}

impl TokenSource for BasicRecovering<'_> {
    fn peek(&mut self, _state: usize) -> Result<Token, SourceError> {
        if self.current.is_none() {
            self.current = Some(self.lex_next()?);
        }
        Ok(self.current.clone().unwrap())
    }

    fn advance(&mut self) {
        if let Some(tok) = self.current.take() {
            self.state.advance_by(tok.value.len());
        }
    }

    /// Skip exactly one character and drop the cached lookahead (issue #93), so the
    /// next [`peek`](TokenSource::peek) re-lexes after the un-lexable character.
    fn skip_char(&mut self) {
        if let Some(ch) = self.state.text[self.state.pos..].chars().next() {
            self.state.advance_by(ch.len_utf8());
        }
        self.current = None;
    }
}

// ─── PostlexContextual: contextual lexing with a streaming postlex hook ────────

/// A [`TokenSource`] that lexes lazily with the [`Contextual`] lexer *and* runs a
/// streaming postlex hook ([`IndenterStream`]) over the result, injecting
/// synthetic INDENT/DEDENT tokens between the real ones (issue #67).
///
/// The contextual lexer can't be materialized up front — it narrows the candidate
/// terminals by the live parser state — so the postlex has to sit inside the lazy
/// pull loop rather than after a finished `Vec`. This adapter pulls one real token
/// at a time from the inner lexer (at the current parser state), feeds it to the
/// indenter, and serves tokens from the indenter's output queue. The indenter's
/// newline terminal is forced into every state's scanner (`always_accept`, set up
/// in `build_frontend`) so the lazy lexer still emits the newlines it measures
/// indentation from — mirroring Python Lark's `PostLex.always_accept`.
///
/// Note the inner lexer is advanced *eagerly* as each real token is pulled, while
/// the `peek`/`advance` the parser sees operate on the indenter's output queue.
/// The two are decoupled: a real token is only pulled once the queue drains, so
/// every pull happens at the parser state that follows the previously injected
/// tokens — exactly the state at which that token must be lexed.
///
/// The inner source is generic so the *same* streaming-indenter machine drives
/// both a clean parse (over [`Contextual`]) and recovery (over
/// [`ContextualRecovering`], issue #94): the indenter sits upstream of the parser's
/// token-deletion recovery exactly as Python's `PostLexConnector` sits upstream of
/// `resume_parse`, so a deleted token never reaches the indenter and its
/// bracket/indent bookkeeping cannot desync.
pub struct PostlexContextual<'a, S: TokenSource> {
    inner: S,
    stream: IndenterStream<'a>,
    /// The token currently offered to the parser (cached across REDUCEs).
    current: Option<Token>,
    /// The real `$END` token, held back until the indenter's trailing DEDENTs have
    /// been flushed (Python emits them *before* end of input).
    end_token: Option<Token>,
    /// Whether [`IndenterStream::finish`] has run (the EOF flush happens once).
    finished: bool,
}

impl<'a, S: TokenSource> PostlexContextual<'a, S> {
    pub(crate) fn new(inner: S, stream: IndenterStream<'a>) -> Self {
        PostlexContextual {
            inner,
            stream,
            current: None,
            end_token: None,
            finished: false,
        }
    }
}

impl<'a, S: TokenSource> PostlexContextual<'a, S> {
    /// Ensure `self.current` holds the next token to offer (the shared fill behind
    /// `peek`/`peek_type`/`take_current`, perf spike 2026-07-01 — one materialization,
    /// no per-call clone).
    fn fill(&mut self, state: usize) -> Result<(), SourceError> {
        if self.current.is_some() {
            return Ok(());
        }
        loop {
            // Serve any token the indenter has already queued.
            if let Some(tok) = self.stream.pop() {
                self.current = Some(tok);
                return Ok(());
            }
            // Queue empty. If the inner lexer is exhausted, flush trailing DEDENTs
            // once (they land before `$END`), then serve the held-back `$END`.
            if let Some(end) = &self.end_token {
                if !self.finished {
                    self.stream.finish();
                    self.finished = true;
                    continue;
                }
                self.current = Some(end.clone());
                return Ok(());
            }
            // Pull the next real token from the contextual lexer at the current
            // parser state, advancing the inner stream past it.
            let tok = self.inner.take_current(state)?;
            if tok.type_id == SymbolId::END {
                self.end_token = Some(tok);
            } else {
                self.stream.feed(tok).map_err(SourceError::Postlex)?;
            }
        }
    }
}

impl<S: TokenSource> TokenSource for PostlexContextual<'_, S> {
    fn peek(&mut self, state: usize) -> Result<Token, SourceError> {
        self.fill(state)?;
        Ok(self.current.clone().unwrap())
    }

    fn peek_type(&mut self, state: usize) -> Result<SymbolId, SourceError> {
        self.fill(state)?;
        Ok(self.current.as_ref().unwrap().type_id)
    }

    fn take_current(&mut self, state: usize) -> Result<Token, SourceError> {
        self.fill(state)?;
        Ok(self.current.take().unwrap())
    }

    fn advance(&mut self) {
        self.current = None;
    }

    /// Forward a character-level skip to the inner (lazily-lexing) source. Reached
    /// only during recovery over a [`ContextualRecovering`] inner source, when the
    /// inner `peek` surfaced a `Lex` failure *through* this adapter (the indenter
    /// queue was empty, so the failure was not masked by a pending injected token).
    ///
    /// A char skip is *also* a recovery resume: Python Lark routes both
    /// `UnexpectedToken` (token deletion) **and** `UnexpectedCharacters` (char skip)
    /// through `resume_parse`, which re-invokes `Indenter.process` and so resets
    /// `indent_level`/`paren_level` either way (`lalr_parser.py`'s `on_error` loop).
    /// We mirror that by resetting the indenter here too — not just in
    /// [`on_delete`](Self::on_delete) — so a char skip *inside an indented block*
    /// stays byte-for-byte faithful (otherwise the surviving block measures
    /// indentation against a stale stack and the recovered tree diverges). Then drop
    /// our cached lookahead so the next `peek` re-pulls from the inner lexer.
    fn skip_char(&mut self) {
        self.inner.skip_char();
        self.stream.reset_for_resume();
        self.current = None;
    }

    /// Reset the streaming indenter on a recovery resume (issue #94). Python Lark's
    /// `resume_parse` re-invokes `Indenter.process`, which clears the indent/paren
    /// stack each call — so after the parser deletes an offending token, a *fresh*
    /// indenter continues over the lexer's current position with `indent_stack=[0]`.
    /// We mirror that exactly: reset the [`IndenterStream`]'s bookkeeping and drop
    /// our cached lookahead, leaving the inner lexer position untouched. This is the
    /// step that makes a *multi-deletion* recovery byte-for-byte faithful — without
    /// it the trailing DEDENTs that close a block are never re-emitted, so lark-rs
    /// would (wrongly) recover a tree Python re-raises on.
    fn on_delete(&mut self) {
        self.stream.reset_for_resume();
        self.current = None;
    }
}

/// Drive the LALR parser over the contextual lexer with a streaming [`Indenter`]
/// postlex hook. Resolves the indenter's `%declare`d ids up front, then loops the
/// shared parser driver against a [`PostlexContextual`] source over a clean
/// [`Contextual`] lexer.
pub fn postlex_contextual_source<'a>(
    text: &'a str,
    lexer: &'a ContextualLexer,
    postlex: &'a Indenter,
    symbols: &crate::grammar::intern::SymbolTable,
) -> Result<PostlexContextual<'a, Contextual<'a>>, ParseError> {
    let stream = IndenterStream::new(postlex, symbols)?;
    Ok(PostlexContextual::new(Contextual::new(text, lexer), stream))
}

/// As [`postlex_contextual_source`], but over the **recovering** contextual lexer
/// ([`ContextualRecovering`]) for error recovery (issue #94, sub-target 1). The
/// streaming indenter runs over the recovering contextual stream, and the parser's
/// token-deletion recovery happens downstream of the injection — mirroring Python's
/// `lexer → PostLexConnector(postlex) → parser` wiring, where `on_error` deletes
/// from the post-indenter stream. A grammar whose contextual lexer is load-bearing
/// (overlapping terminals disambiguated only by parser state) therefore recovers to
/// the same tree a clean contextual parse would build, *with* INDENT/DEDENT.
pub fn postlex_contextual_recovering_source<'a>(
    text: &'a str,
    lexer: &'a ContextualLexer,
    postlex: &'a Indenter,
    symbols: &crate::grammar::intern::SymbolTable,
) -> Result<PostlexContextual<'a, ContextualRecovering<'a>>, ParseError> {
    let stream = IndenterStream::new_recovering(postlex, symbols)?;
    Ok(PostlexContextual::new(
        ContextualRecovering::new(text, lexer),
        stream,
    ))
}

/// As [`postlex_contextual_recovering_source`], but over the **basic** (global)
/// lexer (the basic-lexer postlex driver, issue #94). It drives the *same* streaming
/// indenter + per-resume-reset machine over a lazy [`BasicRecovering`] source, so
/// the basic-lexer postlex recovery path is byte-for-byte identical to Python's
/// (whose `Indenter.process` resets per `resume_parse`) — including interleaving
/// char skips with the indenter reset, which an eager "lex_recovering up front, then
/// indent once" model cannot reproduce.
pub fn postlex_basic_recovering_source<'a>(
    text: &'a str,
    lexer: &'a BasicLexer,
    postlex: &'a Indenter,
    symbols: &crate::grammar::intern::SymbolTable,
) -> Result<PostlexContextual<'a, BasicRecovering<'a>>, ParseError> {
    let stream = IndenterStream::new_recovering(postlex, symbols)?;
    Ok(PostlexContextual::new(
        BasicRecovering::new(text, lexer),
        stream,
    ))
}
