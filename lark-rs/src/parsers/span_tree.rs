//! `SpanTree<'i>` — the zero-copy **output** backend (C8, #233).
//!
//! The first genuinely speed-relevant slice of epic #225. Where the default
//! [`TreeOutputBuilder`](super::tree_builder::TreeOutputBuilder) materializes an
//! owned [`Tree`]/[`Token`] per reduction (`data: String`, `value: String` — the
//! *output* portion of ADR-0011's ~3 allocs/byte), this backend builds a
//! [`SpanNode`] whose:
//!
//!   * **token values borrow the input** — `&'i str` slices of the parse `input`,
//!     so the *output* holds no copied value `String` (and
//!     [`perf::token_value_string_bytes`] stays `0`);
//!   * **labels borrow the grammar** — a node's callback name and a token's
//!     terminal name are `&'g str` into the already-interned grammar tables, never
//!     re-allocated (the "labels interned" half);
//!   * **no `Tree` node is built** — the engine never routes through
//!     [`build_node`](super::tree_builder::TreeOutputBuilder), so
//!     [`perf::tree_nodes_built`] stays `0`.
//!
//! **Scope — output *and* lexer halves; the child `Vec` reuse is still open.** As of
//! C8.1 (#582) the lexer no longer allocates a `Token.value: String` on the span
//! path: `Lark::parse_span` drives a **span-emitting** token source
//! (`make_span_source`) whose tokens carry positions but an empty `value`
//! (`crate::perf::lexer_token_value_bytes == 0`), and `SpanTreeBuilder::token`
//! recovers each value as an `&input` slice through the char→byte cursor. The
//! `run_into` seam still `clone()`s the (now value-less) token per shift — an empty
//! `String` clone, no heap traffic. So both `token_value_string_bytes` (output copy)
//! **and** `lexer_token_value_bytes` (upstream alloc) are `0` on the span path,
//! separately gated so "the pipeline allocated no token strings" is a counter result
//! (ADR-0007). The default `parse()`/`parse_into` owned path is unchanged
//! (byte-identical, `lexer_token_value_bytes > 0`). Still open: the per-reduction
//! child `Vec` is allocated (bounded, but not reused) — the reuse gate #233 asks for
//! is split to #583 (C8.2).
//!
//! This ships under ADR-0026's **relative oracle** (it is beyond-oracle in
//! *representation*, not behaviour): [`SpanNode::materialize`] projects a
//! `SpanNode` back to the exact [`ParseTree`] `parse()` returns, and the projection
//! is gated byte-identical over the corpus (`tests/test_span_tree.rs`). Combined
//! with the C5 counters (#230) proving zero owned *output*, that is the whole
//! falsifiable story — no throughput claim ships except as a gated counter result
//! (ADR-0027 non-goal). Per ADR-0029 fork 3 the surface is **experimental /
//! feature-gated** (`--features span-tree`), not stabilised.
//!
//! Support boundary is the `parse_into` seam's: LALR + basic/contextual lexer
//! (ADR-0029 fork 4). [`Lark::parse_span`](crate::Lark::parse_span) is the entry
//! point; every other configuration returns the same typed refusal `parse_into`
//! does.

use crate::grammar::intern::{CompiledRule, SymbolId, SymbolTable};
use crate::tree::{Child, Meta, ParseTree, Token, Tree};

use super::tree_builder::{OutputBuilder, OutputContext};

/// A borrowed leaf token: the terminal id + name (`&'g` grammar), the matched text
/// as an `&'i` input span, and the engine-computed positions. Byte-for-byte the
/// data a [`Token`] carries, minus the two owned `String`s (`type_`, `value`) —
/// those are resolved lazily at [`materialize`](SpanNode::materialize) time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpanToken<'i, 'g> {
    /// Interned terminal id (the parser dispatches on this, never on the name).
    pub type_id: SymbolId,
    /// Terminal type name (e.g. `"NUMBER"`), borrowed from the grammar's symbol
    /// table — the [`Token::type_`] world, resolved once and shared, never copied.
    pub type_name: &'g str,
    /// The matched text, an `&'i str` slice of the parse `input` (zero copy) — the
    /// [`Token::value`] world.
    pub value: &'i str,
    pub line: usize,
    pub column: usize,
    pub end_line: usize,
    pub end_column: usize,
    pub start_pos: usize,
    pub end_pos: usize,
}

