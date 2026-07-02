//! The Earley dynamic lexer (Phase 2, Sprint 5): scanning folded into the parse
//! loop, trying only the terminals the parser predicts at each input position.
//!
//! [`EarleyParser::build_chart_dynamic`] and [`EarleyParser::scan_dynamic`]. The
//! predict/complete phase is the shared [`EarleyParser::predict_and_complete`] in
//! [`super::recognizer`]; only the scanner differs. Split out of the former
//! monolithic `earley.rs` (no logic change).

use std::collections::HashMap;

use crate::error::ParseError;
use crate::grammar::intern::SymbolId;
use crate::lexer::DynamicMatcher;
use crate::tree::{PosInt, Token};

use super::chart::{Column, Delayed, Item, ScanSet};
use super::forest::{Forest, ForestRef, NodeKey, Trans};
use super::EarleyParser;

impl EarleyParser {
    // ─── Dynamic lexer (Sprint 5) ─────────────────────────────────────────────

    /// Build the Earley chart and SPPF over `text` using the dynamic lexer.
    ///
    /// Columns are indexed by **character step** `0..=n`; `boundaries[i]` is the
    /// byte offset where step `i` starts (regex matching is byte-based). The
    /// predict/complete phase is identical to the basic-lexer path
    /// ([`predict_and_complete`](Self::predict_and_complete)); only the scanner
    /// differs — see [`scan_dynamic`](Self::scan_dynamic).
    pub(crate) fn build_chart_dynamic(
        &self,
        text: &str,
        start_id: SymbolId,
        matcher: &DynamicMatcher,
        complete_lex: bool,
    ) -> Result<(Forest, usize), ParseError> {
        // Byte offset of every character start, plus the end-of-input offset.
        let boundaries: Vec<usize> = text
            .char_indices()
            .map(|(b, _)| b)
            .chain(std::iter::once(text.len()))
            .collect();
        let n = boundaries.len() - 1;
        let byte_to_step: HashMap<usize, usize> = boundaries
            .iter()
            .enumerate()
            .map(|(i, &b)| (b, i))
            .collect();

        // Per-step (line, column), 1-based and newline-aware — for token positions.
        let mut lines = Vec::with_capacity(n + 1);
        let mut cols = Vec::with_capacity(n + 1);
        {
            let (mut line, mut col) = (1usize, 1usize);
            let mut chars = text.chars();
            for _ in 0..=n {
                lines.push(line);
                cols.push(col);
                match chars.next() {
                    Some('\n') => {
                        line += 1;
                        col = 1;
                    }
                    Some(_) => col += 1,
                    None => {}
                }
            }
        }

        let mut forest = Forest::new();
        let mut columns: Vec<Column> = vec![Column::new()];
        let mut to_scan = ScanSet::new();
        let mut delayed: HashMap<usize, Vec<Delayed>> = HashMap::new();

        // Predict the start symbol into column 0 / the scan buffer.
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

        let mut transitives: Vec<HashMap<SymbolId, usize>> = vec![HashMap::new()];
        let mut trans_arena: Vec<Trans> = Vec::new();

        for i in 0..=n {
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
            match self.scan_dynamic(
                i,
                start_id,
                text,
                &boundaries,
                &byte_to_step,
                &lines,
                &cols,
                matcher,
                complete_lex,
                &mut columns,
                &to_scan,
                &mut forest,
                &mut delayed,
                &trans_arena,
            ) {
                Some(next_scan) => {
                    to_scan = next_scan;
                }
                None => {
                    let pos = boundaries[i];
                    let ch = text[pos..].chars().next().unwrap_or('\u{0}');
                    return Err(ParseError::UnexpectedCharacter {
                        ch,
                        line: lines[i],
                        col: cols[i],
                        pos,
                        expected: "valid token for the dynamic lexer".to_string(),
                    });
                }
            }
        }

        let root = forest.index.get(&(NodeKey::Sym(start_id), 0, n)).copied();
        if let Some(root) = root {
            self.load_leo_paths(&mut forest, &trans_arena, root);
        }
        root.map(|root| (forest, root)).ok_or_else(|| {
            ParseError::unexpected_eof(
                *lines.last().unwrap_or(&1),
                *cols.last().unwrap_or(&1),
                vec![],
            )
        })
    }

