//! The original `regex`-crate combined-alternation scanner (the `Regex` backend),
//! plus the per-terminal [`SideProbe`] for lookaround terminals.

use std::cell::RefCell;
use std::collections::HashMap;

use regex::{CaptureLocations, Regex};

use super::dfa::LoweredTerminalMatcher;
use super::plan::{scanner_plan, RetypeTable};
use super::record_scan_skip;
use crate::error::GrammarError;
use crate::grammar::intern::SymbolId;
use crate::grammar::terminal::TerminalDef;

// ─── SideProbe: a per-terminal matcher for a regex-crate-rejected terminal ────
//
// The combined scanner is built on the linear-time `regex` crate, which has no
// lookahead/lookbehind. A terminal the `regex` crate rejects either LOWERS into its
// own single-terminal DFA ([`LoweredTerminalMatcher`], the default build) or — under
// the TEST-ONLY `fancy-oracle` feature — is matched by the historical `\G`-anchored
// `fancy-regex` probe, so the `Regex` reference backend stays an independent oracle
// for the whole-lexer differential. There is no runtime fallback engine, and the
// feature never widens the accepted grammar set: BOTH builds route every
// regex-rejected terminal through THE refusal seam (`route_fancy_only_terminal`)
// first, so a terminal the lowering refuses fails the grammar build with the same
// categorized scope error (`docs/LOOKAROUND_SCOPE.md`) with and without the feature.
// The feature only swaps the *matcher* for terminals that lower.
//
// Fence-idiom terminals (`fence.rs` — named-backref tag echoes) are the one shape
// that bypasses the seam in BOTH builds, by the same recognizer, so acceptance is
// still identical: the default build matches them via the `FenceMatcher` inside
// `LoweredTerminalMatcher`, while the fancy build keeps the `\G` probe —
// `fancy-regex` natively supports `(?P=name)`, which makes the feature build a
// genuinely independent oracle for exactly the newest matcher.

enum SideProbe {
    /// The lowered single-terminal DFA — the default build (and the engine whose
    /// semantics the combined `DfaScanner` shares by construction). Under
    /// `fancy-oracle` every lowered terminal gets a [`Self::Fancy`] probe instead,
    /// so this variant is never constructed there — it exists so the
    /// single-variant default build is the same type.
    #[cfg_attr(feature = "fancy-oracle", allow(dead_code))]
    Lowered(LoweredTerminalMatcher),
    /// TEST-ONLY historical reference: the per-position `\G`-anchored fancy probe.
    /// (`\G` makes `find_from_pos` fail immediately when nothing matches at `pos`
    /// instead of forward-scanning — the linearity fix; see the module docs.)
    #[cfg(feature = "fancy-oracle")]
    Fancy(fancy_regex::Regex),
}

impl SideProbe {
    /// End offset of a non-empty match beginning *exactly* at `pos`, or `None`.
    /// The full `text` (not a suffix) is passed so a lookbehind can see the bytes
    /// before `pos`.
    fn match_end_at(&self, text: &str, pos: usize) -> Option<usize> {
        match self {
            SideProbe::Lowered(m) => m.match_end_at(text, pos),
            #[cfg(feature = "fancy-oracle")]
            SideProbe::Fancy(re) => {
                let m = re.find_from_pos(text, pos).ok().flatten();
                record_scan_skip(pos, m.as_ref().map(|m| m.start()));
                let m = m?;
                (m.start() == pos && m.end() > pos).then_some(m.end())
            }
        }
    }
}

// ─── Scanner: one compiled alternation over a set of terminals ────────────────

