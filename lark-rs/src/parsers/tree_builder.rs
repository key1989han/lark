//! The shared tree-builder and the `OutputBuilder` seam.
//!
//! Turning a reduced rule plus its child values into the parent's value is the
//! one place where Lark's tree-shaping semantics live: punctuation filtering,
//! transparent (`_rule` / `__anon_*`) splicing, `?rule` (`expand1`), aliases, and
//! `maybe_placeholders`. Both parser backends must apply these *identically* — the
//! LALR reducer today, and the Earley forest-walk that materializes trees from the
//! SPPF tomorrow. Keeping the logic here, rather than inline in either driver,
//! guarantees there is exactly one definition of "what tree does this rule build",
//! and makes it the single chokepoint where the node representation could later
//! change (e.g. an arena or interned labels) without touching the parsers.
//!
//! ## Architecture: `OutputBuilder` seam (issue #227)
//!
//! The `OutputBuilder` trait is the internal seam between the engine's tree-shaping
//! logic and the value it produces. The engine drives all shaping decisions — token
//! filtering, transparent splicing, `expand1` unwrapping, placeholder insertion —
//! through the [`Slot`] discriminant on the parse stack. The `OutputBuilder` only
//! ever sees the *flat, already-shaped* children and builds the final value from
//! them.
//!
//! [`TreeOutputBuilder`] is the default (and currently only) implementation:
//! `Value = Child`, producing the same `Tree`/`Token` parse trees as before.
//! Future implementations (e.g. a semantic-action builder) will use a different
//! `Value` type but share the same engine-driven shaping. The trait is internal
//! (not re-exported from the public API); the public trait shape is deferred to
//! issue #231.

use crate::grammar::intern::{CompiledRule, SymbolId, SymbolTable};
use crate::perf;
use crate::tree::{Child, Meta, ParseTree, PosInt, Token, Tree};

// ─── Root Slot → ParseTree ──────────────────────────────────────────────────

/// Convert the final root [`Slot`] off an engine's value stack into the public
/// [`ParseTree`] result, handling the three shapes every backend agrees on:
///
/// - `Slot::Tree` → [`ParseTree::Tree`], `Slot::Token` → [`ParseTree::Token`].
/// - A **lone-`None`** `Slot::Inline([Child::None])` → [`ParseTree::None`]. A root
///   `?start` rule whose sole alternative is an absent `maybe_placeholders` `[...]`
///   collapses its placeholder `None` through `?`-expand1 to `Inline([None])` (RC9
///   in [`shape`](TreeOutputBuilder::shape)); Python Lark returns a bare `None`
///   there (`?start: [A]` on `""`), so all three backends must too (#289, ADR-0033).
///   A non-`?` start rule never reaches `Inline`, so this stays *not*-more-permissive
///   (ADR-0017).
///
/// Centralized here so the lone-`None` carve-out has **one** definition shared by
/// the LALR, CYK, and Earley roots, rather than hand-kept copies that could drift.
/// Any **other** `Inline` shape (a non-lone-`None` collapse) is structurally
/// impossible at a start root, so its children are returned as `Err(children)` for
/// the caller to resolve under its own residual policy — LALR rejects it as
/// no-parse, CYK wraps it in a start-named node, and Earley's `forest_to_tree`
/// likewise wraps it in a start-named node — keeping each backend's existing
/// behaviour byte-for-byte.
///
/// One copy of this carve-out remains *un*-migrated by design: `standalone/runtime.rs`
/// keeps its own (ADR-0008 — it can't share in-tree code), so a change to the
/// lone-`None` contract (#289/ADR-0033) must still be mirrored there.
pub(crate) fn root_slot_to_parse_tree(value: Slot) -> Result<ParseTree, Vec<Child>> {
    match value {
        Slot::Tree(t) => Ok(ParseTree::Tree(t)),
        Slot::Token(tok) => Ok(ParseTree::Token(tok)),
        Slot::Inline(cs) if cs.len() == 1 && matches!(cs[0], Child::None) => Ok(ParseTree::None),
        Slot::Inline(cs) => Err(cs),
    }
}

// ─── Slot: the engine's internal stack currency ─────────────────────────────

/// A semantic value on the parse stack. This is the engine's internal currency:
/// it carries the shaping discriminant (`Token`/`Tree` vs `Inline`) that drives
/// transparent splicing, so the engine can flatten `Inline` children before
/// handing the shaped result to the [`OutputBuilder`]. The builder never sees
/// `Inline` — only the already-flattened child list.
///
/// `Clone` so the Earley forest-walk can memoize a shared SPPF node's assembled
/// value (a DAG node is reachable by many parents).
#[derive(Clone)]
pub(crate) enum Slot {
    Token(Token),
    Tree(Tree),
    /// Children of a transparent (`_rule` / `__anon_*`) reduction, to be spliced
    /// into the parent's child list rather than wrapped in a node.
    Inline(Vec<Child>),
}

// Backward-compat alias: all internal code that used `NodeValue` keeps compiling.
pub(crate) type NodeValue = Slot;

// ─── OutputContext: interned-id → name resolution for builders ──────────────

