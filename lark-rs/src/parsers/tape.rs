//! `TapeTree<'i, 'g>` — the internal flat-tape **output** backend prototype
//! (#243 C8c, perf spike 2026-07-02). **Experimental, feature-gated
//! (`--features tape-tree`), internal-only** — not a public commitment (the
//! promotion question is #243's `needs-decision`; ADR-0029).
//!
//! Where the default tree backend materializes one `Tree` + label `String` +
//! child `Vec` per node and two `String`s per token, and the opt-in `SpanTree`
//! still allocates one `SpanBranch` child `Vec` per node, this backend appends
//! everything to **two flat arrays** (`nodes`, `kids`) that grow amortized — so a
//! whole parse performs O(log n) allocator calls, driving the per-parse
//! allocation *count* (the post-2026-07-01 frontier, see
//! `docs/notes/perf-spike-2026-07-01.md`) to ~zero on the output side:
//!
//!   * a shifted terminal appends one [`TapeEntry::Token`] carrying positions and
//!     the **byte span** of its text (sliceable from the parse `input` — no owned
//!     value, `perf::token_value_string_bytes == 0`);
//!   * a reduction appends its children's tape indices to `kids` and one
//!     [`TapeEntry::Node`] pointing at that range (`perf::tree_nodes_built == 0`);
//!   * labels stay interned — a node stores its rule index, a token its
//!     [`SymbolId`]; names resolve lazily at projection time.
//!
//! Like `SpanTree` (ADR-0026) it is beyond-oracle in *representation*, not
//! behaviour, so it ships under the **relative oracle**:
//! [`TapeTree::materialize`] projects the tape back to the exact [`ParseTree`]
//! `parse()` returns, gated byte-identical over the compliance bank
//! (`tests/test_tape_tree.rs`), with the zero-materialization counters proving
//! no generic output was built. Support boundary is `parse_into`'s (LALR +
//! basic/contextual, ADR-0029 fork 4).
//!
//! Filtered punctuation caveat: the engine calls [`OutputBuilder::token`] for
//! *every* shifted terminal and drops filtered ones during shaping, so a
//! filtered token's tape entry stays behind, **orphaned** (present in `nodes`,
//! referenced by no `kids` range). A tape consumer walks from [`TapeTree::root`]
//! and never sees them; the space cost is bounded by the token count.

use crate::grammar::intern::{CompiledRule, SymbolId, SymbolTable};
use crate::tree::{checked_pos, Child, Meta, ParseTree, PosInt, Token, Tree};

use super::tree_builder::{OutputBuilder, OutputContext};

/// One entry on the tape. Tokens carry both the char positions (`Token` parity,
/// #278) and the byte span of the matched text (for O(1) slicing out of the
/// input); nodes carry their rule index, the engine-computed `Meta`, and a range
/// into the shared `kids` array.
#[derive(Debug, Clone)]
pub enum TapeEntry {
    Token {
        type_id: SymbolId,
        byte_start: PosInt,
        byte_end: PosInt,
        line: PosInt,
        column: PosInt,
        end_line: PosInt,
        end_column: PosInt,
        start_pos: PosInt,
        end_pos: PosInt,
    },
    Node {
        /// Rule index — resolves to the callback name (`Tree::data`) lazily.
        rule: u32,
        kids_start: u32,
        kids_len: u32,
        meta: Meta,
    },
    /// A `maybe_placeholders` absent optional (`Child::None`).
    None,
}

/// A parsed tape: the flat entries, the child-index array, and the root entry.
/// Borrows the parse input (`'i`) for token text and the grammar (`'g`) for
/// lazy name resolution.
pub struct TapeTree<'i, 'g> {
    input: &'i str,
    rules: &'g [CompiledRule],
    symbols: &'g SymbolTable,
    nodes: Vec<TapeEntry>,
    kids: Vec<u32>,
    root: u32,
}

impl<'i, 'g> TapeTree<'i, 'g> {
    /// The tape entries (post-order: children precede parents).
    pub fn entries(&self) -> &[TapeEntry] {
        &self.nodes
    }

    /// The root entry index.
    pub fn root(&self) -> u32 {
        self.root
    }

    /// A node entry's children (tape indices).
    pub fn children(&self, entry: &TapeEntry) -> &[u32] {
        match entry {
            TapeEntry::Node {
                kids_start,
                kids_len,
                ..
            } => &self.kids[*kids_start as usize..(*kids_start + *kids_len) as usize],
            _ => &[],
        }
    }