/// A borrowed interior node: the callback name (`&'g` grammar — Python's
/// `create_callback` dispatch string, alias-else-origin), its shaped children, and
/// the engine-computed span [`Meta`]. No `Tree` is built.
///
/// (No `PartialEq`: [`Meta`] carries no equality; the gate compares via
/// [`SpanNode::materialize`] + `Debug`.)
#[derive(Debug, Clone)]
pub struct SpanBranch<'i, 'g> {
    /// The node's callback name (`Tree::data`), borrowed from the grammar.
    pub name: &'g str,
    pub children: Vec<SpanNode<'i, 'g>>,
    pub meta: Meta,
}

/// A node in a borrowed, zero-copy parse tree — the [`Child`] analog whose leaves
/// borrow the input and whose labels borrow the grammar.
///
/// `'i` ties borrowed token values to the parse `input`; `'g` ties borrowed labels
/// to the grammar (the [`Lark`](crate::Lark) that produced it). A `None` is a
/// `maybe_placeholders` absent optional, exactly as [`Child::None`].
#[derive(Debug, Clone)]
pub enum SpanNode<'i, 'g> {
    Token(SpanToken<'i, 'g>),
    Branch(SpanBranch<'i, 'g>),
    None,
}

impl<'i, 'g> SpanNode<'i, 'g> {
    /// The node's span metadata (a token's derived [`Meta`] or a branch's
    /// engine-computed one; `None` for a placeholder).
    pub fn meta(&self) -> Option<Meta> {
        match self {
            // The raw `Meta` a shifted token contributes (all fields present,
            // `empty = false`) — the same shape `meta_from_token` builds.
            SpanNode::Token(t) => Some(Meta {
                line: Some(t.line as u32),
                column: Some(t.column as u32),
                end_line: Some(t.end_line as u32),
                end_column: Some(t.end_column as u32),
                start_pos: Some(t.start_pos as u32),
                end_pos: Some(t.end_pos as u32),
                empty: false,
            }),
            SpanNode::Branch(b) => Some(b.meta.clone()),
            SpanNode::None => Option::None,
        }
    }

    /// Project this borrowed node back to the owned [`ParseTree`] `parse()` returns
    /// — the **relative-oracle** direction (ADR-0026). Byte-identical to the tree
    /// backend by construction: a branch becomes `Tree { data: name.to_string(),
    /// children, meta }`, a token becomes the full owned [`Token`] (names/value
    /// copied *here*, at the boundary, not during the parse).
    ///
    /// Recurses to tree depth — an opt-in projection utility for the gate and for
    /// callers who want an owned tree after a zero-copy walk, not an engine hot
    /// path (the engine builds the `SpanNode` iteratively via `run_into`; only this
    /// boundary conversion recurses, and only when asked).
    pub fn materialize(&self) -> ParseTree {
        match self.materialize_child() {
            Child::Tree(t) => ParseTree::Tree(t),
            Child::Token(t) => ParseTree::Token(t),
            Child::None => ParseTree::None,
        }
    }

    fn materialize_child(&self) -> Child {
        match self {
            SpanNode::Token(t) => Child::Token(Token {
                type_id: t.type_id,
                type_: t.type_name.to_string(),
                value: t.value.to_string(),
                line: t.line as u32,
                column: t.column as u32,
                end_line: t.end_line as u32,
                end_column: t.end_column as u32,
                start_pos: t.start_pos as u32,
                end_pos: t.end_pos as u32,
            }),
            SpanNode::Branch(b) => Child::Tree(Tree {
                data: b.name.to_string(),
                children: b.children.iter().map(SpanNode::materialize_child).collect(),
                meta: b.meta.clone(),
            }),
            SpanNode::None => Child::None,
        }
    }
}