/// A compiled scanner over a fixed set of terminals.
///
/// Matching is leftmost-first (Python-`re` semantics), so the alternation order
/// breaks ties. The `unless` map carries Lark's keyword retyping (see module
/// docs). Capture-group names are derived from the symbol id (`g{n}`), so no
/// terminal-name sanitization is needed.
///
/// Two allocation-avoidance measures (profiling spike, 2026-06-04 — both shared
/// by the future Earley engine, which scans through this same `Scanner`):
///
///   * each terminal's capture-group *index* is resolved once at build time, so
///     `match_at` reads the winning group by number instead of hashing a group
///     *name* per token (the SipHash cost the profiler flagged);
///   * a single [`CaptureLocations`] scratch buffer is reused across matches
///     (`captures_read_at`) rather than allocating a fresh `Captures` per token.
pub(super) struct Scanner {
    /// Combined alternation over every *plain* (`regex`-crate) terminal, or `None`
    /// when this scanner's terminals are all lookaround terminals. Returns the
    /// lowest-rank plain terminal matching at a position (leftmost-first).
    re: Option<Regex>,
    /// (terminal id, capture-group index, rank), in alternation order. `rank` is the
    /// terminal's index in the fully-sorted candidate list, so a plain match can be
    /// compared against a fancy match by who Python's combined alternation would
    /// reach first.
    groups: Vec<(SymbolId, usize, usize)>,
    /// Lookaround terminals, each matched individually by its [`SideProbe`]
    /// (lowered by default; the historical fancy probe under the TEST-ONLY
    /// `fancy-oracle` feature). Stored in ascending `rank` order, so the first one
    /// that matches is the lowest-rank side candidate. Empty for the overwhelming
    /// common case (no lookaround).
    side: Vec<(usize, SymbolId, SideProbe)>,
    /// regex-terminal-id → compiled keyword retype table.
    unless: Vec<Option<RetypeTable>>,
    /// Reused match-location scratch, sized for `re`. `RefCell` because the hot
    /// `match_at` runs behind `&self` (the contextual lexer's per-token path).
    locs: Option<RefCell<CaptureLocations>>,
}

impl Scanner {
    /// Build a scanner from candidate terminals (deduplicated by id).
    ///
    /// `global_flags` is Lark's `g_regex_flags`: a flag bitset applied to the
    /// whole combined regex (and to the `unless` membership tests) so that, e.g.,
    /// `IGNORECASE` makes every terminal — string literals included — match
    /// case-insensitively, without mutating the individual `TerminalDef`s.
    pub(super) fn build(
        terminals: &[(SymbolId, &TerminalDef)],
        global_flags: u32,
    ) -> Result<Scanner, GrammarError> {
        // The selection + ordering + unless retyping is shared with the standalone
        // generator (`scanner_plan`) so a baked scanner is byte-identical to this
        // runtime one.
        let plan = scanner_plan(terminals, global_flags)?;
        let unless = RetypeTable::build_all_dense(&plan.unless)?;
        let prefix = plan.global_prefix;

        // Split the plan's (rank-ordered) terminals into *plain* terminals — which
        // go into the single fast combined `regex` alternation — and *lookaround*
        // terminals (the `regex` crate rejects them), which are matched individually
        // via a [`SideProbe`]. `rank` is the index in the plan's sorted order: Python
        // builds the alternation in this order and takes the first branch that
        // matches, so the lowest rank wins ties. We preserve it to merge the two
        // engines' candidates at match time.
        let by_id: HashMap<SymbolId, &TerminalDef> =
            terminals.iter().map(|(id, t)| (*id, *t)).collect();
        let mut parts = Vec::new();
        let mut group_names = Vec::new();
        let mut side: Vec<(usize, SymbolId, SideProbe)> = Vec::new();
        for (rank, (id, inline)) in plan.groups.iter().enumerate() {
            // `to_inline_regex` (used by `scanner_plan`) keeps per-terminal flags
            // (e.g. `(?i:…)` for a case-insensitive terminal) scoped to this group.
            match Regex::new(&format!("{prefix}{inline}")) {
                Ok(_) => {
                    let group = format!("g{}", id.0);
                    parts.push(format!("(?P<{group}>{inline})"));
                    group_names.push((*id, group, rank));
                }
                #[cfg(feature = "fancy-oracle")]
                Err(e) => {
                    // Acceptance is decided by THE refusal seam FIRST, exactly as the
                    // default build below decides it — so the TEST-ONLY feature can
                    // never change what a grammar build accepts (the Cargo.toml
                    // contract). Only a terminal that *lowers* proceeds to the probe.
                    // A fence-idiom terminal is the one exception in BOTH builds (the
                    // recognizer is the shared acceptance test); its `\G` fancy probe
                    // below works as-is — fancy-regex supports `(?P=name)` natively.
                    if super::fence::recognize_fence_idiom_from_def(by_id[id]).is_none() {
                        super::route::route_fancy_only_terminal(
                            by_id[id],
                            global_flags,
                            &e.to_string(),
                        )?;
                    }
                    // The historical reference matcher for the lowered terminal: the
                    // `\G`-anchored fancy probe (`\G` anchors `find_from_pos` to
                    // `pos` so a sparse terminal stays linear; the `regex` crate
                    // cannot parse `\G`, so this pattern lives on the fancy engine
                    // only). This is what makes the feature build an independent
                    // oracle for the differential.
                    let src = format!("{prefix}\\G{inline}");
                    let anchored =
                        fancy_regex::Regex::new(&src).map_err(|e| GrammarError::InvalidRegex {
                            pattern: src.clone(),
                            reason: e.to_string(),
                        })?;
                    side.push((rank, *id, SideProbe::Fancy(anchored)));
                }
                #[cfg(not(feature = "fancy-oracle"))]
                Err(e) => {
                    // Default build: lower the terminal through THE refusal seam —
                    // its own single-terminal DFA, or the categorized scope error
                    // (identical to the `DfaScanner` backend's policy).
                    let m = LoweredTerminalMatcher::build(
                        *id,
                        by_id[id],
                        global_flags,
                        &e.to_string(),
                    )?;
                    side.push((rank, *id, SideProbe::Lowered(m)));
                }
            }
        }

        // The combined regex over plain terminals (skipped entirely when every
        // candidate is a lookaround terminal).
        let (re, groups, locs) = if parts.is_empty() {
            (None, Vec::new(), None)
        } else {
            let pattern = format!("{}{}", prefix, parts.join("|"));
            let re = Regex::new(&pattern).map_err(|e| GrammarError::InvalidRegex {
                pattern: pattern.clone(),
                reason: e.to_string(),
            })?;
            // Resolve each terminal's named group to its capture *index* once. A
            // terminal pattern may itself contain capturing groups, so the index is
            // not the alternation position — read it from `capture_names` (which
            // enumerates every group in index order, unnamed ones as `None`).
            let name_to_idx: HashMap<&str, usize> = re
                .capture_names()
                .enumerate()
                .filter_map(|(i, n)| n.map(|n| (n, i)))
                .collect();
            let groups = group_names
                .iter()
                .map(|(id, name, rank)| (*id, name_to_idx[name.as_str()], *rank))
                .collect();
            let locs = Some(RefCell::new(re.capture_locations()));
            (Some(re), groups, locs)
        };

        Ok(Scanner {
            re,
            groups,
            side,
            unless,
            locs,
        })
    }