/// A cheap borrow of the metadata a builder needs to resolve an interned rule
/// index / [`SymbolId`] back to the name world Python Lark dispatches on. Passed
/// per-call to the [`OutputBuilder`] methods (ADR-0029 fork 6) so name resolution
/// stays lazy and the engine hot path stays interned — "an array index per token,
/// never a string hash." Holds no owned data.
///
/// Public so a user [`OutputBuilder`] can resolve ids to names; constructed only by
/// the engine (`new` is crate-internal).
pub struct OutputContext<'g> {
    rules: &'g [CompiledRule],
    symbols: &'g SymbolTable,
}

impl<'g> OutputContext<'g> {
    pub(crate) fn new(rules: &'g [CompiledRule], symbols: &'g SymbolTable) -> Self {
        OutputContext { rules, symbols }
    }

    /// The callback name Python's `create_callback` dispatches on for a reduction of
    /// `rule_idx`: the rule's `-> alias`, else its origin/template name. This is
    /// exactly the `tree_name` the loader already resolved (alias-else-origin,
    /// template source folded in at lowering) — the same string `Tree::data`
    /// carries, so a builder keyed by callback name matches Python without the
    /// engine re-deriving anything.
    pub fn callback_name(&self, rule_idx: usize) -> &'g str {
        &self.rules[rule_idx].tree_name
    }

    /// The rule's explicit `-> alias`, if any; `callback_name` falls back to the
    /// origin name when this is `None`.
    pub fn rule_alias(&self, rule_idx: usize) -> Option<&'g str> {
        self.rules[rule_idx].alias.as_deref()
    }

    /// The terminal type name for a shifted token's interned id (the `Token::type_`
    /// world), e.g. `"NUMBER"`.
    pub fn terminal_name(&self, terminal: SymbolId) -> &'g str {
        self.symbols.name(terminal)
    }
}

// ─── OutputBuilder trait ────────────────────────────────────────────────────

/// The internal seam between engine-driven tree shaping and value construction.
///
/// The engine handles all shaping decisions (token filtering, transparent
/// splicing, `expand1` unwrapping, placeholder insertion) and calls the builder
/// only with the flat, already-shaped children. The builder's job is to wrap
/// those children into its `Value` type.
///
/// The public trait shape is ADR-0029 (resolved) + ADR-0038 (the generic-`Value`
/// placeholder / `Discard` / `token`-input edges). The engine funnels every LALR
/// reduction through this seam; [`TreeOutputBuilder`] is the default `Value = Child`
/// backend. `Lark::parse_into` drives an arbitrary builder; `parse()` is the tree
/// backend. Earley/CYK stay post-parse (ADR-0029 fork 4) and keep the concrete
/// tree-shaping path below.
///
/// `'i` ties a borrowing builder's `Value` to the parse `input` (owned backends
/// like the tree ignore it). The engine performs *all* tree shaping — punctuation
/// filtering, transparent/anon splicing, `expand1`, placeholder insertion — before
/// calling [`reduce`](OutputBuilder::reduce), so the builder only ever sees the
/// flat, already-shaped child list.
pub trait OutputBuilder<'i> {
    /// The value carried on the parse stack (Yacc's semantic value).
    type Value;

    /// A shifted terminal. The engine hands the lexer's token record (interned
    /// `type_id`, `span`, precomputed positions, and — in this C7 intermediate —
    /// the owned value; ADR-0038 §3) plus the whole `input`, so a span backend can
    /// borrow the token's text out of `input`. Note `token.start_pos()`/`end_pos()`
    /// are **character** indices (Python parity, #278), not byte offsets, so slice
    /// after mapping them to byte offsets (as `SpanTreeBuilder` does); they are not a
    /// direct `&input[..]` range. `ctx` resolves the interned
    /// terminal id to its Python-side name when the builder needs it. Runs for
    /// *every* shifted terminal — the parse stack always needs a value (this is
    /// engine token materialization, lower-level than Python's *visible* terminal
    /// callback; see RFC §5).
    fn token(&mut self, token: Token, input: &'i str, ctx: &OutputContext) -> Self::Value;

    /// A completed reduction of `rule` over its already-shaped `children` (filtered,
    /// transparent-spliced, placeholders inserted — the engine did all shaping).
    /// `meta` is the node's engine-computed span/position (subsumes
    /// `propagate_positions`); it is the single source of truth, so no backend
    /// recomputes positions and none can diverge. `ctx.callback_name(rule)` resolves
    /// the name Python's `create_callback` dispatches on (alias → template → origin).
    fn reduce(
        &mut self,
        rule: usize,
        children: &mut Vec<Self::Value>,
        meta: &Meta,
        ctx: &OutputContext,
    ) -> Self::Value;

    /// The value for an absent `maybe_placeholders` optional (`[...]`). Python Lark
    /// inserts a literal `None` child; a builder maps that to its own "absent"
    /// value. Default: unreachable unless the grammar uses `maybe_placeholders` — a
    /// builder used with such a grammar MUST override this (ADR-0038 §1).
    fn placeholder(&mut self, _ctx: &OutputContext) -> Self::Value {
        panic!(
            "OutputBuilder::placeholder called: this builder was used with a \
             maybe_placeholders grammar but does not implement placeholder()"
        );
    }

    /// Discard hook (Python's `Discard` sentinel). Default: nothing discards, so the
    /// engine skips the check entirely and non-discarding builders pay zero
    /// (ADR-0029 fork 1). The engine drops discarded children *after* placeholder
    /// insertion (ADR-0038 §2).
    #[inline]
    fn is_discard(&self, _value: &Self::Value) -> bool {
        false
    }
}

