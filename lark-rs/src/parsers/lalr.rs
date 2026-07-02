//! LALR(1) parser: table construction and execution.
//!
//! The pipeline is:
//! 1. `GrammarAnalysis` computes NULLABLE/FIRST.
//! 2. `LR0Builder` constructs LR(0) item sets (states).
//! 3. `LookaheadComputer` propagates true LALR(1) lookaheads.
//! 4. `build_lalr_table` assembles the sparse ACTION/GOTO tables.
//! 5. `LalrParser` drives the state machine against a token stream.
//!
//! The grammar is fully interned ([`CompiledGrammar`]): every symbol is a `Copy`
//! [`SymbolId`], terminals occupy id range `[0, n_terminals)` and non-terminals
//! `[n_terminals, len)`. ACTION/GOTO are **sparse** per-state `(id, …)` rows
//! (Python Lark's dict-of-dicts, not a dense `[state][terminal]` matrix) so the
//! table stores only the `O(filled)` actions, not `O(states × terminals)` cells —
//! a grammar whose state and terminal counts both grow with size is otherwise
//! `O(n²)` (#367, H5-9). Lookup ([`ParseTable::action_at`]) is a linear scan of a
//! state's few entries — the same `&[(u32, Action)]` shape the standalone runtime
//! bakes — and a token still hashes nothing on the hot path. Every tree-shaping
//! decision is a precomputed flag on the rule; the engine never inspects a
//! symbol's name.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};

use crate::error::{GrammarError, ParseError, RecoveryAction};
use crate::grammar::analysis::GrammarAnalysis;
use crate::grammar::intern::{CompiledGrammar, CompiledRule, SymbolId, SymbolTable};
use crate::lexer::{BasicLexer, ContextualLexer};
use crate::tree::{Child, ParseTree, Token};

use super::token_source::{
    postlex_basic_recovering_source, postlex_contextual_recovering_source,
    postlex_contextual_source, Contextual, ContextualRecovering, LexFailure, PreLexed, SourceError,
    TokenSource,
};
use super::tree_builder::{
    accept_value, meta_from_token, shape_reduction, GElem, GSlot, GTag, OutputBuilder,
    OutputContext, Slot, TreeOutputBuilder,
};

// ─── Parse table ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
pub enum Action {
    Shift(usize),  // shift and go to state N
    Reduce(usize), // reduce using rule index N
    Accept,
}

/// Immutable parse tables produced by LALR analysis. **Sparse** and id-keyed.
///
/// Each `action[state]` row is a `(terminal id, action)` list sorted ascending by
/// id, and each `goto[state]` row a `(nonterminal index, next state)` list likewise
/// — Python Lark's sparse dict-of-dicts, not a dense `[state][terminal]` matrix
/// (#367, H5-9). For a grammar whose state *and* terminal counts both grow with
/// size the dense matrix is `O(states × terminals) = O(n²)` cells where only the
/// `O(n)` `Some` actions are semantic; the sparse rows store only those. This is
/// the same `&[(u32, Action)]` shape the standalone emitter already bakes
/// (`standalone::runtime::GrammarData`), so lookup is a linear scan of the row's
/// few entries (`action_at`), and the per-state valid-terminal set the contextual
/// lexer needs (`state_terminals`) reads directly off the row keys.
#[derive(Debug)]
pub struct ParseTable {
    /// `action[state]` → sparse `(terminal id, action)` row, sorted by id. A
    /// missing terminal is an error (the old dense `None`).
    pub action: Vec<Vec<(u32, Action)>>,
    /// `goto[state]` → sparse `(nonterminal index, next state)` row, sorted by
    /// index. A missing index is no transition (the old dense `None`).
    pub goto: Vec<Vec<(u32, u32)>>,
    /// Start state per start symbol.
    pub start_states: HashMap<SymbolId, usize>,
    /// Configured start symbols, in `LarkOptions.start` order. Resolving a
    /// default start (`initial_state(None)`) walks this list — never a
    /// nondeterministic `start_states` key — to mirror Python Lark's
    /// `_verify_start` (issue #251).
    pub starts: Vec<SymbolId>,
    /// Compiled rules (indexed by rule index).
    pub rules: Vec<CompiledRule>,
    /// Symbol metadata (names for diagnostics, kind, …).
    pub symbols: SymbolTable,
    /// Size of the terminal id range; non-terminal GOTO index is `id - this`.
    pub n_terminals: usize,
    /// Python Lark's `propagate_positions` (carried from the [`CompiledGrammar`]):
    /// the reducer threads it to the [`TreeOutputBuilder`](super::tree_builder) so a
    /// node's `meta` spans its pre-filter children (#402).
    pub propagate_positions: bool,
}

impl ParseTable {
    /// ACTION for `(state, terminal)`, or `None` if the state has no action for
    /// the terminal (the old dense `None`). A linear scan of the state's sparse
    /// row — the same lookup the standalone runtime does (`GrammarData::action_at`)
    /// — which is cheap because a state has only its few outgoing actions.
    #[inline]
    fn action_at(&self, state: usize, terminal: SymbolId) -> Option<Action> {
        let key = terminal.index() as u32;
        self.action
            .get(state)?
            .iter()
            .find(|(t, _)| *t == key)
            .map(|(_, a)| *a)
    }

    /// Next state for `(state, nonterminal)` via GOTO, or `None`. `nt_index` is the
    /// non-terminal's GOTO index (`origin.index() - n_terminals`). A linear scan of
    /// the sparse row (`GrammarData::goto_at`'s in-process twin).
    #[inline]
    fn goto_at(&self, state: usize, nt_index: u32) -> Option<u32> {
        self.goto
            .get(state)?
            .iter()
            .find(|(n, _)| *n == nt_index)
            .map(|(_, s)| *s)
    }
}

// ─── LR(0) item ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct LR0Item {
    rule_idx: usize,
    dot: usize, // position of the dot in the expansion
}

impl LR0Item {
    fn new(rule_idx: usize, dot: usize) -> Self {
        LR0Item { rule_idx, dot }
    }

    fn expected(&self, rules: &[CompiledRule]) -> Option<SymbolId> {
        rules[self.rule_idx].expansion.get(self.dot).copied()
    }

    fn advance(&self) -> Self {
        LR0Item {
            rule_idx: self.rule_idx,
            dot: self.dot + 1,
        }
    }

    fn is_complete(&self, rules: &[CompiledRule]) -> bool {
        self.dot >= rules[self.rule_idx].expansion.len()
    }
}

type ItemSet = BTreeSet<LR0Item>;

// ─── LR(0) state machine builder ─────────────────────────────────────────────

struct LR0Builder<'g> {
    rules: &'g [CompiledRule],
    n_terminals: usize,
    /// non-terminal id → rule indices producing it.
    rule_index: HashMap<SymbolId, Vec<usize>>,
    states: Vec<ItemSet>,
    transitions: BTreeMap<(usize, SymbolId), usize>,
}

impl<'g> LR0Builder<'g> {
    fn new(rules: &'g [CompiledRule], n_terminals: usize) -> Self {
        let mut rule_index: HashMap<SymbolId, Vec<usize>> = HashMap::new();
        for (i, rule) in rules.iter().enumerate() {
            rule_index.entry(rule.origin).or_default().push(i);
        }
        LR0Builder {
            rules,
            n_terminals,
            rule_index,
            states: Vec::new(),
            transitions: BTreeMap::new(),
        }
    }

    #[inline]
    fn is_nonterminal(&self, id: SymbolId) -> bool {
        id.index() >= self.n_terminals
    }