    /// Match a single token starting exactly at `pos`. Returns `(terminal id,
    /// value)`, with keyword retyping already applied. `None` means nothing
    /// matched here.
    ///
    /// The winner is the lowest-`rank` terminal that matches at `pos` — exactly the
    /// branch Python's combined `(A)|(B)|…` alternation reaches first. We get the
    /// lowest-rank *plain* terminal from the combined regex and the lowest-rank
    /// *side-probe* terminal from the (rank-sorted) lookaround list, then keep
    /// whichever has the smaller rank.
    pub(super) fn match_at<'t>(&self, text: &'t str, pos: usize) -> Option<(SymbolId, &'t str)> {
        // Lowest-rank plain candidate.
        let mut best: Option<(usize, SymbolId, &'t str)> = None;
        if let (Some(re), Some(locs)) = (&self.re, &self.locs) {
            let mut locs = locs.borrow_mut();
            let m0 = re.captures_read_at(&mut locs, text, pos);
            record_scan_skip(pos, m0.as_ref().map(|m| m.start()));
            if let Some(m0) = m0 {
                // Accept only a non-empty match beginning exactly at pos.
                if m0.start() == pos && m0.end() != pos {
                    let value = m0.as_str();
                    for (id, idx, rank) in &self.groups {
                        if locs.get(*idx).is_some() {
                            best = Some((*rank, *id, value));
                            break;
                        }
                    }
                }
            }
        }
        // Lowest-rank side candidate (the list is rank-sorted, so the first match
        // wins); keep it only if it out-ranks the plain candidate.
        for (rank, id, re) in &self.side {
            if best.is_some_and(|(b, _, _)| *rank > b) {
                break;
            }
            if let Some(end) = re.match_end_at(text, pos) {
                best = Some((*rank, *id, &text[pos..end]));
                break;
            }
        }

        let (_, id, value) = best?;
        let ty = self
            .unless
            .get(id.index())
            .and_then(|m| m.as_ref())
            .and_then(|m| m.retype(value))
            .unwrap_or(id);
        Some((ty, value))
    }
}