// ─── TreeOutputBuilder: the default implementation ──────────────────────────

/// The default [`OutputBuilder`] implementation: builds `Tree`/`Token`/`Child`
/// parse trees, identical to the output before the seam was introduced.
///
/// Applies a rule's tree-shaping options to its assembled children. Borrows the
/// compiled rules from the parse table; holds no mutable state, so a fresh one can
/// be made per reduction for free. Token filtering is per *rule position* (each
/// rule carries its own keep mask), not per terminal — see [`CompiledRule::filter_pos`].
pub(crate) struct TreeOutputBuilder<'g> {
    rules: &'g [CompiledRule],
    /// When true (Python Lark's `propagate_positions=True`), a node's `meta` span
    /// is derived from its rule's **pre-filter** children, so filtered punctuation
    /// (`"(" A ")"`) contributes to the container span. Mirrors Python's
    /// `PropagatePositions`, which wraps the node builder *outside* the child
    /// filter and so sees the unfiltered children (bug-bounty H6-5, #402). When
    /// false the span is computed from the already-filtered children, exactly as
    /// before (lark-rs always populates `meta`; the flag only changes *which*
    /// children feed the span).
    propagate_positions: bool,
}

impl<'g> TreeOutputBuilder<'g> {
    /// Build a tree node from a rule's tree name and its flat children. Inherent
    /// (not the trait `reduce`) because the concrete tree-shaping path
    /// ([`assemble`](Self::assemble)/[`shape`](Self::shape)) — still driven by
    /// LALR-recovery, Earley, and CYK — computes the node `meta` from the children
    /// via `Tree::new`, whereas the value-parametric [`OutputBuilder::reduce`] is
    /// handed the engine-computed `meta`.
    fn build_node(&self, tree_name: &str, children: Vec<Child>) -> Child {
        // Output-shape counter (#230): one `Tree` node materialized. A future
        // tree-bypassing span backend (C8) builds none of these.
        perf::add_tree_node_built();
        Child::Tree(Tree::new(tree_name.to_string(), children))
    }

    /// Build a token value from a shifted terminal (inherent — the concrete path's
    /// analog of [`OutputBuilder::token`]).
    fn build_token(&self, token: Token) -> Child {
        // Output-shape counter (#230): the owned token value bytes this backend
        // copies into the output. The span backend (C8) keeps offsets, not strings,
        // and drives this to 0.
        perf::add_token_value_string_bytes(token.value.len() as u64);
        Child::Token(token)
    }
}

impl<'i, 'g> OutputBuilder<'i> for TreeOutputBuilder<'g> {
    type Value = Child;

    fn token(&mut self, token: Token, _input: &'i str, _ctx: &OutputContext) -> Child {
        // Output-shape counter (#230): owned value bytes copied into the tree.
        perf::add_token_value_string_bytes(token.value.len() as u64);
        Child::Token(token)
    }

    fn reduce(
        &mut self,
        rule: usize,
        children: &mut Vec<Child>,
        meta: &Meta,
        ctx: &OutputContext,
    ) -> Child {
        // Output-shape counter (#230): one `Tree` node materialized.
        perf::add_tree_node_built();
        let children = std::mem::take(children);
        // The engine already computed `meta` (the single source of truth), so use it
        // directly rather than re-deriving from children as `Tree::new` would — that
        // is what keeps this backend byte-identical to the concrete path under
        // `propagate_positions` without a second, drift-prone computation.
        Child::Tree(Tree {
            data: ctx.callback_name(rule).to_string(),
            children,
            meta: meta.clone(),
        })
    }

    fn placeholder(&mut self, _ctx: &OutputContext) -> Child {
        Child::None
    }
}

impl<'g> TreeOutputBuilder<'g> {
    pub fn new(rules: &'g [CompiledRule]) -> Self {
        TreeOutputBuilder {
            rules,
            propagate_positions: false,
        }
    }

    /// As [`new`](Self::new), with Python Lark's `propagate_positions` flag.
    pub fn with_propagate_positions(rules: &'g [CompiledRule], propagate_positions: bool) -> Self {
        TreeOutputBuilder {
            rules,
            propagate_positions,
        }
    }

