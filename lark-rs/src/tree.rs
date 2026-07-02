//! Parse tree types: `Tree` and `Token`.

use std::fmt;

use crate::grammar::intern::SymbolId;

/// The integer width source positions are **stored** in (ADR-0040). It is a private
/// storage detail, never part of the public surface: every accessor
/// ([`Token::line`], [`Meta::start_pos`], …) returns `usize` regardless of this
/// width, so switching it neither changes a public type nor breaks a caller.
///
/// Default `u32` — halves the position payload of every `Token`/`Meta` moved through
/// the parser's value stack vs `usize` (the stack element `GSlot<Child>` shrinks
/// 264 → ~168 bytes; `Option<u32>` is 8 B where `Option<usize>` is 16 B), at the
/// cost of a 4 GiB (`u32::MAX`) input bound. The **`wide-positions`** feature widens
/// it to `u64`, lifting that bound and restoring the pre-ADR-0040 layout, for the
/// rare consumer parsing >4 GiB inputs (e.g. over the zero-copy span backend). The
/// feature is additive-safe: it only enlarges private storage, so unifying it across
/// a dependency graph can never break a caller (accessors still return `usize`).
#[cfg(not(feature = "wide-positions"))]
pub(crate) type PosInt = u32;
#[cfg(feature = "wide-positions")]
pub(crate) type PosInt = u64;

/// Narrow a `usize` source position into the private [`PosInt`] storage, asserting
/// (in debug builds) that it fits (ADR-0040). This is the loud tripwire for the
/// default `u32` build's 4 GiB (`u32::MAX`) input ceiling: rather than silently
/// truncate a position past the ceiling, a debug build panics with an actionable
/// message. Compiles to a bare `as` cast in release (the `debug_assert!` is elided),
/// so the hot path pays nothing. Under `wide-positions` (`u64`) the bound is
/// `usize`-wide, so the assert never fires.
#[inline]
pub(crate) fn checked_pos(v: usize) -> PosInt {
    debug_assert!(
        v <= PosInt::MAX as usize,
        "source position {v} exceeds the {}-bit position width ({} max); \
         enable the `wide-positions` cargo feature to parse inputs > 4 GiB",
        PosInt::BITS,
        PosInt::MAX
    );
    v as PosInt
}

/// A positioned token from the lexer.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Token {
    /// Interned terminal id — the parser dispatches on this (an array index),
    /// never on the name. [`SymbolId::END`] for `$END`; [`SymbolId::UNSET`] for
    /// tokens not produced by a lexer.
    pub type_id: SymbolId,
    /// Terminal type name (e.g. "WORD", "NUMBER"), kept for tree output/display.
    pub type_: String,
    /// The matched text.
    pub value: String,
    // Source positions. Stored in `PosInt` (private width, ADR-0040) and exposed
    // only through the `usize`-returning accessors below, so the storage width is
    // never part of the public surface.
    pub(crate) line: PosInt,
    pub(crate) column: PosInt,
    pub(crate) end_line: PosInt,
    pub(crate) end_column: PosInt,
    pub(crate) start_pos: PosInt,
    pub(crate) end_pos: PosInt,
}

impl Token {
    pub fn new(type_: impl Into<String>, value: impl Into<String>) -> Self {
        Token {
            type_id: SymbolId::UNSET,
            type_: type_.into(),
            value: value.into(),
            line: 0,
            column: 0,
            end_line: 0,
            end_column: 0,
            start_pos: 0,
            end_pos: 0,
        }
    }

    /// The synthetic end-of-input token, carrying [`SymbolId::END`].
    pub fn end() -> Self {
        let mut t = Token::new("$END", "");
        t.type_id = SymbolId::END;
        t
    }

    pub fn with_position(mut self, line: usize, col: usize, start: usize, end: usize) -> Self {
        self.line = checked_pos(line);
        self.column = checked_pos(col);
        self.end_line = checked_pos(line); // updated by lexer for multi-line tokens
        self.end_column = checked_pos(col + (end - start));
        self.start_pos = checked_pos(start);
        self.end_pos = checked_pos(end);
        self
    }