    /// Epsilon-closure of a set of LR(0) items.
    fn closure(&self, kernel: &ItemSet) -> ItemSet {
        let mut result = kernel.clone();
        let mut worklist: VecDeque<LR0Item> = kernel.iter().cloned().collect();
        while let Some(item) = worklist.pop_front() {
            if let Some(sym) = item.expected(self.rules) {
                if self.is_nonterminal(sym) {
                    if let Some(prods) = self.rule_index.get(&sym) {
                        for &rule_idx in prods {
                            let new_item = LR0Item::new(rule_idx, 0);
                            if result.insert(new_item.clone()) {
                                worklist.push_back(new_item);
                            }
                        }
                    }
                }
            }
        }
        result
    }

    /// GOTO transition from a state on a symbol.
    fn goto(&self, state: &ItemSet, sym: SymbolId) -> ItemSet {
        let moved: ItemSet = state
            .iter()
            .filter(|item| item.expected(self.rules) == Some(sym))
            .map(|item| item.advance())
            .collect();
        self.closure(&moved)
    }

    /// Build all LR(0) states from the augmented start rules.
    ///
    /// `starts`: (start symbol id, its augmented `$root` rule index).
    fn build(&mut self, starts: &[(SymbolId, usize)]) -> HashMap<SymbolId, usize> {
        let mut start_states = HashMap::new();
        let mut index: HashMap<ItemSet, usize> = HashMap::new();
        let mut worklist: VecDeque<usize> = VecDeque::new();

        for &(start_id, aug_idx) in starts {
            let kernel: ItemSet = std::iter::once(LR0Item::new(aug_idx, 0)).collect();
            let s0 = self.intern_state(kernel, &mut index, &mut worklist);
            start_states.insert(start_id, s0);
        }

        // Iterative worklist (not recursion) so deep GOTO chains — e.g. `"A"~8191`
        // expands to thousands of chained states — do not overflow the stack.
        while let Some(id) = worklist.pop_front() {
            let closed = self.states[id].clone();
            let symbols: BTreeSet<SymbolId> = closed
                .iter()
                .filter_map(|item| item.expected(self.rules))
                .collect();
            for sym in symbols {
                let next_state_items = self.goto(&closed, sym);
                if !next_state_items.is_empty() {
                    let next_id = self.intern_state(next_state_items, &mut index, &mut worklist);
                    self.transitions.insert((id, sym), next_id);
                }
            }
        }
        start_states
    }

    /// State id for the closure of `kernel`, creating + queuing it if new.
    fn intern_state(
        &mut self,
        kernel: ItemSet,
        index: &mut HashMap<ItemSet, usize>,
        worklist: &mut VecDeque<usize>,
    ) -> usize {
        let closed = self.closure(&kernel);
        if let Some(&id) = index.get(&closed) {
            return id;
        }
        let id = self.states.len();
        self.states.push(closed.clone());
        index.insert(closed, id);
        worklist.push_back(id);
        id
    }
}

// ─── LALR(1) lookahead computation ───────────────────────────────────────────

/// Sentinel lookahead marking lookaheads that *propagate* from a kernel item
/// rather than being generated spontaneously. [`SymbolId::UNSET`] can never
/// collide with a real terminal (terminals live in `[0, n_terminals)`).
const PROPAGATE_MARK: SymbolId = SymbolId::UNSET;

/// Computes true LALR(1) lookaheads for every reduce item via spontaneous
/// generation + propagation (Aho/Sethi/Ullman 4.62–4.63). Strictly more precise
/// than SLR FOLLOW sets, which is what avoids spurious conflicts and what the
/// contextual lexer relies on.
struct LookaheadComputer<'g> {
    rules: &'g [CompiledRule],
    states: &'g [ItemSet],
    transitions: &'g BTreeMap<(usize, SymbolId), usize>,
    analysis: &'g GrammarAnalysis,
}

impl<'g> LookaheadComputer<'g> {
    fn new(
        rules: &'g [CompiledRule],
        states: &'g [ItemSet],
        transitions: &'g BTreeMap<(usize, SymbolId), usize>,
        analysis: &'g GrammarAnalysis,
    ) -> Self {
        LookaheadComputer {
            rules,
            states,
            transitions,
            analysis,
        }
    }

    /// Kernel items: dot past the start, plus the augmented start items (dot 0).
    fn is_kernel(&self, item: &LR0Item) -> bool {
        item.dot > 0 || self.rules[item.rule_idx].is_start
    }

    fn rule_index(&self) -> HashMap<SymbolId, Vec<usize>> {
        let mut idx: HashMap<SymbolId, Vec<usize>> = HashMap::new();
        for (i, rule) in self.rules.iter().enumerate() {
            idx.entry(rule.origin).or_default().push(i);
        }
        idx
    }

    /// LR(1) closure: propagate lookahead sets from kernel items to every
    /// reachable closure item, to a fixpoint.
    fn lr1_closure(
        &self,
        kernel: &HashMap<LR0Item, HashSet<SymbolId>>,
        rule_index: &HashMap<SymbolId, Vec<usize>>,
    ) -> HashMap<LR0Item, HashSet<SymbolId>> {
        let mut result: HashMap<LR0Item, HashSet<SymbolId>> = kernel.clone();
        let mut changed = true;
        while changed {
            changed = false;
            let snapshot: Vec<(LR0Item, HashSet<SymbolId>)> =
                result.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            for (item, la) in snapshot {
                let Some(sym) = item.expected(self.rules) else {
                    continue;
                };
                let Some(prods) = rule_index.get(&sym) else {
                    continue;
                };
                let rule = &self.rules[item.rule_idx];
                let beta = &rule.expansion[item.dot + 1..];
                let (first_beta, beta_nullable) = self.analysis.first_of_seq(beta);
                let mut seed = first_beta;
                if beta_nullable {
                    seed.extend(la.iter().copied());
                }
                for &ri in prods {
                    let entry = result.entry(LR0Item::new(ri, 0)).or_default();
                    for &s in &seed {
                        if entry.insert(s) {
                            changed = true;
                        }
                    }
                }
            }
        }
        result
    }