    /// Build the value the parent sees when `rule_idx` reduces over `child_values`
    /// (in left-to-right order, one per expansion symbol). This is backend-agnostic:
    /// the LALR reducer drains them off its value stack; an Earley walk collects
    /// them from a forest node.
    pub fn assemble(&self, rule_idx: usize, child_values: Vec<Slot>) -> Slot {
        // Output-shape counter (#230): one rule reduction shaped into a value. For
        // a known LALR input this equals the parser's user-rule reduction count (the
        // augmented `$root` accept does not route through `assemble`).
        perf::add_semantic_reduce_call();
        // Flatten child values into the parent's child list: drop filtered
        // punctuation tokens (unless the rule keeps all tokens), and splice the
        // children of an inlined (transparent) sub-rule in place. Inlined children
        // were already filtered when their own rule reduced. The child at index `i`
        // corresponds to expansion symbol `i`, so its keep/drop is `filter_pos[i]`.
        //
        // Under `propagate_positions`, the node's span must come from the *pre*-
        // filter children (so a filtered `"("`/`")"` still bounds the span — #402),
        // so accumulate the container Meta over every value as it streams by, kept
        // or dropped, before the kept list is shaped into a node.
        let mut children: Vec<Child> = Vec::new();
        let mut container = ContainerSpan::new();
        for (i, value) in child_values.into_iter().enumerate() {
            for _ in 0..self.nones_at(rule_idx, i) {
                children.push(Child::None);
            }
            match value {
                Slot::Token(t) => {
                    if self.propagate_positions {
                        container.observe_token(&t);
                    }
                    if self.keep_token(rule_idx, i) {
                        // Materialize the kept terminal through the seam
                        // (`Value = Child` → `Child::Token`), the same call the
                        // engine's shift path uses, so token construction has a
                        // single definition (ADR-0027).
                        children.push(self.build_token(t));
                    }
                }
                Slot::Tree(t) => {
                    if self.propagate_positions {
                        container.observe_meta(&t.meta);
                    }
                    children.push(Child::Tree(t));
                }
                Slot::Inline(cs) => {
                    if self.propagate_positions {
                        for c in &cs {
                            container.observe_child(c);
                        }
                    }
                    children.extend(cs);
                }
            }
        }
        self.shape_with_container(rule_idx, children, container)
    }

    /// Number of `None` placeholders a distributed absent `[...]` left before
    /// expansion position `gap` of `rule_idx` (position `expansion.len()` is
    /// trailing — appended by [`shape`](Self::shape)). 0 for ordinary rules.
    pub fn nones_at(&self, rule_idx: usize, gap: usize) -> usize {
        self.rules[rule_idx]
            .options
            .nones_before
            .get(gap)
            .copied()
            .unwrap_or(0)
    }

    /// Whether the token at expansion position `pos` of `rule_idx` is kept (not a
    /// filtered punctuation terminal), honoring `keep_all_tokens`. Split out of
    /// [`assemble`] so a streaming forest walk can apply the *same* per-position
    /// filtering rule without first materializing a per-symbol value list.
    pub fn keep_token(&self, rule_idx: usize, pos: usize) -> bool {
        let rule = &self.rules[rule_idx];
        rule.options.keep_all_tokens || !rule.filter_pos.get(pos).copied().unwrap_or(false)
    }

    /// Turn a rule's already-filtered child list into the value its parent sees:
    /// append `maybe_placeholders`, then splice (transparent), unwrap (`expand1`),
    /// or wrap in a [`Tree`]. The tail half of [`assemble`], shared with the Earley
    /// streaming walk so both produce identical shaping.
    ///
    /// The streaming Earley walk filters tokens *before* `shape`, so the dropped
    /// punctuation is gone here — under `propagate_positions` that caller threads
    /// the pre-filter container span through
    /// [`shape_with_container`](Self::shape_with_container) instead. With the flag
    /// off (the common case), the two are identical.
    pub fn shape(&self, rule_idx: usize, children: Vec<Child>) -> Slot {
        self.shape_with_container(rule_idx, children, ContainerSpan::new())
    }