    /// 1-based line of the token's first character.
    #[inline]
    pub fn line(&self) -> usize {
        self.line as usize
    }
    /// 1-based column of the token's first character.
    #[inline]
    pub fn column(&self) -> usize {
        self.column as usize
    }
    /// 1-based line of the character after the token.
    #[inline]
    pub fn end_line(&self) -> usize {
        self.end_line as usize
    }
    /// 1-based column of the character after the token.
    #[inline]
    pub fn end_column(&self) -> usize {
        self.end_column as usize
    }
    /// Character offset of the token's start (Python parity, #278).
    #[inline]
    pub fn start_pos(&self) -> usize {
        self.start_pos as usize
    }
    /// Character offset one past the token's end.
    #[inline]
    pub fn end_pos(&self) -> usize {
        self.end_pos as usize
    }
}

impl fmt::Display for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.value)
    }
}

/// Source position metadata attached to a `Tree` node.
#[derive(Debug, Clone, Default)]
pub struct Meta {
    // Stored in `PosInt` (private width, ADR-0040); exposed via the `usize`-returning
    // accessors below.
    pub(crate) line: Option<PosInt>,
    pub(crate) column: Option<PosInt>,
    pub(crate) end_line: Option<PosInt>,
    pub(crate) end_column: Option<PosInt>,
    pub(crate) start_pos: Option<PosInt>,
    pub(crate) end_pos: Option<PosInt>,
    /// True when the rule produced zero tokens (empty match).
    pub empty: bool,
}

impl Meta {
    /// 1-based line of the node's first positioned child, if any.
    #[inline]
    pub fn line(&self) -> Option<usize> {
        self.line.map(|v| v as usize)
    }
    /// 1-based column of the node's first positioned child, if any.
    #[inline]
    pub fn column(&self) -> Option<usize> {
        self.column.map(|v| v as usize)
    }
    /// 1-based line after the node's last positioned child, if any.
    #[inline]
    pub fn end_line(&self) -> Option<usize> {
        self.end_line.map(|v| v as usize)
    }
    /// 1-based column after the node's last positioned child, if any.
    #[inline]
    pub fn end_column(&self) -> Option<usize> {
        self.end_column.map(|v| v as usize)
    }
    /// Character offset of the node's start, if any.
    #[inline]
    pub fn start_pos(&self) -> Option<usize> {
        self.start_pos.map(|v| v as usize)
    }
    /// Character offset one past the node's end, if any.
    #[inline]
    pub fn end_pos(&self) -> Option<usize> {
        self.end_pos.map(|v| v as usize)
    }

    pub fn from_children(children: &[Child]) -> Self {
        let mut meta = Meta::default();
        // Propagate position from first/last tokens
        for child in children {
            if let (None, Some(line)) = (meta.line, child_line(child)) {
                meta.line = Some(line);
                meta.column = child_column(child);
                meta.start_pos = child_start(child);
            }
        }
        for child in children.iter().rev() {
            if let Some(line) = child_end_line(child) {
                meta.end_line = Some(line);
                meta.end_column = child_end_column(child);
                meta.end_pos = child_end(child);
                break;
            }
        }
        // `empty` is true iff no child contributed a position — not only when the
        // child list is literally empty. Mirrors Python: `Meta.empty` defaults
        // `True` and is cleared only by a position-bearing first/last child, which
        // skips empty subtrees (`PropagatePositions._pp_get_meta`). So a node whose
        // sole child is a positionless empty subtree (`start: empty` / `empty:` on
        // `""`) stays `empty=true`, matching Python (#337/H10), while a node with any
        // position-bearing child clears it via the `line` set above. The
        // `propagate_positions` widen pass (`tree_builder::ContainerSpan::widen_meta`)
        // only ever *clears* `empty` on a positioned pre-filter child, so this base
        // computation is the deciding site for the positionless case.
        meta.empty = meta.line.is_none();
        meta
    }
}

