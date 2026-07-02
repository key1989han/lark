//! The Earley recognizer: chart construction (predict / scan / complete) over the
//! basic-lexer token stream, building the SPPF as it goes.
//!
//! [`EarleyParser::build_chart`], [`EarleyParser::predict_and_complete`], and
//! [`EarleyParser::scan`]. The Joop-Leo right-recursion shortcut these drive lives
//! in [`super::leo`]; the dynamic-lexer variants in [`super::dynamic`]. Split out
//! of the former monolithic `earley.rs` (no logic change).

use std::collections::HashMap;

use crate::error::ParseError;
use crate::grammar::intern::SymbolId;
use crate::tree::Token;

use super::chart::{Column, Item, ScanSet};
use super::forest::{Forest, ForestRef, NodeKey, Trans};
use super::EarleyParser;

impl EarleyParser {
    // ─── Chart construction (recognizer + forest) ─────────────────────────────

    /// Build the Earley chart and SPPF over `toks` from `start_id`. On success
    /// returns the forest and the node id of the completed start symbol spanning
    /// the whole input; otherwise a parse error.
    pub(crate) fn build_chart(
        &self,
        toks: &[&Token],
        start_id: SymbolId,
    ) -> Result<(Forest, usize), ParseError> {
        let n = toks.len();
        let mut forest = Forest::new();
        let mut columns: Vec<Column> = vec![Column::new()];
        let mut to_scan = ScanSet::new();

        // Joop-Leo state. `transitives[j]` maps a recognized symbol to the
        // memoized transitive rooted at column `j` (grown in lockstep with
        // `columns`); `trans_arena` is the stable backing store the `parent`
        // links index into.
        let mut transitives: Vec<HashMap<SymbolId, usize>> = vec![HashMap::new()];
        let mut trans_arena: Vec<Trans> = Vec::new();

        // Predict the start symbol into column 0 (non-terminal items) / the scan
        // buffer (terminal items).
        if let Some(prods) = self.rules_by_origin.get(&start_id) {
            for &ri in prods {
                let item = Item {
                    rule: ri,
                    dot: 0,
                    origin: 0,
                    node: ForestRef::None,
                };
                if self.expects_terminal(&item) {
                    to_scan.add(item);
                } else {
                    columns[0].add(item, self.expect(&item));
                }
            }
        }

        let mut i = 0;
        loop {
            while transitives.len() < columns.len() {
                transitives.push(HashMap::new());
            }
            self.predict_and_complete(
                i,
                &mut columns,
                &mut to_scan,
                &mut forest,
                &mut transitives,
                &mut trans_arena,
                start_id,
            );
            if i == n {
                break;
            }
            let token = toks[i];
            match self.scan(token, &mut columns, &to_scan, &mut forest) {
                Some(next_scan) => {
                    to_scan = next_scan;
                }
                None => {
                    // No per-state expected set exists for Earley (the scan set
                    // is per-item, not a table row), so the report carries none.
                    return Err(ParseError::unexpected_token(token, vec![]));
                }
            }
            i += 1;
        }

        // The root is the completed start symbol spanning (0, n): one node in the
        // forest's global index, holding all of its derivations. Expand the
        // Joop-Leo deferred reconstructions reachable from it before the forest→tree
        // walk reads them.
        let root = forest.index.get(&(NodeKey::Sym(start_id), 0, n)).copied();
        if let Some(root) = root {
            self.load_leo_paths(&mut forest, &trans_arena, root);
        }
        root.map(|root| (forest, root)).ok_or_else(|| {
            let (line, col) = toks
                .last()
                .map(|t| (t.end_line.max(t.line), t.end_column.max(t.column)))
                .unwrap_or((1, 1));
            ParseError::unexpected_eof(line as usize, col as usize, vec![])
        })
    }