    /// As [`shape`](Self::shape), but with the rule's pre-filter container span
    /// (the span over *all* children including filtered punctuation). Under
    /// `propagate_positions`, a non-transparent node's `meta` span is widened to
    /// this container so a filtered `"("`/`")"` bounds the parent (#402). An empty
    /// container (or the flag off) leaves the post-filter span untouched.
    pub fn shape_with_container(
        &self,
        rule_idx: usize,
        mut children: Vec<Child>,
        container: ContainerSpan,
    ) -> Slot {
        let rule = &self.rules[rule_idx];

        // maybe_placeholders: an empty `[...]` production emits one `None` per
        // kept symbol of its widest alternative; a distributed absent `[...]` at
        // the end of this alternative appends its trailing placeholders.
        for _ in 0..rule.options.placeholder_count {
            children.push(Child::None);
        }
        for _ in 0..self.nones_at(rule_idx, rule.expansion.len()) {
            children.push(Child::None);
        }

        if rule.transparent {
            // `_rule` / `__anon_*`: splice children into the parent. No node is
            // built, so there is no `meta` to widen — the spliced children carry
            // their own positions up, exactly as Python's `_pp_get_meta` reads them.
            Slot::Inline(children)
        } else if rule.options.expand1 && rule.alias.is_none() && children.len() == 1 {
            // `?rule` with a single child: return that child directly. A lone `None`
            // placeholder (`?w: [A]` on the absent branch) collapses exactly like a
            // real single child — Python yields `start[None]`, not `start[w[None]]`
            // (bounty RC9; the `?` collapse is purely arity-1, never value-typed).
            // An empty `?` rule (`?w: A?` with zero children) is *not* len==1 and so
            // correctly keeps its wrapper (`start[w[]]`).
            //
            // No container widening here: Python's `PropagatePositions` only sets
            // meta when its node builder returns a `Tree` (`isinstance(res, Tree)`);
            // an `ExpandSingleChild` collapse to a bare token/tree skips it, so the
            // collapsed value keeps its own span.
            match children.pop().unwrap() {
                Child::Tree(t) => Slot::Tree(t),
                Child::Token(t) => Slot::Token(t),
                // A bare `None` has no Token/Tree slot; carry it as a single-None
                // inline so the parent splices exactly one `Child::None` in place.
                Child::None => Slot::Inline(vec![Child::None]),
            }
        } else {
            // Build the node through the seam (`Value = Child` → a `Child::Tree`),
            // so node construction has a single definition shared with every future
            // backend (ADR-0027). `propagate_positions` then widens the freshly
            // built node's `meta` to its pre-filter container span (#402); the
            // widen is a property of *this* tree backend, applied after the seam
            // hands back the node.
            let mut tree = match self.build_node(&rule.tree_name, children) {
                Child::Tree(tree) => tree,
                // `TreeOutputBuilder::build_node` always returns `Child::Tree`.
                other => unreachable!("build_node must produce a tree node, got {other:?}"),
            };
            if self.propagate_positions {
                container.widen_meta(&mut tree.meta);
            }
            Slot::Tree(tree)
        }
    }
}

/// The container span of a rule's **pre-filter** children — the first/last
/// positioned child's start/end, including filtered punctuation tokens that the
/// tree drops. Mirrors Python Lark's `PropagatePositions._pp_get_meta` (which runs
/// outside the child filter), the regression net for bug-bounty H6-5 / #402.
///
/// Built by streaming every child value past [`observe_token`](Self::observe_token)
/// / [`observe_meta`](Self::observe_meta) / [`observe_child`](Self::observe_child)
/// in left-to-right order, then applied with [`widen_meta`](Self::widen_meta).
#[derive(Default, Clone)]
pub(crate) struct ContainerSpan {
    first: Option<Meta>,
    last: Option<Meta>,
}

impl ContainerSpan {
    pub fn new() -> Self {
        ContainerSpan::default()
    }

    /// Record a positioned child's `meta`. The first positioned child fixes the
    /// start fields; every positioned child updates the (so-far) last, so the final
    /// `last` fixes the end fields — matching Python's first/`reversed`-first scan.
    fn observe(&mut self, meta: Meta) {
        if self.first.is_none() {
            self.first = Some(meta.clone());
        }
        self.last = Some(meta);
    }

    /// Observe a (pre-filter) token — it contributes its own start/end.
    pub fn observe_token(&mut self, t: &Token) {
        self.observe(Meta {
            line: Some(t.line),
            column: Some(t.column),
            end_line: Some(t.end_line),
            end_column: Some(t.end_column),
            start_pos: Some(t.start_pos),
            end_pos: Some(t.end_pos),
            empty: false,
        });
    }

    /// Observe a (pre-filter) subtree's `meta`. A positionless empty subtree
    /// contributes nothing (Python skips `c.meta.empty` children in `_pp_get_meta`).
    pub fn observe_meta(&mut self, meta: &Meta) {
        if !meta.empty && meta.line.is_some() {
            self.observe(meta.clone());
        }
    }

    /// Observe a spliced (transparent-rule) child — token or subtree.
    pub fn observe_child(&mut self, c: &Child) {
        match c {
            Child::Token(t) => self.observe_token(t),
            Child::Tree(t) => self.observe_meta(&t.meta),
            Child::None => {}
        }
    }

    /// Widen `meta`'s span to the container: set its start fields from the first
    /// positioned child and its end fields from the last, when the container saw
    /// any positioned child. A node with no positioned pre-filter child (an empty
    /// production) is left untouched.
    fn widen_meta(&self, meta: &mut Meta) {
        if let Some(first) = &self.first {
            meta.line = first.line;
            meta.column = first.column;
            meta.start_pos = first.start_pos;
            meta.empty = false;
        }
        if let Some(last) = &self.last {
            meta.end_line = last.end_line;
            meta.end_column = last.end_column;
            meta.end_pos = last.end_pos;
            meta.empty = false;
        }
    }
}

// Backward-compat: keep `TreeBuilder` as a type alias so any code outside the
// core three files (earley.rs, cyk.rs, lalr.rs) that references it by name
// keeps compiling without a rename. This alias is crate-internal only.
pub(crate) type TreeBuilder<'g> = TreeOutputBuilder<'g>;