fn child_line(c: &Child) -> Option<PosInt> {
    match c {
        Child::Token(t) if t.line > 0 => Some(t.line),
        Child::Tree(t) => t.meta.line,
        _ => None,
    }
}
fn child_column(c: &Child) -> Option<PosInt> {
    match c {
        Child::Token(t) if t.column > 0 => Some(t.column),
        Child::Tree(t) => t.meta.column,
        _ => None,
    }
}
fn child_start(c: &Child) -> Option<PosInt> {
    match c {
        Child::Token(t) => Some(t.start_pos),
        Child::Tree(t) => t.meta.start_pos,
        Child::None => None,
    }
}
fn child_end_line(c: &Child) -> Option<PosInt> {
    match c {
        Child::Token(t) if t.end_line > 0 => Some(t.end_line),
        Child::Tree(t) => t.meta.end_line,
        _ => None,
    }
}
fn child_end_column(c: &Child) -> Option<PosInt> {
    match c {
        Child::Token(t) if t.end_column > 0 => Some(t.end_column),
        Child::Tree(t) => t.meta.end_column,
        _ => None,
    }
}
fn child_end(c: &Child) -> Option<PosInt> {
    match c {
        Child::Token(t) => Some(t.end_pos),
        Child::Tree(t) => t.meta.end_pos,
        Child::None => None,
    }
}

/// A child of a `Tree` node — a subtree, a leaf token, or a `None` placeholder.
///
/// `None` placeholders are inserted for absent `[...]` groups when the parser is
/// built with `maybe_placeholders` (mirroring Python Lark's `None` children).
#[derive(Debug, Clone)]
pub enum Child {
    Tree(Tree),
    Token(Token),
    None,
}

impl Child {
    pub fn as_tree(&self) -> Option<&Tree> {
        match self {
            Child::Tree(t) => Some(t),
            _ => None,
        }
    }

    pub fn as_token(&self) -> Option<&Token> {
        match self {
            Child::Token(t) => Some(t),
            _ => None,
        }
    }

    pub fn is_tree(&self) -> bool {
        matches!(self, Child::Tree(_))
    }
    pub fn is_token(&self) -> bool {
        matches!(self, Child::Token(_))
    }
}

impl From<Tree> for Child {
    fn from(t: Tree) -> Self {
        Child::Tree(t)
    }
}
impl From<Token> for Child {
    fn from(t: Token) -> Self {
        Child::Token(t)
    }
}

impl fmt::Display for Child {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Child::Tree(t) => write!(f, "{}", t),
            Child::Token(t) => write!(f, "{}", t.value),
            Child::None => write!(f, "None"),
        }
    }
}

/// A node in the parse tree.
///
/// `data` is the rule name or alias that produced this node.
/// `children` are the sub-nodes or tokens.
///
/// `Drop` and `Clone` are implemented manually with explicit worklists (#151):
/// the compiler-derived glue recurses to tree depth, and a parse result is as
/// deep as the input is nested (e.g. `a: X a | X` over a long input), so the
/// derived glue overflows small native stacks — notably WASM's (#47). With
/// these impls, no engine or caller code path recurses to input depth.
#[derive(Debug)]
pub struct Tree {
    pub data: String,
    pub children: Vec<Child>,
    pub meta: Meta,
}

impl Drop for Tree {
    fn drop(&mut self) {
        // Fast path: leaf-only children drop in place — no recursion possible.
        // This keeps the hot path (every token-holding node) at one scan.
        if !self.children.iter().any(Child::is_tree) {
            return;
        }
        // Worklist of whole `children` vectors (3-word moves, never per-element
        // copies). Invariant: a vector is pushed only if it contains a subtree
        // with sub-subtrees; every `Tree` value therefore reaches its own
        // (recursive) drop with empty or leaf-only children, where the fast
        // path returns immediately — so native depth stays constant.
        let mut stack: Vec<Vec<Child>> = vec![std::mem::take(&mut self.children)];
        while let Some(mut vec) = stack.pop() {
            for child in vec.iter_mut() {
                if let Child::Tree(t) = child {
                    if t.children.iter().any(Child::is_tree) {
                        stack.push(std::mem::take(&mut t.children));
                    }
                }
            }
            // `vec` drops here; every tree in it is now empty or leaf-only.
        }
    }
}