    /// Lookaheads for every reduce item in every state.
    /// `reduce_la[state][rule_idx]` → lookahead terminals.
    fn compute(&self) -> HashMap<usize, HashMap<usize, HashSet<SymbolId>>> {
        let rule_index = self.rule_index();
        // Kernel-item lookahead sets, keyed (state, item).
        let mut kla: HashMap<(usize, LR0Item), HashSet<SymbolId>> = HashMap::new();
        // Propagation links: (from_state, from_item) → (to_state, to_item).
        let mut links: Vec<((usize, LR0Item), (usize, LR0Item))> = Vec::new();

        // Seed augmented start kernels with $END.
        for (state_id, state) in self.states.iter().enumerate() {
            for item in state {
                if self.is_kernel(item) {
                    let set = kla.entry((state_id, item.clone())).or_default();
                    if self.rules[item.rule_idx].is_start {
                        set.insert(SymbolId::END);
                    }
                }
            }
        }

        // Discover spontaneous lookaheads and propagation links by closing each
        // kernel item against the dummy lookahead `#`.
        for (state_id, state) in self.states.iter().enumerate() {
            for k in state {
                if !self.is_kernel(k) {
                    continue;
                }
                let mut seed = HashMap::new();
                seed.insert(k.clone(), std::iter::once(PROPAGATE_MARK).collect());
                let closed = self.lr1_closure(&seed, &rule_index);
                for (item, la_set) in &closed {
                    let Some(sym) = item.expected(self.rules) else {
                        continue;
                    };
                    let Some(&goto_state) = self.transitions.get(&(state_id, sym)) else {
                        continue;
                    };
                    let advanced = item.advance();
                    for &a in la_set {
                        if a == PROPAGATE_MARK {
                            links.push(((state_id, k.clone()), (goto_state, advanced.clone())));
                        } else {
                            kla.entry((goto_state, advanced.clone()))
                                .or_default()
                                .insert(a);
                        }
                    }
                }
            }
        }

        // Propagate lookaheads along links to a fixpoint.
        let mut changed = true;
        while changed {
            changed = false;
            for (from, to) in &links {
                let src: Vec<SymbolId> = kla
                    .get(from)
                    .map(|s| s.iter().copied().collect())
                    .unwrap_or_default();
                if src.is_empty() {
                    continue;
                }
                let dst = kla.entry(to.clone()).or_default();
                for t in src {
                    if dst.insert(t) {
                        changed = true;
                    }
                }
            }
        }

        // Final per-state LR(1) closure so ε-rule reduce items (closure items,
        // not kernels) also receive lookaheads; collect per reduce rule.
        let mut reduce_la: HashMap<usize, HashMap<usize, HashSet<SymbolId>>> = HashMap::new();
        for (state_id, state) in self.states.iter().enumerate() {
            let mut kernel: HashMap<LR0Item, HashSet<SymbolId>> = HashMap::new();
            for item in state {
                if self.is_kernel(item) {
                    let set = kla
                        .get(&(state_id, item.clone()))
                        .cloned()
                        .unwrap_or_default();
                    kernel.insert(item.clone(), set);
                }
            }
            let closed = self.lr1_closure(&kernel, &rule_index);
            for (item, la_set) in closed {
                if item.is_complete(self.rules) {
                    reduce_la
                        .entry(state_id)
                        .or_default()
                        .entry(item.rule_idx)
                        .or_default()
                        .extend(la_set);
                }
            }
        }
        reduce_la
    }
}

// ─── Table construction ───────────────────────────────────────────────────────

pub fn build_lalr_table(
    grammar: &CompiledGrammar,
    strict: bool,
) -> Result<ParseTable, GrammarError> {
    let rules = &grammar.rules;
    let n_terminals = grammar.n_terminals();
    let analysis = GrammarAnalysis::compute(grammar);

    // Pair each start symbol with its augmented `$root` rule index.
    let starts: Vec<(SymbolId, usize)> = grammar
        .start
        .iter()
        .filter_map(|&s| {
            rules
                .iter()
                .position(|r| r.is_start && r.expansion == [s])
                .map(|idx| (s, idx))
        })
        .collect();

    // LR(0) state construction.
    let mut builder = LR0Builder::new(rules, n_terminals);
    let start_states = builder.build(&starts);
    let (states, transitions) = (builder.states, builder.transitions);

    let n_states = states.len();
    // Sparse per-state rows (#367, H5-9): a `BTreeMap` per state holds only the
    // filled cells, keyed by terminal id / non-terminal GOTO index. Build keeps the
    // dense matrix's read-modify-write semantics (the shift/reduce conflict check
    // below reads back what SHIFT/ACCEPT wrote) but stores `Θ(filled)` entries, not
    // the dense `n_states × n_terminals`. The `BTreeMap` key order also makes the
    // flattened rows sorted ascending — the deterministic `&[(u32, Action)]` shape
    // the standalone emitter bakes. (The old dense GOTO matrix sized itself on
    // `n_nonterminals`; the sparse rows don't need it, so it is no longer computed.)
    let mut action: Vec<BTreeMap<u32, Action>> = vec![BTreeMap::new(); n_states];
    let mut goto: Vec<BTreeMap<u32, u32>> = vec![BTreeMap::new(); n_states];

    // SHIFT and GOTO from transitions — terminal vs non-terminal is the id range.
    for (&(state_id, sym), &next_state) in &transitions {
        if sym.index() < n_terminals {
            action[state_id].insert(sym.index() as u32, Action::Shift(next_state));
        } else {
            goto[state_id].insert((sym.index() - n_terminals) as u32, next_state as u32);
        }
    }

    // REDUCE / ACCEPT, resolving conflicts exactly as Python Lark does:
    //   * shift/reduce  → shift wins (no error in default mode)
    //   * reduce/reduce → highest rule priority wins; a tie is a hard error
    let reduce_la = LookaheadComputer::new(rules, &states, &transitions, &analysis).compute();
    let mut conflicts: Vec<String> = Vec::new();

    for (state_id, state) in states.iter().enumerate() {
        // Augmented start items reduce to ACCEPT on $END.
        for item in state {
            if item.is_complete(rules) && rules[item.rule_idx].is_start {
                action[state_id].insert(SymbolId::END.index() as u32, Action::Accept);
            }
        }

        let Some(rule_la) = reduce_la.get(&state_id) else {
            continue;
        };
        let mut reduces_by_la: BTreeMap<SymbolId, Vec<usize>> = BTreeMap::new();
        for (&rule_idx, la_set) in rule_la {
            if rules[rule_idx].is_start {
                continue; // handled as ACCEPT
            }
            for &la in la_set {
                reduces_by_la.entry(la).or_default().push(rule_idx);
            }
        }

        for (la, mut candidates) in reduces_by_la {
            candidates.sort_unstable();
            candidates.dedup();

            // Collapse candidates that are the *same reduction* modulo tree-naming:
            // two rules sharing `origin` + `expansion` differ only by an alias /
            // `tree_name` (`p: "a"? -> al1 | "b"? -> al2`, both nullable → two
            // `p -> ε` reductions; or a helper's byte-identical twin ε arms). Python
            // Lark keeps the alias as tree-naming metadata *outside* the LALR
            // reduce/reduce comparison and resolves a same-origin/same-expansion tie
            // by rule order (first arm wins), so this is not a real collision. Keep
            // the lowest-`order` representative per (origin, expansion) group — the
            // first arm — matching Python (#401, H6-3; also folds H6-4's identical
            // ε arms). Genuinely distinct reductions (different `expansion`) are
            // untouched and still error below. The winning rule's `tree_name` /
            // alias / placeholder options drive the built node, exactly as the
            // first-arm reduction would.
            if candidates.len() > 1 {
                let mut by_group: BTreeMap<(SymbolId, &[SymbolId]), usize> = BTreeMap::new();
                for &ri in &candidates {
                    let key = (rules[ri].origin, rules[ri].expansion.as_slice());
                    by_group
                        .entry(key)
                        .and_modify(|best| {
                            if rules[ri].order < rules[*best].order {
                                *best = ri;
                            }
                        })
                        .or_insert(ri);
                }
                if by_group.len() < candidates.len() {
                    candidates = by_group.into_values().collect();
                    candidates.sort_unstable();
                }
            }

            let winner = if candidates.len() > 1 {
                let mut by_prio = candidates.clone();
                by_prio.sort_by_key(|&ri| std::cmp::Reverse(rules[ri].options.priority));
                let best = rules[by_prio[0]].options.priority;
                let second = rules[by_prio[1]].options.priority;
                if best > second {
                    by_prio[0]
                } else {
                    let rule_list: String = candidates
                        .iter()
                        .map(|&ri| format!("\n\t- {}", rules[ri]))
                        .collect();
                    conflicts.push(format!(
                        "Reduce/Reduce collision in state {} for terminal {}:{}",
                        state_id,
                        grammar.symbols.name(la),
                        rule_list
                    ));
                    continue;
                }
            } else {
                candidates[0]
            };

            // Shift/accept wins over reduce (Lark default). In strict mode a
            // shift/reduce conflict is fatal instead of silently resolved —
            // exactly Python Lark's `strict=True` (lalr_analysis.py).
            match action[state_id].get(&(la.index() as u32)) {
                Some(Action::Shift(_)) | Some(Action::Accept) => {
                    if strict {
                        conflicts.push(format!(
                            "Shift/Reduce conflict for terminal {}. [strict-mode]\n * {}",
                            grammar.symbols.name(la),
                            rules[winner]
                        ));
                    }
                }
                _ => {
                    action[state_id].insert(la.index() as u32, Action::Reduce(winner));
                }
            }
        }
    }

    if !conflicts.is_empty() {
        return Err(GrammarError::Conflict {
            report: conflicts.join("\n\n"),
        });
    }

    // Flatten the per-state maps into sorted sparse rows — the `&[(u32, Action)]`
    // shape (#367). `BTreeMap` iteration is already key-ascending, so the rows are
    // deterministic without an explicit sort. The work counter records the *stored*
    // cell count — now `Θ(filled)` (the `Some` actions), not the dense `Θ(states ×
    // terminals)`: this is exactly the density win the #367 gate asserts.
    let action: Vec<Vec<(u32, Action)>> = action
        .into_iter()
        .map(|row| row.into_iter().collect())
        .collect();
    let goto: Vec<Vec<(u32, u32)>> = goto
        .into_iter()
        .map(|row| row.into_iter().collect())
        .collect();
    crate::perf::add_parse_table_action_cells(
        action.iter().map(|row| row.len() as u64).sum::<u64>(),
    );

    Ok(ParseTable {
        action,
        goto,
        start_states,
        starts: grammar.start.clone(),
        rules: rules.clone(),
        symbols: grammar.symbols.clone(),
        n_terminals,
        propagate_positions: grammar.propagate_positions,
    })
}