// ─── Value-parametric shaping (the `parse_into` seam, #232 C7) ──────────────────
//
// The generic analog of [`TreeOutputBuilder::assemble`] + [`shape_with_container`]:
// the *same* tree-shaping rules (per-position punctuation filtering, transparent /
// anon splicing, `expand1`, `maybe_placeholders`, `propagate_positions` container
// widening), but over an [`OutputBuilder`]'s opaque `Value` instead of `Child`. The
// engine tracks each value's `Meta` + [`GTag`] on the stack (never inspecting the
// value), so a semantic/span backend never materializes a generic tree.
//
// This parallels the concrete `assemble`/`shape` above; the two are kept
// byte-identical for `Value = Child` by the `parse_into(tree) == parse()` relative
// oracle over the full compliance corpus, and the ADR-0015 follow-up to fold them
// onto one definition rides the C7 PR. Earley/CYK stay on the concrete path
// (post-parse, ADR-0029 fork 4).

/// Origin tag for a generic stack value — the shaping decisions the opaque
/// `Value` can't answer: which `child_*` position rule applies for meta, whether an
/// `expand1` lone child is a `None` placeholder, and the root ParseTree variant.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum GTag {
    Token,
    Tree,
    None,
}

/// One value on the generic parse stack: the builder value plus the engine-tracked
/// `meta` and `tag`, kept beside the value so shaping never inspects it.
pub(crate) struct GElem<V> {
    pub value: V,
    pub meta: Meta,
    pub tag: GTag,
}

/// The generic stack currency (the `Value`-parametric analog of [`Slot`]): a single
/// value, or the children of a transparent rule to splice into the parent.
pub(crate) enum GSlot<V> {
    Value(GElem<V>),
    Inline(Vec<GElem<V>>),
}

/// The `Meta` a shifted token contributes (all fields present, unguarded — the raw
/// form `ContainerSpan::observe_token` uses; the `child_*` guards below re-apply the
/// `> 0` line/column rules).
pub(crate) fn meta_from_token(t: &Token) -> Meta {
    Meta {
        line: Some(t.line),
        column: Some(t.column),
        end_line: Some(t.end_line),
        end_column: Some(t.end_column),
        start_pos: Some(t.start_pos),
        end_pos: Some(t.end_pos),
        empty: false,
    }
}

// Tag-aware mirrors of `tree.rs`'s `child_*` position helpers, so
// `node_meta_from_elems` reproduces `Meta::from_children` byte-for-byte over
// `(Meta, GTag)` pairs (Token guards `> 0` on line/column; Tree reads meta directly;
// None contributes nothing).
fn gc_line(m: &Meta, tag: GTag) -> Option<PosInt> {
    match tag {
        GTag::Token => m.line.filter(|&l| l > 0),
        GTag::Tree => m.line,
        GTag::None => Option::None,
    }
}
fn gc_column(m: &Meta, tag: GTag) -> Option<PosInt> {
    match tag {
        GTag::Token => m.column.filter(|&c| c > 0),
        GTag::Tree => m.column,
        GTag::None => Option::None,
    }
}
fn gc_start(m: &Meta, tag: GTag) -> Option<PosInt> {
    match tag {
        GTag::None => Option::None,
        _ => m.start_pos,
    }
}
fn gc_end_line(m: &Meta, tag: GTag) -> Option<PosInt> {
    match tag {
        GTag::Token => m.end_line.filter(|&l| l > 0),
        GTag::Tree => m.end_line,
        GTag::None => Option::None,
    }
}
fn gc_end_column(m: &Meta, tag: GTag) -> Option<PosInt> {
    match tag {
        GTag::Token => m.end_column.filter(|&c| c > 0),
        GTag::Tree => m.end_column,
        GTag::None => Option::None,
    }
}
fn gc_end(m: &Meta, tag: GTag) -> Option<PosInt> {
    match tag {
        GTag::None => Option::None,
        _ => m.end_pos,
    }
}

/// The `Meta` a node builds from its (post-filter, placeholder-inclusive) children —
/// the exact contract of [`Meta::from_children`], reproduced over `GElem`s so the
/// engine (not the opaque value) is the single source of a node's span.
fn node_meta_from_elems<V>(elems: &[GElem<V>]) -> Meta {
    let mut meta = Meta::default();
    for e in elems {
        if let Some(line) = gc_line(&e.meta, e.tag) {
            meta.line = Some(line);
            meta.column = gc_column(&e.meta, e.tag);
            meta.start_pos = gc_start(&e.meta, e.tag);
            break;
        }
    }
    for e in elems.iter().rev() {
        if let Some(line) = gc_end_line(&e.meta, e.tag) {
            meta.end_line = Some(line);
            meta.end_column = gc_end_column(&e.meta, e.tag);
            meta.end_pos = gc_end(&e.meta, e.tag);
            break;
        }
    }
    meta.empty = meta.line.is_none();
    meta
}

impl ContainerSpan {
    /// Observe a generic pre-filter child (the `GElem` analog of `observe_child`):
    /// a token contributes unconditionally, a subtree only if positioned + non-empty,
    /// a placeholder never.
    fn observe_gelem<V>(&mut self, e: &GElem<V>) {
        match e.tag {
            GTag::Token => self.observe(e.meta.clone()),
            GTag::Tree => self.observe_meta(&e.meta),
            GTag::None => {}
        }
    }
}