impl Clone for Tree {
    fn clone(&self) -> Self {
        // Explicit-frame deep copy: one heap frame per open node instead of one
        // native frame per tree level.
        struct Frame<'a> {
            src: std::slice::Iter<'a, Child>,
            data: String,
            meta: Meta,
            out: Vec<Child>,
        }
        fn frame(t: &Tree) -> Frame<'_> {
            Frame {
                src: t.children.iter(),
                data: t.data.clone(),
                meta: t.meta.clone(),
                out: Vec::with_capacity(t.children.len()),
            }
        }
        let mut stack = vec![frame(self)];
        loop {
            let top = stack
                .last_mut()
                .expect("clone stack never empties mid-walk");
            match top.src.next() {
                Some(Child::Tree(t)) => stack.push(frame(t)),
                Some(Child::Token(tok)) => top.out.push(Child::Token(tok.clone())),
                Some(Child::None) => top.out.push(Child::None),
                None => {
                    let done = stack.pop().expect("just peeked");
                    let tree = Tree {
                        data: done.data,
                        children: done.out,
                        meta: done.meta,
                    };
                    match stack.last_mut() {
                        Some(parent) => parent.out.push(Child::Tree(tree)),
                        None => return tree,
                    }
                }
            }
        }
    }
}

impl Tree {
    pub fn new(data: impl Into<String>, children: Vec<Child>) -> Self {
        let meta = Meta::from_children(&children);
        Tree {
            data: data.into(),
            children,
            meta,
        }
    }

    /// Iterate all subtrees depth-first (post-order).
    pub fn iter_subtrees(&self) -> impl Iterator<Item = &Tree> {
        IterSubtrees {
            stack: vec![self],
            result: Vec::new(),
        }
        .collect_all()
    }

    /// Iterate all leaf tokens.
    pub fn scan_values(&self) -> impl Iterator<Item = &Token> {
        self.iter_subtrees()
            .flat_map(|t| t.children.iter().filter_map(|c| c.as_token()))
    }

    /// Pretty-print the tree with indentation.
    pub fn pretty(&self, indent: usize) -> String {
        let pad = "  ".repeat(indent);
        let mut out = format!("{}Tree({}", pad, self.data);
        if self.children.is_empty() {
            out.push(')');
            return out;
        }
        out.push('\n');
        for child in &self.children {
            match child {
                Child::Tree(t) => out.push_str(&t.pretty(indent + 1)),
                Child::Token(tok) => {
                    out.push_str(&format!("{}  Token({}, {:?})", pad, tok.type_, tok.value))
                }
                Child::None => out.push_str(&format!("{}  None", pad)),
            }
            out.push('\n');
        }
        out.push_str(&format!("{})", pad));
        out
    }
}

impl fmt::Display for Tree {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let children: Vec<String> = self
            .children
            .iter()
            .map(|c| match c {
                Child::Tree(t) => t.to_string(),
                Child::Token(tok) => format!("Token({}, {:?})", tok.type_, tok.value),
                Child::None => "None".to_string(),
            })
            .collect();
        write!(f, "Tree({}, [{}])", self.data, children.join(", "))
    }
}