/// Run the post-lowering reduce/reduce collision audit (RC7/#272, ADR-0013) for a
/// surface [`Grammar`](crate::grammar::Grammar) about to be built as LALR.
///
/// The load-bearing EBNF helper *sharing* (`recurse_cache`) can fuse two recurse
/// helpers Python Lark mints distinctly (`start: r0* | (r0)*`), masking a
/// reduce/reduce collision Python rejects at build. When the loader detected such an
/// over-share it attached a Python-faithful audit shadow (`Grammar::lalr_audit` —
/// the same grammar re-lowered with recurse helpers keyed on the inner source-AST).
/// This lowers that shadow and runs the *same* conflict detector over it, surfacing
/// any `Conflict` it reports. The sharing stays load-bearing for the real parse
/// table — the shadow only gates the build, never parses. The shadow is structurally
/// a superset of the real grammar's recurse rules (split, never merged), so it can
/// only ever expose the masked collision, never invent a spurious one.
///
/// A no-op when no shadow is attached (no over-share was detected). Shared by both
/// LALR build paths — the live frontend (`build_lalr`) and standalone generation —
/// so the rejection contract can never drift between them.
///
/// Naming note: the over-share the audit *targets* is a reduce/reduce collision, but
/// the function runs the full [`build_lalr_table`], so in `strict` mode it also
/// surfaces any **shift/reduce** conflict the shadow's split helpers expose — i.e. it
/// reports whatever `Conflict` Python's un-shared model would, not reduce/reduce
/// exclusively. The shadow is a structural superset of the real recurse rules (split,
/// never merged), so any conflict it surfaces is one the sharing masked, never a
/// spurious one.
pub fn audit_lalr_reduce_reduce(
    grammar: &crate::grammar::Grammar,
    strict: bool,
) -> Result<(), GrammarError> {
    if let Some(shadow) = &grammar.lalr_audit {
        let shadow_cg = crate::grammar::lower(shadow);
        build_lalr_table(&shadow_cg, strict)?;
    }
    Ok(())
}

// ─── ParserStack: the shared state machine (#168) ───────────────────────────

/// The two stacks plus the "feed one token" reduce-loop that drive every LALR
/// parse. Lifting them out of [`LalrParser::run`]/`run_recovering` into a single
/// [`feed_token`](ParserStack::feed_token) mirrors Python Lark's
/// `ParserState.feed_token` and gives exactly one definition of "advance the
/// machine by one token" — shared by the batch drivers and the interactive parser
/// (issue #168, ADR-0015). The fed token is fixed for the whole reduce loop,
/// exactly as the contextual lexer caches its token across REDUCEs
/// ([`token_source`](super::token_source)), so this is behaviour-preserving for
/// every existing driver.
#[derive(Clone)]
pub(crate) struct ParserStack {
    state_stack: Vec<usize>,
    value_stack: Vec<Slot>,
}

/// What feeding one token did to a [`ParserStack`].
pub(crate) enum Feed {
    /// The token was shifted (consumed) — pull the next one.
    Shifted,
    /// Reached ACCEPT; here is the finished tree.
    Accepted(ParseTree),
    /// No action for this token in the current (post-reduce) state. The stack is
    /// left where the parser would raise `UnexpectedToken`; the caller decides to
    /// error (batch parse) or delete-and-resume (recovery).
    NoAction,
    /// A missing GOTO after a reduce — effectively unreachable for a valid table,
    /// surfaced rather than panicked.
    Error(ParseError),
}

impl ParserStack {
    fn new(initial_state: usize) -> Self {
        ParserStack {
            state_stack: vec![initial_state],
            value_stack: Vec::new(),
        }
    }

    /// The current (top) parser state.
    #[inline]
    pub(crate) fn position(&self) -> usize {
        *self.state_stack.last().unwrap()
    }

    /// Feed one token: REDUCE as many times as it dictates, then SHIFT it
    /// ([`Feed::Shifted`]), ACCEPT ([`Feed::Accepted`]), or report no action
    /// ([`Feed::NoAction`]). Mirrors Python Lark's `ParserState.feed_token`.
    pub(crate) fn feed_token(&mut self, table: &ParseTable, token: &Token) -> Feed {
        loop {
            let state = self.position();
            match table.action_at(state, token.type_id) {
                Some(Action::Shift(next_state)) => {
                    self.state_stack.push(next_state);
                    self.value_stack.push(Slot::Token(token.clone()));
                    return Feed::Shifted;
                }
                Some(Action::Reduce(rule_idx)) => {
                    if let Err(e) = self.reduce(table, rule_idx, token) {
                        return Feed::Error(e);
                    }
                }
                Some(Action::Accept) => {
                    return match self.accept() {
                        Ok(tree) => Feed::Accepted(tree),
                        Err(e) => Feed::Error(e),
                    };
                }
                None => return Feed::NoAction,
            }
        }
    }

    /// Apply a REDUCE: pop the rule's child values, shape the parent via the shared
    /// [`TreeOutputBuilder`], and follow GOTO. `at` supplies the position for the
    /// (effectively unreachable) missing-GOTO error.
    fn reduce(
        &mut self,
        table: &ParseTable,
        rule_idx: usize,
        at: &Token,
    ) -> Result<(), ParseError> {
        let rule = &table.rules[rule_idx];
        let len = rule.expansion.len();

        let child_values: Vec<Slot> = self
            .value_stack
            .drain(self.value_stack.len() - len..)
            .collect();
        for _ in 0..len {
            self.state_stack.pop();
        }
        let value =
            TreeOutputBuilder::with_propagate_positions(&table.rules, table.propagate_positions)
                .assemble(rule_idx, child_values);

        let top_state = self.position();
        let nt_index = (rule.origin.index() - table.n_terminals) as u32;
        let next_state =
            table
                .goto_at(top_state, nt_index)
                .ok_or_else(|| ParseError::UnexpectedToken {
                    token: at.value.clone(),
                    token_type: table.symbols.name(rule.origin).to_string(),
                    line: at.line as usize,
                    col: at.column as usize,
                    expected: vec![table.symbols.name(rule.origin).to_string()],
                })?;
        self.state_stack.push(next_state as usize);
        self.value_stack.push(value);
        Ok(())
    }