#[inline]
fn keep_token_pos(rule: &CompiledRule, pos: usize) -> bool {
    rule.options.keep_all_tokens || !rule.filter_pos.get(pos).copied().unwrap_or(false)
}

#[inline]
fn nones_at_gap(rule: &CompiledRule, gap: usize) -> usize {
    rule.options.nones_before.get(gap).copied().unwrap_or(0)
}

/// Shape one reduction of `rule` over its child stack slots into the parent value —
/// the value-parametric mirror of `assemble` + `shape_with_container`. `builder`
/// mints token/node/placeholder values; the engine owns every shaping decision.
///
/// ## Child-buffer reuse (perf spike 2026-07-02, the #583 reuse frontier)
///
/// Two structural facts make most reductions buffer-free:
///
/// * **`expand1` collapse is arity-1 by definition**, so a `?rule` whose single
///   slot is a plain kept value needs no child list at all — the slot is returned
///   unchanged (the fast path below), where the old path allocated and dropped a
///   one-element buffer per collapse.
/// * **EBNF `+`/`*` lower to *left*-recursive transparent helpers**
///   (`_p: item | _p item`), so the growing spliced child list is always the
///   *first* contribution to the next reduction. When nothing precedes it, the
///   reduction **steals** that `Inline` buffer and pushes the remaining children
///   onto it, instead of copying its whole prefix into a fresh buffer — which
///   made a length-*n* list cost O(n²) element moves and one allocation per step.
///
/// Fresh buffers therefore scale with the *node* count (and only nodes whose
/// first kept child isn't a spliced list), no longer the reduction count:
/// `child_vec_allocs / semantic_reduce_calls` drops below 1, exactly the reuse
/// signal the #583 gate doc anticipated. On top of that, the node branch drains
/// `kept` instead of consuming it, so the emptied buffer cycles through a
/// per-parse [`ReduceScratch`] — steady state, the engine's shaping scratch
/// allocates only when a transparent splice carries a buffer away onto the value
/// stack.
pub(crate) fn shape_reduction<'i, B: OutputBuilder<'i>>(
    rule_idx: usize,
    rule: &CompiledRule,
    // The engine's value stack: this reduction consumes its last `len` slots in
    // place (perf spike 2026-07-01 — no per-reduction `drain().collect()`
    // intermediate `Vec`).
    value_stack: &mut Vec<GSlot<B::Value>>,
    len: usize,
    builder: &mut B,
    ctx: &OutputContext,
    propagate: bool,
    scratch: &mut ReduceScratch<B::Value>,
) -> GSlot<B::Value> {
    // Output-shape counter (#230): one reduction shaped, matching `assemble`.
    perf::add_semantic_reduce_call();

    // Fast path: an `expand1` (`?rule`) arity-1 collapse over a single plain kept
    // value returns the slot unchanged — no child buffer exists on this path, so
    // nothing is allocated or copied. Guards reproduce the slow path exactly:
    // no placeholders anywhere, the value not a filtered token, not discarded.
    // (A `GSlot::Value` can never carry `GTag::None` — placeholders only travel
    // inside `Inline` buffers — so the RC9 lone-`None` shape can't arrive here.)
    if rule.options.expand1
        && len == 1
        && rule.alias.is_none()
        && rule.options.placeholder_count == 0
        && rule.options.nones_before.iter().all(|&n| n == 0)
    {
        if let Some(GSlot::Value(e)) = value_stack.last() {
            let kept_here = e.tag != GTag::Token || keep_token_pos(rule, 0);
            if kept_here && !builder.is_discard(&e.value) {
                match value_stack.pop() {
                    Some(slot) => return slot,
                    None => unreachable!("value_stack.last() was Some"),
                }
            }
        }
    }

    // Flatten to the kept child list (drop per-position filtered punctuation, splice
    // transparent inlines, insert `nones_before` placeholders), accumulating the
    // pre-filter container span exactly as `assemble` does.
    //
    // The buffer is materialized lazily: while nothing has been kept yet, a spliced
    // `Inline` buffer is *stolen* as the child list (the left-recursive EBNF-helper
    // case — its whole prefix is never copied); the first plain push allocates with
    // capacity for the rule's remaining arity, removing the 4→8→16… growth
    // reallocs (perf spike 2026-07-01/02).
    let mut kept: Vec<GElem<B::Value>> = Vec::new();
    let mut container = ContainerSpan::new();
    let placeholder = |builder: &mut B| GElem {
        value: builder.placeholder(ctx),
        meta: Meta::default(),
        tag: GTag::None,
    };
    // First materialization of the `kept` buffer: recycle the scratch buffer when
    // one is banked, else charge the child-buffer counter (#583/C8.2 — one *fresh*
    // buffer; recycles, steals, and the fast path charge nothing) and reserve
    // `$cap` slots. Every path that grows `kept` from empty *must* route through
    // here so the recycling and the counter both see the allocation — including the
    // trailing-placeholder pushes below, whose first push (for a reduction with no
    // preceding kept child) would otherwise allocate silently.
    macro_rules! materialize {
        ($cap:expr) => {
            if kept.capacity() == 0 {
                if scratch.kept.capacity() > 0 {
                    kept = std::mem::take(&mut scratch.kept);
                } else {
                    perf::add_child_vec_alloc();
                }
                kept.reserve($cap);
            }
        };
    }
    let drain_start = value_stack.len() - len;
    for (i, slot) in value_stack.drain(drain_start..).enumerate() {
        for _ in 0..nones_at_gap(rule, i) {
            materialize!(len - i + rule.options.placeholder_count);
            kept.push(placeholder(builder));
        }
        match slot {
            GSlot::Value(e) => {
                if propagate {
                    container.observe_gelem(&e);
                }
                let filtered = e.tag == GTag::Token && !keep_token_pos(rule, i);
                if !filtered {
                    materialize!(len - i + rule.options.placeholder_count);
                    kept.push(e);
                }
            }
            GSlot::Inline(mut cs) => {
                if propagate {
                    for c in &cs {
                        container.observe_gelem(c);
                    }
                }
                if kept.capacity() == 0 {
                    // Nothing kept yet: steal the spliced buffer wholesale. Its
                    // elements keep their order and nothing precedes them, so
                    // this is byte-identical to copying — minus the copy.
                    kept = cs;
                } else {
                    kept.append(&mut cs);
                }
            }
        }
    }
    // Trailing placeholders: an empty `[...]`'s widest-alternative count, plus a
    // distributed absent `[...]` at the end of this alternative. Materialize first
    // (with room for both) so a reduction whose *only* children are trailing
    // placeholders still recycles the scratch buffer and charges the counter — the
    // buffer growth must not bypass `materialize!`.
    let trailing = rule.options.placeholder_count + nones_at_gap(rule, rule.expansion.len());
    if trailing > 0 {
        materialize!(trailing);
    }
    for _ in 0..rule.options.placeholder_count {
        kept.push(placeholder(builder));
    }
    for _ in 0..nones_at_gap(rule, rule.expansion.len()) {
        kept.push(placeholder(builder));
    }

    // `Discard` sweep (ADR-0038 §2): after placeholders, before the node is built.
    // Monomorphized away for the default (non-discarding) builder.
    kept.retain(|e| !builder.is_discard(&e.value));

    if rule.transparent {
        // `_rule` / `__anon_*`: splice into the parent, no node built.
        GSlot::Inline(kept)
    } else if rule.options.expand1 && rule.alias.is_none() && kept.len() == 1 {
        // `?rule` with a single child: propagate it unchanged. A lone `None`
        // placeholder stays an inline of exactly one `None` (RC9/#289), so the parent
        // splices one placeholder (reusing this buffer); a real single child
        // collapses to a bare value. (The no-buffer common case took the fast path
        // above and never reaches here.)
        let e = kept.pop().unwrap();
        match e.tag {
            GTag::None => {
                kept.push(e);
                GSlot::Inline(kept)
            }
            _ => GSlot::Value(e),
        }
    } else {
        let mut node_meta = node_meta_from_elems(&kept);
        if propagate {
            container.widen_meta(&mut node_meta);
        }
        // Extract the child values by draining, not consuming: `kept` keeps its
        // capacity and banks in the scratch for the next reduction. `values`
        // likewise cycles — a taking builder (the tree backend `mem::take`s the
        // children as the node's child list) leaves a fresh empty vec behind,
        // while a reading builder (tape/null) leaves the buffer to be cleared
        // and reused, so those paths reach a zero-alloc steady state.
        let mut values = std::mem::take(&mut scratch.values);
        values.reserve(kept.len());
        values.extend(kept.drain(..).map(|e| e.value));
        if kept.capacity() > scratch.kept.capacity() {
            scratch.kept = kept;
        }
        let value = builder.reduce(rule_idx, &mut values, &node_meta, ctx);
        values.clear();
        scratch.values = values;
        GSlot::Value(GElem {
            value,
            meta: node_meta,
            tag: GTag::Tree,
        })
    }
}

/// Per-parse recycling scratch for [`shape_reduction`] (perf spike 2026-07-02):
/// the emptied `kept` buffer and the (reading-builder) `values` buffer cycle
/// here between reductions instead of round-tripping the allocator. Owned by the
/// drive loop (`run_into`), one per parse.
pub(crate) struct ReduceScratch<V> {
    kept: Vec<GElem<V>>,
    values: Vec<V>,
}

impl<V> ReduceScratch<V> {
    pub(crate) fn new() -> Self {
        ReduceScratch {
            kept: Vec::new(),
            values: Vec::new(),
        }
    }
}

/// The final parse value off the generic stack at ACCEPT. Mirrors
/// [`root_slot_to_parse_tree`]: a normal root value, or a `?start` lone-`None`
/// collapse (`Inline([None])`) whose single placeholder value is the result. Any
/// other `Inline` shape is structurally impossible at a start root.
pub(crate) fn accept_value<V>(slot: GSlot<V>) -> Result<V, ()> {
    match slot {
        GSlot::Value(e) => Ok(e.value),
        GSlot::Inline(mut es) if es.len() == 1 => Ok(es.pop().unwrap().value),
        GSlot::Inline(_) => Err(()),
    }
}