    /// Scott's predictor + completer for one column. Processes the column as a
    /// LIFO worklist (matching the reference's `deque.pop()`), so newly derived
    /// items are handled before older ones — the order that fixes resolve
    /// tie-breaks.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn predict_and_complete(
        &self,
        i: usize,
        columns: &mut Vec<Column>,
        to_scan: &mut ScanSet,
        forest: &mut Forest,
        transitives: &mut [HashMap<SymbolId, usize>],
        trans_arena: &mut Vec<Trans>,
        start_id: SymbolId,
    ) {
        // Held (ε) completions at this column: origin → its empty node.
        let mut held: HashMap<SymbolId, usize> = HashMap::new();
        let mut stack: Vec<Item> = columns[i].items.clone();

        while let Some(item) = stack.pop() {
            if self.is_complete(&item) {
                let origin = self.grammar.rules[item.rule].origin;

                // Ensure this completed item has a forest node. A node is absent
                // only for an ε production predicted directly as complete — give it
                // the empty (None, None) family that represents the ε derivation.
                let node_id = match item.node {
                    ForestRef::Node(id) => id,
                    _ => {
                        let id = forest.get_or_create(NodeKey::Sym(origin), item.origin, i);
                        // ε derivation: no right child, so right_pos is unused.
                        forest.add_family(
                            id,
                            item.rule,
                            self.grammar.rules[item.rule].order,
                            ForestRef::None,
                            ForestRef::None,
                            0,
                        );
                        id
                    }
                };

                if item.origin == i {
                    held.insert(origin, node_id);
                }

                // ── Joop-Leo right-recursion shortcut ─────────────────────────
                // For a non-empty completion, if the reduction path out of this
                // symbol is *deterministic* (a unique quasi-complete originator at
                // each step), memoize/extend the transitive chain and jump straight
                // to the topmost item instead of cascading through every
                // intermediate completion. The skipped reduction spine is recorded
                // as a deferred path on the topmost node (`load_leo_paths` rebuilds
                // it lazily) so the SPPF stays identical to the non-Leo forest.
                // This collapses O(n²) right-recursion completions to O(n) — and,
                // crucially, the regular waiter cascade below (which the perf
                // counter measures) is skipped entirely on the Leo path.
                if item.origin != i {
                    self.create_leo(
                        origin,
                        item.origin,
                        columns,
                        transitives,
                        trans_arena,
                        start_id,
                    );
                    if let Some(&t) = transitives[item.origin].get(&origin) {
                        let top = trans_arena[t].top;
                        let top_key = self.node_key(top.rule, top.dot);
                        let top_node = forest.get_or_create(top_key, top.origin, i);
                        forest.add_path(top_node, t, ForestRef::Node(node_id), i);
                        let new_item = Item {
                            node: ForestRef::Node(top_node),
                            ..top
                        };
                        if self.expects_terminal(&new_item) {
                            to_scan.add(new_item);
                        } else if columns[i].add(new_item, self.expect(&new_item)) {
                            stack.push(new_item);
                        }
                        continue; // Leo handled it — skip the regular waiter cascade
                    }
                }

                // Advance every item in the origin column that was waiting on this
                // non-terminal. The column's `waiting` index gives exactly those
                // items in O(matches) instead of an O(column) rescan; snapshot into
                // an owned Vec first (we mutate columns[i] below).
                //
                // Counting the items examined here (deterministically, behind the
                // `perf-counters` feature) is the Arm-1 scaling signal #56 demands:
                // it now stays flat per input byte even on right recursion, the
                // gateable proof the rescan quadratic is gone (`tests/test_earley_scaling.rs`).
                let origin_col = &columns[item.origin];
                let waiters = origin_col.waiting_on(origin);
                crate::perf::add_completer_scan_steps(waiters.len() as u64);
                let originators: Vec<Item> =
                    waiters.iter().map(|&idx| origin_col.items[idx]).collect();

                for o in originators {
                    let key = self.node_key(o.rule, o.dot + 1);
                    let new_node = forest.get_or_create(key, o.origin, i);
                    // `origin` was expected at position `o.dot`, so the completed
                    // node is the right child at that expansion position.
                    forest.add_family(
                        new_node,
                        o.rule,
                        self.grammar.rules[o.rule].order,
                        o.node,
                        ForestRef::Node(node_id),
                        o.dot,
                    );
                    let advanced = Item {
                        rule: o.rule,
                        dot: o.dot + 1,
                        origin: o.origin,
                        node: ForestRef::Node(new_node),
                    };
                    if self.expects_terminal(&advanced) {
                        to_scan.add(advanced);
                    } else if columns[i].add(advanced, self.expect(&advanced)) {
                        stack.push(advanced);
                    }
                }
            } else if let Some(sym) = self.expect(&item) {
                if self.is_terminal(sym) {
                    continue; // terminal-expecting items belong to the scan buffer
                }

                let mut new_items: Vec<Item> = Vec::new();
                if let Some(prods) = self.rules_by_origin.get(&sym) {
                    for &ri in prods {
                        new_items.push(Item {
                            rule: ri,
                            dot: 0,
                            origin: i,
                            node: ForestRef::None,
                        });
                    }
                }
                // If `sym` already completed empty at this column, advance past it
                // now (held completion) so ε-derivations propagate.
                if let Some(&hnode) = held.get(&sym) {
                    let key = self.node_key(item.rule, item.dot + 1);
                    let new_node = forest.get_or_create(key, item.origin, i);
                    // The held (ε) symbol was expected at position `item.dot`.
                    forest.add_family(
                        new_node,
                        item.rule,
                        self.grammar.rules[item.rule].order,
                        item.node,
                        ForestRef::Node(hnode),
                        item.dot,
                    );
                    new_items.push(Item {
                        rule: item.rule,
                        dot: item.dot + 1,
                        origin: item.origin,
                        node: ForestRef::Node(new_node),
                    });
                }

                for new in new_items {
                    if self.expects_terminal(&new) {
                        to_scan.add(new);
                    } else if columns[i].add(new, self.expect(&new)) {
                        stack.push(new);
                    }
                }
            }
        }
    }

    /// Scott's scanner: advance every terminal-expecting item that matches
    /// `token`, recording a token-leaf packed node. Returns the next column's scan
    /// buffer, or `None` if nothing matched (a parse failure).
    pub(crate) fn scan(
        &self,
        token: &Token,
        columns: &mut Vec<Column>,
        to_scan: &ScanSet,
        forest: &mut Forest,
    ) -> Option<ScanSet> {
        let mut next_scan = ScanSet::new();
        let mut next_col = Column::new();
        // The column being built is the next one (its index = current length).
        let end = columns.len();

        // One token leaf per position, shared by every item that scans it (so
        // packed-node de-duplication works).
        let tok_ref = ForestRef::Tok(forest.add_token(token.clone()));

        for item in &to_scan.items {
            if self.expect(item) == Some(token.type_id) {
                let key = self.node_key(item.rule, item.dot + 1);
                let new_node = forest.get_or_create(key, item.origin, end);
                // The scanned terminal is the right child at position `item.dot`.
                forest.add_family(
                    new_node,
                    item.rule,
                    self.grammar.rules[item.rule].order,
                    item.node,
                    tok_ref,
                    item.dot,
                );
                let advanced = Item {
                    rule: item.rule,
                    dot: item.dot + 1,
                    origin: item.origin,
                    node: ForestRef::Node(new_node),
                };
                if self.expects_terminal(&advanced) {
                    next_scan.add(advanced);
                } else {
                    next_col.add(advanced, self.expect(&advanced));
                }
            }
        }

        if next_scan.items.is_empty() && next_col.items.is_empty() {
            return None;
        }
        columns.push(next_col);
        Some(next_scan)
    }
}