    /// ACCEPT: the final value on the stack is the parse result (a `?start` rule can
    /// collapse to a bare token or a bare `None`, hence [`ParseTree`]).
    fn accept(&mut self) -> Result<ParseTree, ParseError> {
        match self.value_stack.pop() {
            // `Slot::Tree`/`Token`, plus the top-level `?start` lone-`None` collapse
            // (`Inline([None])` → `ParseTree::None`, RC9/#289/ADR-0033), go through
            // the shared root converter so the carve-out has one definition across
            // backends. Any *other* `Inline` shape on a start rule is structurally
            // impossible (a start rule never inlines); treat it, like an empty stack,
            // as no parse result rather than panicking.
            Some(value) => super::tree_builder::root_slot_to_parse_tree(value)
                .map_err(|_| ParseError::unexpected_eof(0, 0, vec![])),
            None => Err(ParseError::unexpected_eof(0, 0, vec![])),
        }
    }

    /// State-only simulation: would feeding `terminal` advance the parser (SHIFT or
    /// ACCEPT, possibly after REDUCEs) without a no-action error? Clones only the
    /// cheap state stack — no tree values are built — so [`accepts`](Self::accepts)
    /// is far cheaper than Python's copy-and-trial-feed.
    fn would_accept(&self, table: &ParseTable, terminal: SymbolId) -> bool {
        let mut states = self.state_stack.clone();
        loop {
            let state = *states.last().unwrap();
            match table.action_at(state, terminal) {
                Some(Action::Shift(_)) | Some(Action::Accept) => return true,
                Some(Action::Reduce(rule_idx)) => {
                    let rule = &table.rules[rule_idx];
                    for _ in 0..rule.expansion.len() {
                        states.pop();
                    }
                    let top = *states.last().unwrap();
                    let nt_index = (rule.origin.index() - table.n_terminals) as u32;
                    match table.goto_at(top, nt_index) {
                        Some(next) => states.push(next as usize),
                        None => return false,
                    }
                }
                None => return false,
            }
        }
    }

    /// The terminal names that would advance the parser from here, sorted — the
    /// oracle-comparable form of Python's `InteractiveParser.accepts()`.
    pub(crate) fn accepts(&self, table: &ParseTable) -> Vec<String> {
        let mut names: Vec<String> = (0..table.n_terminals)
            .map(|t| SymbolId(t as u32))
            .filter(|&t| self.would_accept(table, t))
            .map(|t| table.symbols.name(t).to_string())
            .collect();
        names.sort();
        names
    }
}

/// A short-lived recovery view onto the parser's state machine (issue #223).
///
/// Passed to the `on_error` handler so it can inspect `accepts()` and feed
/// corrective tokens (`feed`/`feed_token`) before returning a [`RecoveryAction`].
/// It is backed by the same [`ParserStack`] the batch/recovery drivers use, but
/// it is *not* the public [`InteractiveParser`]: its lifetime is scoped to the
/// handler call and it does not own a lexer or input text.
///
/// Failed feeds are transactional: if `feed_token` returns `Err`, the stack is
/// rolled back to its state before the call, so candidate-insertion patterns
/// (try feed, fall back to Delete on failure) are safe.
///
/// [`RecoveryAction`]: crate::error::RecoveryAction
/// [`InteractiveParser`]: super::interactive::InteractiveParser
pub struct RecoveryContext<'a> {
    stack: &'a mut ParserStack,
    table: &'a ParseTable,
    fed_count: usize,
    accepted_tree: Option<ParseTree>,
}

impl<'a> RecoveryContext<'a> {
    pub(crate) fn new(stack: &'a mut ParserStack, table: &'a ParseTable) -> Self {
        RecoveryContext {
            stack,
            table,
            fed_count: 0,
            accepted_tree: None,
        }
    }

    pub(crate) fn fed_count(&self) -> usize {
        self.fed_count
    }

    /// The terminal names that would advance the parser from its current state,
    /// sorted — identical to [`InteractiveParser::accepts`].
    ///
    /// [`InteractiveParser::accepts`]: super::interactive::InteractiveParser::accepts
    pub fn accepts(&self) -> Vec<String> {
        self.stack.accepts(self.table)
    }

    /// Feed one token, advancing through any REDUCEs to the next SHIFT or ACCEPT.
    /// Returns `Ok(None)` on SHIFT and `Ok(Some(tree))` on ACCEPT, matching
    /// [`InteractiveParser::feed_token`]'s return shape. Returns `Err` when the
    /// parser has no action for the token.
    ///
    /// **On ACCEPT:** the tree is saved internally and the recovery loop will
    /// short-circuit — the parse completed inside the handler. Further feeds
    /// after ACCEPT are rejected.
    ///
    /// **Transactional on failure:** if the feed errors (including after partial
    /// reductions), the stack is rolled back to its pre-call state so the handler
    /// can safely try candidate insertions and fall back. The common case (no
    /// action for the token) is checked before cloning the stack, so candidate-
    /// insertion patterns pay O(1) per rejected candidate.
    ///
    /// Feeding `$END` is rejected — use [`RecoveryAction::Resume`] to retry the
    /// current lookahead after feeding corrective tokens; completion is the
    /// recovery loop's responsibility, not the handler's.
    ///
    /// [`InteractiveParser::feed_token`]: super::interactive::InteractiveParser::feed_token
    /// [`RecoveryAction::Resume`]: crate::error::RecoveryAction::Resume
    pub fn feed_token(&mut self, mut token: Token) -> Result<Option<ParseTree>, ParseError> {
        if self.accepted_tree.is_some() {
            return Err(ParseError::unexpected_token_keep_end(&token, vec![]));
        }
        match self.table.symbols.id(&token.type_) {
            Some(id) => {
                if id == SymbolId::END {
                    let expected = self.accepts();
                    return Err(ParseError::unexpected_token_keep_end(&token, expected));
                }
                token.type_id = id;
            }
            None => {
                let expected = self.accepts();
                return Err(ParseError::unexpected_token_keep_end(&token, expected));
            }
        }
        let state = self.stack.position();
        if self.table.action_at(state, token.type_id).is_none() {
            let expected = self.accepts();
            return Err(ParseError::unexpected_token_keep_end(&token, expected));
        }
        let snapshot = self.stack.clone();
        match self.stack.feed_token(self.table, &token) {
            Feed::Shifted => {
                self.fed_count += 1;
                Ok(None)
            }
            Feed::Accepted(tree) => {
                self.fed_count += 1;
                let ret = tree.clone();
                self.accepted_tree = Some(tree);
                Ok(Some(ret))
            }
            Feed::Error(e) => {
                *self.stack = snapshot;
                Err(e)
            }
            Feed::NoAction => {
                *self.stack = snapshot;
                let expected = self.accepts();
                Err(ParseError::unexpected_token_keep_end(&token, expected))
            }
        }
    }