/// The result of a parse.
///
/// Usually a [`Tree`], but a start rule that collapses via `?rule` (expand1) to a
/// single token yields that bare [`Token`] — exactly as Python Lark does (e.g.
/// `?start: NUMBER` on input `"1"` returns the `NUMBER` token, not a wrapping
/// tree). Likewise a `?start` whose sole alternative is an absent `[...]`
/// placeholder collapses to a bare [`None`](ParseTree::None) — Python's literal
/// `None` result for `?start: [A]` on `""` with `maybe_placeholders` (#289). This
/// is the public return type of [`crate::Lark::parse`].
#[derive(Debug, Clone)]
pub enum ParseTree {
    Tree(Tree),
    Token(Token),
    /// A `?start` rule that collapsed a lone `maybe_placeholders` `None` to the
    /// root (e.g. `?start: [A]` on empty input). Mirrors Python Lark returning a
    /// bare `None`, which neither [`Tree`](ParseTree::Tree) nor
    /// [`Token`](ParseTree::Token) can represent (#289).
    None,
}

impl ParseTree {
    pub fn as_tree(&self) -> Option<&Tree> {
        match self {
            ParseTree::Tree(t) => Some(t),
            ParseTree::Token(_) | ParseTree::None => None,
        }
    }

    pub fn as_token(&self) -> Option<&Token> {
        match self {
            ParseTree::Token(t) => Some(t),
            ParseTree::Tree(_) | ParseTree::None => None,
        }
    }

    pub fn is_tree(&self) -> bool {
        matches!(self, ParseTree::Tree(_))
    }
    pub fn is_token(&self) -> bool {
        matches!(self, ParseTree::Token(_))
    }
    /// True when the parse collapsed to a bare `None` (Python's `None` result for a
    /// root `?start: [A]` lone-placeholder collapse, #289).
    pub fn is_none(&self) -> bool {
        matches!(self, ParseTree::None)
    }
}

impl From<Tree> for ParseTree {
    fn from(t: Tree) -> Self {
        ParseTree::Tree(t)
    }
}
impl From<Token> for ParseTree {
    fn from(t: Token) -> Self {
        ParseTree::Token(t)
    }
}

impl fmt::Display for ParseTree {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseTree::Tree(t) => write!(f, "{t}"),
            ParseTree::Token(tok) => write!(f, "Token({}, {:?})", tok.type_, tok.value),
            ParseTree::None => write!(f, "None"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deep enough that the compiler-derived `Drop`/`Clone` glue (a few native
    /// frames per tree level) overflows the default 8 MB test stack — so this
    /// test crashing is the regression signal that the manual worklist impls
    /// (#151) were lost.
    const DEPTH: usize = 200_000;

    fn deep_tree(depth: usize) -> Tree {
        let mut t = Tree::new("leaf", vec![Child::Token(Token::new("X", "x"))]);
        for _ in 0..depth {
            t = Tree::new("nest", vec![Child::Tree(t)]);
        }
        t
    }

    #[test]
    fn drop_of_deep_tree_is_iterative() {
        drop(deep_tree(DEPTH));
    }

    #[test]
    fn clone_of_deep_tree_is_iterative() {
        let t = deep_tree(DEPTH);
        let copy = t.clone();
        // The clone is structurally complete: same nesting depth, leaf intact.
        let mut cur: &Tree = &copy;
        let mut depth = 0usize;
        while let Some(Child::Tree(next)) = cur.children.first() {
            cur = next;
            depth += 1;
        }
        assert_eq!(depth, DEPTH);
        assert_eq!(cur.data, "leaf");
        assert_eq!(
            cur.children[0].as_token().map(|t| t.value.as_str()),
            Some("x")
        );
    }
}

// Post-order depth-first iterator helper.
struct IterSubtrees<'a> {
    stack: Vec<&'a Tree>,
    result: Vec<&'a Tree>,
}

impl<'a> IterSubtrees<'a> {
    fn collect_all(mut self) -> std::vec::IntoIter<&'a Tree> {
        while let Some(tree) = self.stack.pop() {
            self.result.push(tree);
            for child in &tree.children {
                if let Child::Tree(t) = child {
                    self.stack.push(t);
                }
            }
        }
        self.result.into_iter()
    }
}