/// The zero-copy [`OutputBuilder`] backend. Borrows the grammar's interned rule /
/// symbol tables (`&'g`) so it can resolve a reduction's callback name and a
/// shifted terminal's type name to `&'g str` *without* re-allocating — the same
/// tables `OutputContext` reads, held directly so the produced [`SpanNode`] outlives
/// the builder (its label borrows point into the grammar, not into `self`).
///
/// Constructed by [`Lark::parse_span`](crate::Lark::parse_span). Holds a monotonic
/// char→byte cursor (below) as its only mutable state, so it is single-parse state,
/// not reusable across parses — [`Lark::parse_span`] mints a fresh one per call.
pub struct SpanTreeBuilder<'g> {
    rules: &'g [CompiledRule],
    symbols: &'g SymbolTable,
    /// A running `(char index, byte offset)` cursor into the parse input.
    ///
    /// A [`Token`]'s `start_pos`/`end_pos` are **character** indices (Python parity,
    /// #278), but slicing `&input[..]` needs **byte** offsets — the two diverge on
    /// any non-ASCII input. Tokens are shifted in strictly non-decreasing position
    /// order on the non-recovering LALR path, so a single forward cursor converts
    /// every token's char range to a byte range in O(1) amortized (O(input) total),
    /// with no owned copy. A non-monotonic jump (never expected on this path)
    /// safely recomputes from the start.
    cursor_char: usize,
    cursor_byte: usize,
}

impl<'g> SpanTreeBuilder<'g> {
    pub(crate) fn new(rules: &'g [CompiledRule], symbols: &'g SymbolTable) -> Self {
        SpanTreeBuilder {
            rules,
            symbols,
            cursor_char: 0,
            cursor_byte: 0,
        }
    }

    /// Advance the cursor to character index `char_idx` and return the byte offset
    /// there. Amortized O(1) across a parse (the cursor only moves forward); resets
    /// to the input start if asked to go backwards (belt-and-suspenders — the LALR
    /// shift order never does).
    fn byte_offset_at(&mut self, input: &str, char_idx: usize) -> usize {
        if char_idx < self.cursor_char {
            self.cursor_char = 0;
            self.cursor_byte = 0;
        }
        while self.cursor_char < char_idx {
            match input[self.cursor_byte..].chars().next() {
                Some(ch) => {
                    self.cursor_byte += ch.len_utf8();
                    self.cursor_char += 1;
                }
                None => break, // clamp at end-of-input (defensive; positions are in range)
            }
        }
        self.cursor_byte
    }
}

impl<'i, 'g> OutputBuilder<'i> for SpanTreeBuilder<'g> {
    type Value = SpanNode<'i, 'g>;

    fn token(&mut self, token: Token, input: &'i str, _ctx: &OutputContext) -> Self::Value {
        // Borrow the matched text out of `input` rather than copying `token.value`
        // — this is the whole point: no owned value bytes, so
        // `perf::token_value_string_bytes` stays 0 for a span-only parse. The
        // terminal *name* likewise borrows the grammar's symbol table (`&'g str`),
        // copied from `self`, not from the per-call `ctx` (whose borrow does not
        // outlive this call).
        //
        // References are `Copy`, so reading `self.symbols` yields a genuine `&'g`
        // reference (tied to the grammar, not to this `&mut self` borrow), which is
        // what lets the returned value outlive the builder.
        let symbols: &'g SymbolTable = self.symbols;
        // `start_pos`/`end_pos` are char indices; map them to byte offsets through
        // the forward cursor before slicing (#278 — the two differ on non-ASCII).
        let byte_start = self.byte_offset_at(input, token.start_pos as usize);
        let byte_end = self.byte_offset_at(input, token.end_pos as usize);
        SpanNode::Token(SpanToken {
            type_id: token.type_id,
            type_name: symbols.name(token.type_id),
            value: &input[byte_start..byte_end],
            line: token.line as usize,
            column: token.column as usize,
            end_line: token.end_line as usize,
            end_column: token.end_column as usize,
            start_pos: token.start_pos as usize,
            end_pos: token.end_pos as usize,
        })
    }

    fn reduce(
        &mut self,
        rule: usize,
        children: &mut Vec<Self::Value>,
        meta: &Meta,
        _ctx: &OutputContext,
    ) -> Self::Value {
        // No `Tree` node — build a borrowed branch. The callback name borrows the
        // grammar's interned `tree_name` (alias-else-origin, template folded in) —
        // the same string `OutputContext::callback_name` resolves, read from `self`
        // so it is `&'g` and outlives the builder. The engine already computed
        // `meta` (single source of truth), so use it verbatim; positions can't
        // diverge from the tree backend.
        let rules: &'g [CompiledRule] = self.rules;
        SpanNode::Branch(SpanBranch {
            name: &rules[rule].tree_name,
            children: std::mem::take(children),
            meta: meta.clone(),
        })
    }

    fn placeholder(&mut self, _ctx: &OutputContext) -> Self::Value {
        SpanNode::None
    }
}