    /// Feed a token by terminal name and value — convenience wrapper over
    /// [`feed_token`](Self::feed_token).
    pub fn feed(&mut self, terminal: &str, value: &str) -> Result<Option<ParseTree>, ParseError> {
        self.feed_token(Token::new(terminal, value))
    }
}

// ─── LALR parser execution ────────────────────────────────────────────────────

pub struct LalrParser {
    pub table: ParseTable,
}

impl LalrParser {
    pub fn new(table: ParseTable) -> Self {
        LalrParser { table }
    }

    /// Valid terminal ids per state — for the contextual lexer. With the sparse
    /// rows (#367) this is just the row's keys (each is a terminal the state has
    /// an action for), no longer a scan over the full dense terminal range.
    pub fn state_terminals(&self) -> HashMap<usize, Vec<SymbolId>> {
        self.table
            .action
            .iter()
            .enumerate()
            .map(|(state, row)| {
                let ids = row.iter().map(|(t, _)| SymbolId(*t)).collect();
                (state, ids)
            })
            .collect()
    }

    // ─── Shared LALR driver helpers ──────────────────────────────────────────

    /// Resolve the start symbol name to its initial state.
    ///
    /// Name resolution (default vs. explicit start, the >1-start and unknown-start
    /// diagnostics) is delegated to the shared
    /// [`resolve_start`](super::resolve_start) so LALR, Earley, and CYK reject
    /// identically — mirroring Python Lark's `_verify_start` (issues #251, #256).
    /// The resolved start id is then mapped to its LR(0) start state.
    pub(crate) fn initial_state(&self, start: Option<&str>) -> Result<usize, ParseError> {
        let start_id = super::resolve_start(&self.table.starts, &self.table.symbols, start)?;
        self.table
            .start_states
            .get(&start_id)
            .copied()
            .ok_or_else(|| {
                ParseError::unexpected_eof(0, 0, vec!["no start rule configured".to_string()])
            })
    }