    /// A token entry's matched text, sliced zero-copy from the input.
    pub fn token_text(&self, entry: &TapeEntry) -> Option<&'i str> {
        match entry {
            TapeEntry::Token {
                byte_start,
                byte_end,
                ..
            } => Some(&self.input[*byte_start as usize..*byte_end as usize]),
            _ => None,
        }
    }

    /// Project the tape back to the owned [`ParseTree`] `parse()` returns — the
    /// relative-oracle direction (ADR-0026). Owned names/values are copied *here*,
    /// at the boundary, never during the parse. Recurses to tree depth (an opt-in
    /// projection utility for the gate, not an engine path — same caveat as
    /// `SpanNode::materialize`).
    pub fn materialize(&self) -> ParseTree {
        match self.materialize_child(self.root) {
            Child::Tree(t) => ParseTree::Tree(t),
            Child::Token(t) => ParseTree::Token(t),
            Child::None => ParseTree::None,
        }
    }

    fn materialize_child(&self, idx: u32) -> Child {
        match &self.nodes[idx as usize] {
            TapeEntry::Token {
                type_id,
                byte_start,
                byte_end,
                line,
                column,
                end_line,
                end_column,
                start_pos,
                end_pos,
            } => Child::Token(Token {
                type_id: *type_id,
                type_: self.symbols.name(*type_id).to_string(),
                value: self.input[*byte_start as usize..*byte_end as usize].to_string(),
                line: *line,
                column: *column,
                end_line: *end_line,
                end_column: *end_column,
                start_pos: *start_pos,
                end_pos: *end_pos,
            }),
            TapeEntry::Node {
                rule,
                kids_start,
                kids_len,
                meta,
            } => Child::Tree(Tree {
                data: self.rules[*rule as usize].tree_name.to_string(),
                children: self.kids[*kids_start as usize..(*kids_start + *kids_len) as usize]
                    .iter()
                    .map(|&k| self.materialize_child(k))
                    .collect(),
                meta: meta.clone(),
            }),
            TapeEntry::None => Child::None,
        }
    }
}

/// The tape [`OutputBuilder`]: `Value = u32` (a tape index), so the engine's
/// value stack carries 4-byte handles and the builder appends to two flat
/// arrays. Single-parse state; [`Lark::parse_tape`](crate::Lark::parse_tape)
/// mints a fresh one per call and moves the arrays into the returned
/// [`TapeTree`].
pub struct TapeBuilder {
    nodes: Vec<TapeEntry>,
    kids: Vec<u32>,
    /// Running char→byte cursor (same trick as `SpanTreeBuilder`): token
    /// positions are char indices (#278); slicing needs byte offsets. Tokens
    /// shift in non-decreasing order on this path, so one forward cursor is
    /// amortized O(1) per token.
    cursor_char: usize,
    cursor_byte: usize,
}

impl TapeBuilder {
    pub(crate) fn new() -> Self {
        TapeBuilder {
            nodes: Vec::new(),
            kids: Vec::new(),
            cursor_char: 0,
            cursor_byte: 0,
        }
    }

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
                None => break,
            }
        }
        self.cursor_byte
    }

    fn push(&mut self, entry: TapeEntry) -> u32 {
        let idx = self.nodes.len();
        debug_assert!(idx <= u32::MAX as usize, "tape entry count exceeds u32");
        self.nodes.push(entry);
        idx as u32
    }

    pub(crate) fn into_tape<'i, 'g>(
        self,
        input: &'i str,
        rules: &'g [CompiledRule],
        symbols: &'g SymbolTable,
        root: u32,
    ) -> TapeTree<'i, 'g> {
        TapeTree {
            input,
            rules,
            symbols,
            nodes: self.nodes,
            kids: self.kids,
            root,
        }
    }
}

impl<'i> OutputBuilder<'i> for TapeBuilder {
    type Value = u32;

    fn token(&mut self, token: Token, input: &'i str, _ctx: &OutputContext) -> u32 {
        // No owned value: record the byte span (the span source lexes value-less
        // tokens, C8.1) — `perf::token_value_string_bytes` stays 0 by
        // construction (nothing is charged and nothing is copied).
        let byte_start = self.byte_offset_at(input, token.start_pos as usize);
        let byte_end = self.byte_offset_at(input, token.end_pos as usize);
        self.push(TapeEntry::Token {
            type_id: token.type_id,
            byte_start: checked_pos(byte_start),
            byte_end: checked_pos(byte_end),
            line: token.line,
            column: token.column,
            end_line: token.end_line,
            end_column: token.end_column,
            start_pos: token.start_pos,
            end_pos: token.end_pos,
        })
    }

    fn reduce(
        &mut self,
        rule: usize,
        children: &mut Vec<u32>,
        meta: &Meta,
        _ctx: &OutputContext,
    ) -> u32 {
        let kids_start = self.kids.len();
        debug_assert!(
            kids_start <= u32::MAX as usize,
            "tape kid count exceeds u32"
        );
        self.kids.extend_from_slice(children);
        self.push(TapeEntry::Node {
            rule: rule as u32,
            kids_start: kids_start as u32,
            kids_len: children.len() as u32,
            meta: meta.clone(),
        })
    }

    fn placeholder(&mut self, _ctx: &OutputContext) -> u32 {
        self.push(TapeEntry::None)
    }
}