    /// The dynamic scanner for character step `i`: match each scan-set item's
    /// predicted terminal at `boundaries[i]` (plus the `%ignore` terminals),
    /// queuing every hit into `delayed` keyed by the step where it ends; then
    /// drain whatever completes at step `i+1` into the freshly pushed column.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn scan_dynamic(
        &self,
        i: usize,
        start_id: SymbolId,
        text: &str,
        boundaries: &[usize],
        byte_to_step: &HashMap<usize, usize>,
        lines: &[usize],
        cols: &[usize],
        matcher: &DynamicMatcher,
        complete_lex: bool,
        columns: &mut Vec<Column>,
        to_scan: &ScanSet,
        forest: &mut Forest,
        delayed: &mut HashMap<usize, Vec<Delayed>>,
        trans_arena: &[Trans],
    ) -> Option<ScanSet> {
        let pos = boundaries[i];
        let mk_token = |term: SymbolId, value: &str, end_step: usize| Token {
            type_id: term,
            type_: matcher.name(term).to_string(),
            value: value.to_string(),
            line: lines[i] as PosInt,
            column: cols[i] as PosInt,
            end_line: lines[end_step] as PosInt,
            end_column: cols[end_step] as PosInt,
            // `start_pos`/`end_pos` are **character** indices (Python parity, #278).
            // Columns here are indexed by character step, so the step index *is* the
            // char index — `i` and `end_step`, not the byte offsets
            // `boundaries[i]`/`boundaries[end_step]`.
            start_pos: i as PosInt,
            end_pos: end_step as PosInt,
        };

        // 1) Match each scan-set item's predicted terminal here. A hit is *delayed*
        //    to the step where it ends, so a longer token is acted on only when the
        //    parse reaches its end (this is what makes overlapping terminals work).
        for item in &to_scan.items {
            let Some(term) = self.expect(item) else {
                continue;
            };
            if let Some(value) = matcher.match_at(term, text, pos) {
                let end_step = byte_to_step[&(pos + value.len())];
                let token = mk_token(term, value, end_step);
                delayed
                    .entry(end_step)
                    .or_default()
                    .push(Delayed::Tok { item: *item, token });

                // dynamic_complete: also queue every shorter prefix tokenization.
                if complete_lex {
                    for &cut in &boundaries[i + 1..end_step] {
                        if let Some(short) = matcher.match_in(term, &text[pos..cut]) {
                            let es = byte_to_step[&(pos + short.len())];
                            let token = mk_token(term, short, es);
                            delayed
                                .entry(es)
                                .or_default()
                                .push(Delayed::Tok { item: *item, token });
                        }
                    }
                }
            }
        }

        // 2) Ignored spans (e.g. whitespace): carry every scan-set item — and any
        //    completed start item — past the span unchanged, so ignored text can
        //    sit between tokens (and after the last one) without consuming a symbol.
        for &ig in matcher.ignore() {
            if let Some(value) = matcher.match_at(ig, text, pos) {
                let end_step = byte_to_step[&(pos + value.len())];
                let bucket = delayed.entry(end_step).or_default();
                for item in &to_scan.items {
                    bucket.push(Delayed::Carry { item: *item });
                }
                for item in &columns[i].items {
                    if self.is_complete(item)
                        && item.origin == 0
                        && self.grammar.rules[item.rule].origin == start_id
                    {
                        bucket.push(Delayed::Carry { item: *item });
                    }
                }
            }
        }

        // 3) Drain the matches that complete at step i+1 into the next column.
        let mut next_scan = ScanSet::new();
        let mut next_col = Column::new();
        let end = i + 1;

        for d in delayed.remove(&(i + 1)).unwrap_or_default() {
            match d {
                Delayed::Tok { item, token } => {
                    let key = self.node_key(item.rule, item.dot + 1);
                    let new_node = forest.get_or_create(key, item.origin, end);
                    let tok_ref = ForestRef::Tok(forest.add_token(token));
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
                Delayed::Carry { item } => {
                    // The item keeps its dot, but it now spans one step further (past
                    // the ignored text). Re-anchor its forest node to the carried span
                    // `[origin, end]` through the global `(key, origin, end)` index and
                    // merge in the families of its pre-ignore node — xearley.py's
                    // ignore carry-over (the `token is None` branch), which builds a
                    // fresh node at the new label and copies the old node's children.
                    //
                    // Re-anchoring is what makes the carried derivation survive: a
                    // completion or scan landing at the same `(key, origin, end)` shares
                    // this very node, so their derivations *merge* as alternative
                    // families instead of one shadowing the other. The previous
                    // `index.entry(..).or_insert(..)` dropped the carried derivation on
                    // any such collision (and `ScanSet`/`Column` dedup keeps a single
                    // item, so the lone surviving item must point at the merged node).
                    let carried = if let ForestRef::Node(old) = item.node {
                        // The carried derivation may be a deferred Joop-Leo path
                        // rather than a materialized family (a completed right-
                        // recursive rule reached through the Leo shortcut). Force it
                        // into families first, otherwise the copy below would carry an
                        // empty node and the derivation would be lost across the
                        // ignore (the previous `or_insert` kept the *same* node id, so
                        // the deferred path stayed reachable from the root; copying
                        // into a fresh node does not, so it must be materialized now).
                        self.materialize_leo_paths(forest, trans_arena, old);
                        let key = self.node_key(item.rule, item.dot);
                        let new_node = forest.get_or_create(key, item.origin, end);
                        if old != new_node {
                            let fams = forest.nodes[old].families.clone();
                            for f in fams {
                                forest.add_family(
                                    new_node,
                                    f.rule,
                                    self.grammar.rules[f.rule].order,
                                    f.left,
                                    f.right,
                                    f.right_pos,
                                );
                            }
                        }
                        Item {
                            node: ForestRef::Node(new_node),
                            ..item
                        }
                    } else {
                        item
                    };
                    if self.is_complete(&carried) {
                        next_col.add(carried, None);
                    } else if self.expects_terminal(&carried) {
                        next_scan.add(carried);
                    } else {
                        next_col.add(carried, self.expect(&carried));
                    }
                }
            }
        }

        columns.push(next_col);

        // Dead end: nothing advanced into the next column and no match is still
        // pending further ahead — the input cannot be tokenized from here.
        if next_scan.items.is_empty() && columns[i + 1].items.is_empty() && delayed.is_empty() {
            return None;
        }
        Some(next_scan)
    }
}