    /// Valid terminal names for a state — used to build error reports. The sparse
    /// row's keys are exactly the terminals the state has an action for (#367).
    fn expected_at(&self, state: usize) -> Vec<String> {
        self.table
            .action
            .get(state)
            .map(|row| {
                row.iter()
                    .map(|(t, _)| self.table.symbols.name(SymbolId(*t)).to_string())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Build the error for a token with no action in the current state, filling
    /// `expected` from the state's action row (only the parser knows it). Shared by
    /// the batch driver and the interactive parser (issue #168).
    pub(crate) fn unexpected(&self, state: usize, token: &Token) -> ParseError {
        ParseError::unexpected_token(token, self.expected_at(state))
    }

    /// A fresh [`ParserStack`] at the start state for `start` — the seed of an
    /// interactive parse (issue #168). The driver pairs it with the lexer + input.
    pub(crate) fn initial_stack(&self, start: Option<&str>) -> Result<ParserStack, ParseError> {
        Ok(ParserStack::new(self.initial_state(start)?))
    }

    /// Drive the LALR state machine against any [`TokenSource`]. The per-token
    /// reduce/shift/accept work lives in [`ParserStack::feed_token`]; this loop only
    /// sources the next token and reacts to what the feed did, so the batch driver,
    /// the recovering driver, and the interactive parser (#168) share one definition
    /// of the state machine.
    /// The batch (non-recovering) LALR parse to the default tree. Delegates to the
    /// value-parametric [`run_into`](Self::run_into) with the [`TreeOutputBuilder`],
    /// so `parse()` and `parse_into` share one drive loop (ADR-0029 fork 2) and the
    /// full compliance/oracle/wild banks gate the generic path byte-identically.
    /// `input` is unused by the tree backend (it materializes owned strings), so `""`
    /// is passed; a span backend receives the real source via `parse_into`.
    fn run<S: TokenSource>(
        &self,
        source: &mut S,
        start: Option<&str>,
    ) -> Result<ParseTree, ParseError> {
        let mut builder = TreeOutputBuilder::with_propagate_positions(
            &self.table.rules,
            self.table.propagate_positions,
        );
        let child = self.run_into(source, start, &mut builder, "")?;
        Ok(match child {
            Child::Tree(t) => ParseTree::Tree(t),
            Child::Token(t) => ParseTree::Token(t),
            Child::None => ParseTree::None,
        })
    }

    /// Value-parametric parse (the `Lark::parse_into` seam, #232 C7). Drives an
    /// arbitrary [`OutputBuilder`] through the same ACTION/GOTO tables and the same
    /// tree-shaping rules as [`run`](Self::run), but over a stack of `B::Value`, so a
    /// semantic/span backend never builds a generic tree. LALR + basic/contextual
    /// only (ADR-0029 fork 4); recovery/interactive stay on the concrete
    /// [`ParserStack`]. With `B = TreeOutputBuilder` this is byte-identical to `run`
    /// (the `parse_into(tree) == parse()` relative oracle over the compliance
    /// corpus). `input` is the whole source so a borrowing builder can slice spans;
    /// the tree backend ignores it.
    pub(crate) fn run_into<'i, B>(
        &self,
        source: &mut dyn TokenSource,
        start: Option<&str>,
        builder: &mut B,
        input: &'i str,
    ) -> Result<B::Value, ParseError>
    where
        B: OutputBuilder<'i>,
    {
        let ctx = OutputContext::new(&self.table.rules, &self.table.symbols);
        let mut state_stack: Vec<usize> = vec![self.initial_state(start)?];
        let mut value_stack: Vec<GSlot<B::Value>> = Vec::new();

        loop {
            let state = *state_stack.last().unwrap();
            // Dispatch on the terminal id alone — the token itself is only
            // materialized (moved, not cloned) at the SHIFT, and only cloned on
            // the cold error paths (perf spike 2026-07-01).
            let ttype = match source.peek_type(state) {
                Ok(t) => t,
                Err(SourceError::Lex(failure)) => return Err(self.lex_failure(state, failure)),
                Err(SourceError::Postlex(e)) => return Err(e),
            };

            // REDUCE as many times as the token dictates, then SHIFT / ACCEPT / error
            // — the inlined analog of `ParserStack::feed_token` over the generic stack.
            loop {
                let state = *state_stack.last().unwrap();
                match self.table.action_at(state, ttype) {
                    Some(Action::Shift(next_state)) => {
                        // The token is cached by the peek_type above; take_current
                        // moves it out and advances the source — no clone.
                        let token = match source.take_current(state) {
                            Ok(t) => t,
                            Err(SourceError::Lex(failure)) => {
                                return Err(self.lex_failure(state, failure))
                            }
                            Err(SourceError::Postlex(e)) => return Err(e),
                        };
                        let meta = meta_from_token(&token);
                        let value = builder.token(token, input, &ctx);
                        state_stack.push(next_state);
                        value_stack.push(GSlot::Value(GElem {
                            value,
                            meta,
                            tag: GTag::Token,
                        }));
                        break;
                    }
                    Some(Action::Reduce(rule_idx)) => {
                        let rule = &self.table.rules[rule_idx];
                        let len = rule.expansion.len();
                        state_stack.truncate(state_stack.len() - len);
                        // shape_reduction drains the last `len` slots off the
                        // value stack in place — no intermediate `Vec` per
                        // reduction (perf spike 2026-07-01).
                        let shaped = shape_reduction(
                            rule_idx,
                            rule,
                            &mut value_stack,
                            len,
                            builder,
                            &ctx,
                            self.table.propagate_positions,
                        );
                        let top = *state_stack.last().unwrap();
                        let nt_index = (rule.origin.index() - self.table.n_terminals) as u32;
                        let next = match self.table.goto_at(top, nt_index) {
                            Some(n) => n,
                            None => {
                                // Cold path: the token is still cached, so this
                                // peek is a clone of the cached value, never a
                                // re-lex.
                                let token = match source.peek(state) {
                                    Ok(t) => t,
                                    Err(SourceError::Lex(failure)) => {
                                        return Err(self.lex_failure(state, failure))
                                    }
                                    Err(SourceError::Postlex(e)) => return Err(e),
                                };
                                return Err(ParseError::UnexpectedToken {
                                    token: token.value.clone(),
                                    token_type: self.table.symbols.name(rule.origin).to_string(),
                                    line: token.line as usize,
                                    col: token.column as usize,
                                    expected: vec![self
                                        .table
                                        .symbols
                                        .name(rule.origin)
                                        .to_string()],
                                });
                            }
                        };
                        state_stack.push(next as usize);
                        value_stack.push(shaped);
                    }
                    Some(Action::Accept) => {
                        return value_stack
                            .pop()
                            .ok_or(())
                            .and_then(accept_value)
                            .map_err(|_| ParseError::unexpected_eof(0, 0, vec![]));
                    }
                    None => {
                        let token = match source.peek(state) {
                            Ok(t) => t,
                            Err(SourceError::Lex(failure)) => {
                                return Err(self.lex_failure(state, failure))
                            }
                            Err(SourceError::Postlex(e)) => return Err(e),
                        };
                        return Err(self.unexpected(state, &token));
                    }
                }
            }
        }
    }

    /// Turn a lexer-level failure into a parse error. By the time the
    /// non-recovering driver reaches here the contextual source has already tried
    /// the **root** (full-terminal) scanner and it too missed (see
    /// [`Contextual::lex_next`](super::token_source::Contextual)), so the character
    /// is *genuinely* un-lexable — a *character* error, not a token error: no token
    /// was ever produced. So this builds [`UnexpectedCharacter`] (matching Python's
    /// `UnexpectedCharacters`, and lark-rs's own basic-lexer and recovering paths —
    /// issue #346), not `UnexpectedToken`.
    ///
    /// `expected` mirrors Python's `UnexpectedCharacters.allowed`: the terminals the
    /// parser can act on in `state`, with the `$END` sentinel **dropped unless it is
    /// the only option** — Python reports `{'B'}` for a state expecting `B` (no
    /// `$END`), `{'A'}` for a state expecting `A` *or* end-of-input (`$END` stripped),
    /// but `{'<END-OF-FILE>'}` for a state expecting *only* end-of-input. Fixing the
    /// pre-#346 bug where the raw action row leaked `$END` into the expected set
    /// alongside real terminals.
    ///
    /// [`UnexpectedCharacter`]: ParseError::UnexpectedCharacter
    fn lex_failure(&self, state: usize, f: LexFailure) -> ParseError {
        let mut allowed: Vec<String> = self
            .table
            .action
            .get(state)
            .map(|row| {
                row.iter()
                    .map(|(t, _)| SymbolId(*t))
                    .filter(|&t| t != SymbolId::END)
                    .map(|t| self.table.symbols.name(t).to_string())
                    .collect()
            })
            .unwrap_or_default();
        allowed.sort();
        // `$END` survives only when nothing else is lexable here — a state that
        // expects end-of-input and nothing more (Python's `<END-OF-FILE>`).
        let expected = if allowed.is_empty() {
            "<END-OF-FILE>".to_string()
        } else {
            allowed.join(", ")
        };
        ParseError::UnexpectedCharacter {
            ch: f.ch,
            line: f.line,
            col: f.col,
            pos: f.pos,
            expected,
        }
    }

    // ─── Panic-mode error recovery (issue #43) ───────────────────────────────
    //
    // Single-token-deletion recovery: when the current state has no action for the
    // lookahead, record the error, ask `on_error` whether to continue, and if so
    // *delete* that token and resume in the same state. This is a token-for-token
    // port of Python Lark's built-in recovery loop (`LALR_Parser.parse` with an
    // `on_error` callback that returns `True`): each `UnexpectedToken` is recovered
    // from by `interactive_parser.resume_parse()`, which has already pulled the bad
    // token off the lexer, so the net effect is "drop the token and carry on" — with
    // the same parse tables, the surviving stream therefore builds the same tree.
    //
    // Termination: every iteration either shifts, reduces (toward ACCEPT), deletes a
    // token, or stops — and a deletion strictly advances the source toward `$END`,
    // so the loop cannot spin. A `$END` error can't be deleted (there's nothing
    // after it); Python re-raises there. lark-rs returns `Ok(None)` (no derivation)
    // rather than fabricating a partial — see [`RecoveredTree`](crate::error::RecoveredTree)
    // and ADR-0019. The recorded `errors` remain the authoritative diagnostics.

    /// Drive the state machine with recovery (issue #223). Mirrors [`run`](Self::run)
    /// but, on a token with no action, passes the error and a [`RecoveryContext`] to
    /// the `on_error` handler. The handler returns a [`RecoveryAction`]:
    ///
    /// * [`Delete`](RecoveryAction::Delete) — delete the offending token, retry next.
    /// * [`Resume`](RecoveryAction::Resume) — the handler fed corrective tokens via
    ///   the context; the errored token is dropped (matching Python's `resume_parse`)
    ///   and the next token is parsed in the handler's new state. At `$END` the
    ///   sentinel is retried. A no-progress guard: no feeds → `Stop`.
    /// * [`Stop`](RecoveryAction::Stop) — stop recovery, no derivation.
    ///
    /// Every recovered error is pushed to `errors`. Returns `Some(tree)` on a
    /// normal ACCEPT, or `None` when recovery cannot reach ACCEPT.
    pub(crate) fn run_recovering<S: TokenSource>(
        &self,
        source: &mut S,
        start: Option<&str>,
        on_error: &mut dyn FnMut(&ParseError, &mut RecoveryContext<'_>) -> RecoveryAction,
        errors: &mut Vec<ParseError>,
    ) -> Result<Option<ParseTree>, ParseError> {
        let mut stack = ParserStack::new(self.initial_state(start)?);

        loop {
            let token = match source.peek(stack.position()) {
                Ok(tok) => tok,
                Err(SourceError::Lex(failure)) => {
                    let err = ParseError::UnexpectedCharacter {
                        ch: failure.ch,
                        line: failure.line,
                        col: failure.col,
                        pos: failure.pos,
                        expected: "any token".to_string(),
                    };
                    let mut ctx = RecoveryContext::new(&mut stack, &self.table);
                    let action = on_error(&err, &mut ctx);
                    let handler_tree = ctx.accepted_tree.take();
                    errors.push(err);
                    if let Some(tree) = handler_tree {
                        return Ok(Some(tree));
                    }
                    if matches!(action, RecoveryAction::Stop) {
                        return Ok(None);
                    }
                    // Lex failures always skip one character — the character
                    // can't be lexed regardless of action. Delete and Resume
                    // are equivalent here (Python always calls
                    // `s.line_ctr.feed(...)` to advance past the un-lexable
                    // character).
                    source.skip_char();
                    continue;
                }
                Err(SourceError::Postlex(e)) => return Err(e),
            };

            match stack.feed_token(&self.table, &token) {
                Feed::Shifted => source.advance(),
                Feed::Accepted(tree) => return Ok(Some(tree)),
                Feed::Error(e) => return Err(e),
                Feed::NoAction => {
                    let err = self.unexpected(stack.position(), &token);
                    let is_end = token.type_id == SymbolId::END;
                    let mut ctx = RecoveryContext::new(&mut stack, &self.table);
                    let action = on_error(&err, &mut ctx);
                    let fed = ctx.fed_count();
                    let handler_tree = ctx.accepted_tree.take();
                    errors.push(err);
                    if let Some(tree) = handler_tree {
                        return Ok(Some(tree));
                    }
                    match action {
                        RecoveryAction::Delete if is_end => {
                            return Ok(None);
                        }
                        RecoveryAction::Delete => {
                            source.advance();
                            source.on_delete();
                        }
                        RecoveryAction::Resume if fed == 0 => {
                            return Ok(None);
                        }
                        RecoveryAction::Resume if is_end => {
                            // At $END the handler fed corrective tokens;
                            // retry $END in the (now-advanced) parser state.
                        }
                        RecoveryAction::Resume => {
                            // Python's resume_parse() always drops the errored
                            // token; the handler's feeds advanced the parser
                            // state, and the *next* token is parsed in that
                            // new state — not the errored one.
                            source.advance();
                            source.on_delete();
                        }
                        RecoveryAction::Stop => {
                            return Ok(None);
                        }
                    }
                }
            }
        }
    }

    /// Recovering parse over a pre-tokenized sequence (basic lexer). See
    /// [`run_recovering`](Self::run_recovering). `Ok(None)` means recovery could
    /// not reach ACCEPT (no fabricated partial — issue #167).
    pub fn parse_recovering(
        &self,
        tokens: Vec<Token>,
        start: Option<&str>,
        on_error: &mut dyn FnMut(&ParseError, &mut RecoveryContext<'_>) -> RecoveryAction,
        errors: &mut Vec<ParseError>,
    ) -> Result<Option<ParseTree>, ParseError> {
        self.run_recovering(&mut PreLexed::new(tokens), start, on_error, errors)
    }

    /// Recovering parse over the **contextual** lexer (issue #166). Unlike the
    /// basic-lexer recovery path (which pre-lexes the whole stream with the global
    /// terminal set), this recovers over the contextual token stream: the
    /// [`ContextualRecovering`] source narrows terminals by parser state at each
    /// position and falls back to the root (full-terminal) scanner only where the
    /// per-state scanner refuses — Python Lark's `ContextualLexer.lex` except-branch.
    /// A root match there is an out-of-context-but-valid token the recovery loop
    /// deletes; a root miss is an un-lexable character it skips. So a grammar whose
    /// contextual lexer is load-bearing recovers to the same tree a clean contextual
    /// parse would build.
    ///
    /// [`ContextualRecovering`]: crate::parsers::ContextualRecovering
    pub fn parse_contextual_recovering(
        &self,
        text: &str,
        lexer: &ContextualLexer,
        start: Option<&str>,
        on_error: &mut dyn FnMut(&ParseError, &mut RecoveryContext<'_>) -> RecoveryAction,
        errors: &mut Vec<ParseError>,
    ) -> Result<Option<ParseTree>, ParseError> {
        self.run_recovering(
            &mut ContextualRecovering::new(text, lexer),
            start,
            on_error,
            errors,
        )
    }

    /// Parse a pre-tokenized sequence (basic lexer).
    pub fn parse(&self, tokens: Vec<Token>, start: Option<&str>) -> Result<ParseTree, ParseError> {
        self.run(&mut PreLexed::new(tokens), start)
    }

    /// Parse using the contextual lexer — lex one token at a time, feeding the
    /// current parser state to the lexer so it only tries valid terminals.
    pub fn parse_contextual(
        &self,
        text: &str,
        lexer: &ContextualLexer,
        start: Option<&str>,
    ) -> Result<ParseTree, ParseError> {
        self.run(&mut Contextual::new(text, lexer), start)
    }

    /// Parse using the contextual lexer with a streaming [`Indenter`] postlex hook
    /// (issue #67). The hook injects INDENT/DEDENT into the lazy token stream; the
    /// indenter's newline terminal must already be forced into every state's
    /// scanner (`always_accept`, set up in `build_frontend`).
    ///
    /// [`Indenter`]: crate::postlex::Indenter
    pub fn parse_contextual_postlex(
        &self,
        text: &str,
        lexer: &ContextualLexer,
        postlex: &crate::postlex::Indenter,
        symbols: &SymbolTable,
        start: Option<&str>,
    ) -> Result<ParseTree, ParseError> {
        let mut source = postlex_contextual_source(text, lexer, postlex, symbols)?;
        self.run(&mut source, start)
    }

    /// Recovering parse over the contextual lexer **with** a streaming [`Indenter`]
    /// postlex hook (issue #94, sub-target 1). The streaming indenter runs over the
    /// recovering contextual stream ([`ContextualRecovering`], issue #166), and the
    /// shared [`run_recovering`](Self::run_recovering) loop deletes offending tokens
    /// *downstream* of the INDENT/DEDENT injection — exactly Python Lark's
    /// `lexer → PostLexConnector(postlex) → parser` wiring, where `on_error`/
    /// `resume_parse` operate on the post-indenter stream. A deleted token therefore
    /// never reaches the indenter, so its bracket/indent bookkeeping cannot desync,
    /// and a contextual-load-bearing grammar recovers to the same tree a clean parse
    /// would build. An indenter error (e.g. a bad dedent) surfaces as a hard
    /// [`ParseError`] via [`SourceError::Postlex`], as Python re-raises it.
    ///
    /// [`Indenter`]: crate::postlex::Indenter
    /// [`ContextualRecovering`]: crate::parsers::ContextualRecovering
    pub fn parse_contextual_postlex_recovering(
        &self,
        text: &str,
        lexer: &ContextualLexer,
        postlex: &crate::postlex::Indenter,
        symbols: &SymbolTable,
        start: Option<&str>,
        on_error: &mut dyn FnMut(&ParseError, &mut RecoveryContext<'_>) -> RecoveryAction,
        errors: &mut Vec<ParseError>,
    ) -> Result<Option<ParseTree>, ParseError> {
        let mut source = postlex_contextual_recovering_source(text, lexer, postlex, symbols)?;
        self.run_recovering(&mut source, start, on_error, errors)
    }

    /// Recovering parse over the **basic** (global) lexer with a streaming
    /// [`Indenter`] postlex hook (issue #94, sub-target 1) — the basic-lexer postlex
    /// driver. A lazy [`BasicRecovering`] source feeds the same streaming indenter +
    /// per-resume-reset machine the contextual path uses, so both postlex recovery
    /// paths share the exact Python semantics (`Indenter.process` reset on each
    /// `resume_parse`), including interleaving char skips with the indenter reset.
    ///
    /// [`Indenter`]: crate::postlex::Indenter
    /// [`BasicRecovering`]: crate::parsers::token_source::BasicRecovering
    pub fn parse_basic_postlex_recovering(
        &self,
        text: &str,
        lexer: &BasicLexer,
        postlex: &crate::postlex::Indenter,
        symbols: &SymbolTable,
        start: Option<&str>,
        on_error: &mut dyn FnMut(&ParseError, &mut RecoveryContext<'_>) -> RecoveryAction,
        errors: &mut Vec<ParseError>,
    ) -> Result<Option<ParseTree>, ParseError> {
        let mut source = postlex_basic_recovering_source(text, lexer, postlex, symbols)?;
        self.run_recovering(&mut source, start, on_error, errors)
    }
}
