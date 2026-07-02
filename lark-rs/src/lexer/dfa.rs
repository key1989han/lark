//! The `regex-automata` multi-pattern DFA scanner (the default backend), built
//! as a staged pipeline: **classify** each plan group into unguarded/guarded
//! sub-patterns (lowering lookaround through the refusal seam), **assemble** the
//! two engines, then derive the start-byte **prefilter** — each stage a named
//! function rather than one interleaved build.

use std::cell::RefCell;
use std::collections::HashMap;

use regex::Regex;
use regex_automata::{
    dfa::{dense, Automaton, OverlappingState, StartKind},
    hybrid::dfa::{Cache as LazyCache, DFA as LazyDfa},
    nfa::thompson,
    Anchored, Input, MatchKind,
};

use super::fence::{recognize_fence_idiom_from_def, FenceMatcher};
use super::guard::{Guard, GuardContext, LookbehindGuardC};
use super::pattern::wrap_flags;
use super::plan::{scanner_plan, RetypeTable};
use super::record_scan_skip;
use super::route::route_fancy_only_terminal;
use crate::error::GrammarError;
use crate::grammar::intern::SymbolId;
use crate::grammar::terminal::TerminalDef;

/// The combined scanner (`docs/LEXER_DFA_PLAN.md`). Same contract and selection
/// rules as [`Scanner`](super::scanner::Scanner) — leftmost-first ranking,
/// `unless` retyping — but the *plain* (lookaround-free) terminals are matched
/// by one hand-built `regex-automata` **dense DFA** over all of them, returning
/// a `PatternID`, instead of the `regex`-crate alternation-with-capture-groups
/// trick.
///
/// **M0 re-platform (`docs/LEXER_DFA_PLAN.md`, "L2 re-platforms the engine").** L1's
/// `DfaScanner` was a `meta::Regex::new_many`, whose only input is *pattern strings*
/// and which categorically cannot host a lowered `(?!…)` fragment or expose the
/// per-state accept-set the guarded-accept driver needs. So the engine is rebuilt on
/// the lower layer: each plain terminal is compiled to a Thompson NFA, the terminals
/// are **unioned into one multi-pattern NFA** (`build_many`, `PatternID == rank`),
/// and that NFA is determinized to a `dense::DFA` we drive ourselves through the
/// [`Automaton`] trait. This is the seam M1+ extend: hand-assembled lowered fragments
/// join the same NFA, and the dense DFA exposes `match_pattern`/`match_len` for the
/// accept-set accumulator.
///
/// Why this is byte-identical to [`Scanner`](super::scanner::Scanner):
///
///   * The plain patterns are unioned **in rank order**, so `PatternID` *is* the rank.
///     Built with `MatchKind::LeftmostFirst`, the dense DFA resolves a same-start tie
///     by **pattern order** (lowest `PatternID` wins) with that pattern's own greedy
///     length — exactly Python-`re`'s leftmost-first, the same semantics the
///     `regex`-crate alternation gives (verified: `[ab|abc]` at `"abc"` picks pattern
///     0, length 2, *not* the longest match).
///   * The search is **anchored** at `pos` (`Anchored::Yes` over `pos..len`), so it
///     can only begin exactly at `pos` and never forward-scans. A zero-width match is
///     rejected, mirroring `Scanner`.
///   * The `unless` retype is **copied verbatim** from `Scanner`: only the
///     plain-terminal engine changes.
///
/// **There is no fallback engine (L4).** Every terminal either compiles plain, lowers
/// into the DFA (M1/M2/M3/M4 + the Stage-B idioms — all bundled lookaround terminals),
/// or **fails the build** with the categorized scope error
/// (`route_fancy_only_terminal`, `docs/LOOKAROUND_SCOPE.md`). The historical
/// `fancy-regex` side-probe and its `push_fancy_fallback` compatibility seam are gone;
/// the scanner is fully self-contained `regex-automata` data — the L5 bake target.
///
/// The `regex` crate's combined alternation came with a free literal prefilter; an
/// *anchored* search runs no prefilter of its own, so we re-add an explicit
/// **start-byte prefilter** (`start_bytes`): the set of bytes any plain terminal can
/// begin with, **re-derived from the new union** (a lazy DFA over the plain union).
/// When the byte at `pos` isn't in it we skip the plain engine entirely. It is an
/// over-approximation by construction (a possible start byte is never dropped), so it
/// can only ever *save* an engine call, never change a match — the L0 differential
/// oracle is the proof.
pub(super) struct DfaScanner {
    /// Leftmost-first DFA over the **unguarded** sub-patterns (plain terminals and the
    /// unguarded branches of boundary terminals). `None` when there are none. This is
    /// the M0 engine: it reproduces Python-`re` leftmost-first *exactly*, including a
    /// terminal's own order-sensitive internal alternation (`/ab|abc/` → `"ab"`), so a
    /// sibling guard never disturbs a plain terminal.
    pub(super) plain: Option<PlainEngine>,
    /// All-matches DFA over the **guarded** sub-patterns (branches carrying a leading
    /// and/or trailing boundary guard). `None` when there are none. Driven by the
    /// guarded-accept accumulator (`docs/LEXER_DFA_PLAN.md`, "guarded accept ×
    /// multi-pattern priority").
    pub(super) guarded: Option<GuardedEngine>,
    /// Start-byte prefilter over the base union of both engines (see the struct docs).
    /// `None` disables it (always run the engines).
    start_bytes: Option<Box<[bool; 256]>>,
    /// regex-terminal-id → (matched-text → keyword-terminal-id) — identical retype.
    unless: Vec<Option<RetypeTable>>,
    /// Fence-idiom terminals (tag-echo delimited, `fence.rs`), matched by the
    /// two-phase scanner unconditionally — they do their own open-literal
    /// pre-check and are not included in `start_bytes`. They bypass the refusal
    /// seam: a named backreference is non-regular (the `regex` crate is right to
    /// reject it) but linear-time recognisable per attempt.
    fences: Vec<FenceMatcher>,
}

/// A combined DFA over a set of sub-pattern sources, either **eagerly determinized**
/// (the dense default) or **lazily** (the hybrid fallback for over-budget sources —
/// `#349`/H4-12). Both expose the same anchored leftmost-first / overlapping drives, so
/// a [`PlainEngine`]/[`GuardedEngine`] consults whichever it holds transparently.
///
/// **Why a fallback at all (the H4-12 contract, ADR-0037).** A terminal whose minimal
/// DFA is exponential in its source — the `.*a.{N}` family `/[01]*1[01]{N}/` — blows the
/// dense determinizer to `2^(N+1)` states; Python `re` matches it in linear time (no
/// determinization). The dense build is now bounded by a `dfa_size_limit`; a source that
/// would exceed it is routed here as a **lazy** DFA instead (`regex-automata`'s hybrid
/// engine — the same one the start-byte prefilter already uses), which realizes states on
/// demand and so stays *flat* in build cost. The hybrid DFA produces byte-identical
/// matches, so oracle parity is preserved: the grammar still builds and lexes exactly as
/// Python accepts it — only the eager determinization is skipped.
///
/// The lazy DFA needs a mutable scratch [`LazyCache`] per search. `Lark` is already
/// `!Sync` (the `regex` backend holds a `RefCell` too), so a `RefCell<LazyCache>` here is
/// consistent — a single-threaded reuse of one cache, never a fresh allocation per token.
enum CombinedDfa {
    /// The eager determinization — used for every source that fits the size budget.
    Dense(dense::DFA<Vec<u32>>),
    /// The lazy fallback — used only for over-budget sources (H4-12). The cache is
    /// reused across `match_at` calls (interior-mutable; the scanner is `!Sync`).
    Hybrid {
        dfa: LazyDfa,
        cache: RefCell<LazyCache>,
    },
}

impl CombinedDfa {
    /// Anchored leftmost-first forward search at `input`'s span start — the dense DFA's
    /// `try_search_fwd`, or the lazy DFA's with a borrowed cache. Returns the winning
    /// `(PatternID, end)` (local to this engine's source order), or `None`.
    fn search_fwd(&self, input: &Input<'_>) -> Option<(usize, usize)> {
        match self {
            CombinedDfa::Dense(dfa) => dfa
                .try_search_fwd(input)
                .ok()
                .flatten()
                .map(|hm| (hm.pattern().as_usize(), hm.offset())),
            CombinedDfa::Hybrid { dfa, cache } => dfa
                .try_search_fwd(&mut cache.borrow_mut(), input)
                .ok()
                .flatten()
                .map(|hm| (hm.pattern().as_usize(), hm.offset())),
        }
    }

    /// Enumerate every `(PatternID, end)` accept of the anchored **overlapping**
    /// (all-matches) search, invoking `on_accept` once per distinct accept. The dense and
    /// lazy DFAs use *different* `OverlappingState` types (`dfa::` vs `hybrid::`), so each
    /// owns its own state-threaded loop here rather than sharing one — the caller just
    /// receives `(pid, end)` pairs.
    fn for_each_overlapping(&self, input: &Input<'_>, mut on_accept: impl FnMut(usize, usize)) {
        match self {
            CombinedDfa::Dense(dfa) => {
                let mut state = OverlappingState::start();
                loop {
                    if dfa.try_search_overlapping_fwd(input, &mut state).is_err() {
                        break;
                    }
                    let Some(hm) = state.get_match() else { break };
                    on_accept(hm.pattern().as_usize(), hm.offset());
                }
            }
            CombinedDfa::Hybrid { dfa, cache } => {
                let cache = &mut cache.borrow_mut();
                let mut state = regex_automata::hybrid::dfa::OverlappingState::start();
                loop {
                    if dfa
                        .try_search_overlapping_fwd(cache, input, &mut state)
                        .is_err()
                    {
                        break;
                    }
                    let Some(hm) = state.get_match() else { break };
                    on_accept(hm.pattern().as_usize(), hm.offset());
                }
            }
        }
    }
}

/// Leftmost-first DFA over the unguarded sub-patterns. Sub-patterns are ordered by
/// `(rank, branch_order)`, so the lowest `PatternID` is the leftmost-first winner with
/// its own (order-sensitive) match length — byte-identical to M0.
///
/// A source that overflows the dense size budget (H4-12) lives in a parallel **hybrid**
/// sub-engine (`overflow`); the two sub-engines partition the sources, and `match_at`
/// keeps the lower `(rank, branch_order)` winner across both — leftmost-first by rank is
/// re-merged globally, so the partition never changes which terminal wins.
pub(super) struct PlainEngine {
    /// In-budget sources, eagerly determinized. `local`s are indices into `globals`.
    dfa: CombinedDfa,
    /// `local PatternID` → global sub-pattern index (the `(rank, branch_order)` order).
    globals: Vec<usize>,
    /// Over-budget sources, lazily determinized (the H4-12 fallback). `None` when every
    /// source fit the budget — the common case, zero hybrid cost.
    overflow: Option<(CombinedDfa, Vec<usize>)>,
    /// `global sub-pattern index` → (terminal id, rank, branch_order).
    map: Vec<(SymbolId, usize, usize)>,
}

/// All-matches DFA over the guarded sub-patterns + their guards. Like [`PlainEngine`], an
/// over-budget guarded base lives in a parallel hybrid sub-engine; `guarded_best` walks
/// both accept-sets into the same per-sub `longest` accumulator.
pub(super) struct GuardedEngine {
    /// In-budget sources, eagerly determinized.
    dfa: CombinedDfa,
    /// `local PatternID` → global sub index (into `subs`).
    globals: Vec<usize>,
    /// Over-budget sources, lazily determinized.
    overflow: Option<(CombinedDfa, Vec<usize>)>,
    /// Indexed by the **global** sub index.
    subs: Vec<SubPattern>,
}

/// One lowered sub-pattern fed to the combined NFA. A plain terminal contributes one
/// unguarded sub-pattern; a boundary terminal contributes one per top-level
/// alternation branch (some carrying a leading and/or trailing guard — `lark.OP`).
struct SubPattern {
    id: SymbolId,
    /// The terminal's rank in the sorted plan — cross-terminal leftmost-first.
    rank: usize,
    /// The branch's index within its terminal — within-terminal leftmost-first.
    branch_order: usize,
    /// A guard checked at the match **start** (`pos`) — a leading boundary `(?!S)X`.
    leading: Option<Guard>,
    /// A guard checked at the match **end** — a trailing boundary `X(?!S)`.
    trailing: Option<Guard>,
    /// Bounded-lookbehind guards, each checked *backward* at a fixed char-offset from
    /// the match start (M3). Empty for a branch with no lookbehind.
    lookbehind: Vec<LookbehindGuardC>,
}

/// The classified output of lowering one scanner plan (the build's first stage):
/// every terminal's sub-patterns split by destination engine, plus the inline base
/// union the start-byte prefilter is derived from. `plain_srcs[i]` / `guarded_srcs[i]`
/// is the NFA source of `plain_subs[i]` / `guarded_subs[i]`.
struct ClassifiedSubs {
    plain_subs: Vec<SubPattern>,
    plain_srcs: Vec<String>,
    guarded_subs: Vec<SubPattern>,
    guarded_srcs: Vec<String>,
    base_inlines: Vec<String>,
    /// Fence-idiom terminals — neither engine hosts them (see [`DfaScanner::fences`]).
    fences: Vec<FenceMatcher>,
}

/// Stage 1 — walk the rank-ordered plan, classifying each terminal's lowered branches
/// into **unguarded** sub-patterns (plain terminals + the unguarded branches of
/// boundary terminals) and **guarded** sub-patterns (branches with a leading and/or
/// trailing guard). The two go to two different engines:
///
///   * unguarded → one leftmost-first DFA (M0 semantics, exact within-pattern
///     order — a sibling guard never disturbs `/ab|abc/`);
///   * guarded → one all-matches DFA driven by the guarded-accept accumulator.
///
/// A lookaround terminal whose guarded base is not guard-realizable (see
/// `is_guard_realizable`), or whose lookbehind sits at a variable offset outside
/// a recognized idiom, FAILS THE BUILD with the categorized scope error (L4 —
/// there is no fallback engine). M1/M2/M3 boundary+lookbehind, the M4 STRING
/// splice, and the Stage-B `lark.REGEXP` / `python.LONG_STRING` delimited-token
/// idioms all lower, so every bundled grammar builds.
fn classify_plan_groups(
    groups: &[(SymbolId, String)],
    by_id: &HashMap<SymbolId, &TerminalDef>,
    prefix: &str,
    global_flags: u32,
) -> Result<ClassifiedSubs, GrammarError> {
    let mut out = ClassifiedSubs {
        plain_subs: Vec::new(),
        plain_srcs: Vec::new(),
        guarded_subs: Vec::new(),
        guarded_srcs: Vec::new(),
        base_inlines: Vec::new(),
        fences: Vec::new(),
    };

    for (rank, (id, inline)) in groups.iter().enumerate() {
        let src = format!("{prefix}{inline}");
        // A pattern the `regex` crate compiles is plain; everything else must
        // lower or refuse with the categorized scope error — there is no runtime
        // fallback engine any more (L4).
        let compile_err = match Regex::new(&src) {
            Ok(_) => {
                out.plain_subs.push(SubPattern {
                    id: *id,
                    rank,
                    branch_order: 0,
                    leading: None,
                    trailing: None,
                    lookbehind: Vec::new(),
                });
                out.plain_srcs.push(src);
                out.base_inlines.push(inline.clone());
                continue;
            }
            Err(e) => e.to_string(),
        };

        let def = by_id[id];

        // Fence-idiom terminals bypass the refusal seam entirely: the named
        // backreference makes the `regex` crate refuse them (correctly — the
        // language is non-regular), but the recognized shape is matched
        // linearly per attempt by the two-phase `FenceMatcher`.
        if let Some(spec) = recognize_fence_idiom_from_def(def) {
            out.fences
                .push(FenceMatcher::build(*id, rank, spec, prefix)?);
            continue;
        }

        // A lookaround (or otherwise regex-rejected) terminal — lower it through
        // THE single refusal seam, or fail the build with the categorized scope
        // error. The seam strips the loader's whole-pattern flag wrapper and
        // returns the merged flag bitset; the [`GuardContext`] re-applies the
        // same scoping to every lowered branch and guard below.
        let (lowered, flags) = route_fancy_only_terminal(def, global_flags, &compile_err)?;
        let ctx = GuardContext { prefix, flags };

        for (bo, br) in lowered.iter().enumerate() {
            let inline_br = wrap_flags(flags, &br.regex);
            let nfa_src = format!("{prefix}{inline_br}");
            out.base_inlines.push(inline_br);
            if br.leading.is_none() && br.trailing.is_none() && br.lookbehind.is_empty() {
                // An unguarded branch (e.g. `lark.OP`'s `[+*]`) is plain — it joins
                // the leftmost-first engine so its priority is exact.
                out.plain_subs.push(SubPattern {
                    id: *id,
                    rank,
                    branch_order: bo,
                    leading: None,
                    trailing: None,
                    lookbehind: Vec::new(),
                });
                out.plain_srcs.push(nfa_src);
            } else {
                let leading = br
                    .leading
                    .as_ref()
                    .map(|g| ctx.compile_guard(g))
                    .transpose()?;
                let trailing = br
                    .trailing
                    .as_ref()
                    .map(|g| ctx.compile_guard(g))
                    .transpose()?;
                let lookbehind = br
                    .lookbehind
                    .iter()
                    .map(|g| ctx.compile_lookbehind(g))
                    .collect::<Result<Vec<_>, _>>()?;
                out.guarded_subs.push(SubPattern {
                    id: *id,
                    rank,
                    branch_order: bo,
                    leading,
                    trailing,
                    lookbehind,
                });
                out.guarded_srcs.push(nfa_src);
            }
        }
    }
    Ok(out)
}

impl PlainEngine {
    /// Stage 2a — the leftmost-first plain engine: order the sub-patterns by `(rank,
    /// branch_order)` so the lowest `PatternID` is the leftmost-first winner.
    /// Borrows `subs` (unlike [`GuardedEngine::build`], which consumes them): this
    /// engine only derives its id/rank `map` from them — the guards a `SubPattern`
    /// carries are all `None`/empty on the plain side, so nothing is stored.
    ///
    /// Over-budget sources (H4-12) are split into a parallel hybrid sub-engine in
    /// `(rank, branch_order)` order, so the leftmost-first re-merge in `match_at` is
    /// exact regardless of which sub-engine a winner lands in.
    fn build(subs: &[SubPattern], srcs: &[String]) -> Result<Option<Self>, GrammarError> {
        if srcs.is_empty() {
            return Ok(None);
        }
        let mut order: Vec<usize> = (0..subs.len()).collect();
        order.sort_by_key(|&i| (subs[i].rank, subs[i].branch_order));
        let ordered_srcs: Vec<&str> = order.iter().map(|&i| srcs[i].as_str()).collect();
        let map: Vec<(SymbolId, usize, usize)> = order
            .iter()
            .map(|&i| (subs[i].id, subs[i].rank, subs[i].branch_order))
            .collect();
        let (dfa, globals, overflow) =
            build_partitioned_dfa(&ordered_srcs, MatchKind::LeftmostFirst)?;
        Ok(Some(PlainEngine {
            dfa,
            globals,
            overflow,
            map,
        }))
    }
}

impl GuardedEngine {
    /// Stage 2b — the all-matches guarded engine. Consumes `subs`: the engine
    /// stores them (guards and all) for `guarded_best` to evaluate per accept.
    /// Over-budget guarded bases (H4-12) split into a parallel hybrid sub-engine;
    /// `guarded_best` reads both accept-sets into the same per-sub accumulator.
    fn build(subs: Vec<SubPattern>, srcs: &[String]) -> Result<Option<Self>, GrammarError> {
        if srcs.is_empty() {
            return Ok(None);
        }
        let srcs: Vec<&str> = srcs.iter().map(String::as_str).collect();
        let (dfa, globals, overflow) = build_partitioned_dfa(&srcs, MatchKind::All)?;
        Ok(Some(GuardedEngine {
            dfa,
            globals,
            overflow,
            subs,
        }))
    }
}

impl DfaScanner {
    /// Build a DFA scanner from candidate terminals (deduplicated by id). Consumes
    /// the same [`ScannerPlan`](super::plan::ScannerPlan) as
    /// [`Scanner::build`](super::scanner::Scanner), so selection / ordering /
    /// `unless` are shared by construction — only the plain engine differs.
    /// Orchestrates the three stages: classify/lower ([`classify_plan_groups`]),
    /// assemble the engines ([`PlainEngine::build`] / [`GuardedEngine::build`]),
    /// derive the prefilter ([`plain_start_bytes`]).
    pub(super) fn build(
        terminals: &[(SymbolId, &TerminalDef)],
        global_flags: u32,
    ) -> Result<DfaScanner, GrammarError> {
        let plan = scanner_plan(terminals, global_flags)?;
        let unless = RetypeTable::build_all_dense(&plan.unless)?;
        let by_id: HashMap<SymbolId, &TerminalDef> =
            terminals.iter().map(|(id, t)| (*id, *t)).collect();

        let classified =
            classify_plan_groups(&plan.groups, &by_id, &plan.global_prefix, global_flags)?;

        let plain = PlainEngine::build(&classified.plain_subs, &classified.plain_srcs)?;
        let guarded = GuardedEngine::build(classified.guarded_subs, &classified.guarded_srcs)?;

        // Start-byte prefilter over the base union of both engines (the lowered bases
        // over-approximate the guarded languages, so it never drops a real start byte).
        let start_bytes = if classified.base_inlines.is_empty() {
            None
        } else {
            let union = format!(
                "{}(?:{})",
                plan.global_prefix,
                classified.base_inlines.join("|")
            );
            plain_start_bytes(&union)
        };

        Ok(DfaScanner {
            plain,
            guarded,
            start_bytes,
            unless,
            fences: classified.fences,
        })
    }

    /// Match a single token starting exactly at `pos` — the same contract as
    /// [`Scanner::match_at`](super::scanner::Scanner::match_at), so the two are
    /// byte-for-byte interchangeable. Consults the leftmost-first **plain** engine and
    /// the all-matches **guarded** engine and keeps the lower `(rank, branch_order)`
    /// candidate.
    pub(super) fn match_at<'t>(&self, text: &'t str, pos: usize) -> Option<(SymbolId, &'t str)> {
        let mut best: Option<(usize, usize, SymbolId, &'t str)> = None;
        let runnable = match &self.start_bytes {
            Some(set) => text.as_bytes().get(pos).is_some_and(|b| set[*b as usize]),
            None => true,
        };
        if runnable && (self.plain.is_some() || self.guarded.is_some()) {
            record_scan_skip(pos, Some(pos));
            // Plain engine: the leftmost-first winner over the unguarded sub-patterns,
            // with its own (order-sensitive) length — never disturbed by a sibling guard.
            if let Some(p) = &self.plain {
                if let Some(cand) = p.match_at(text, pos) {
                    best = Some(cand);
                }
            }
            // Guarded engine: the guarded-accept accumulator's winner.
            if let Some(g) = &self.guarded {
                if let Some(cand) = g.guarded_best(text, pos) {
                    if best.is_none_or(|(r, b, _, _)| (cand.0, cand.1) < (r, b)) {
                        best = Some(cand);
                    }
                }
            }
        }
        // Fence matchers run outside the `runnable` gate: they do their own
        // open-literal pre-check and are not included in `start_bytes`. The
        // tie-break is the same `(rank, branch_order)` rule (a fence terminal
        // is a single branch, so its branch order is 0).
        for fm in &self.fences {
            if let Some(value) = fm.match_at(text, pos) {
                if best.is_none_or(|(r, b, _, _)| (fm.rank, 0usize) < (r, b)) {
                    best = Some((fm.rank, 0, fm.id, value));
                }
            }
        }
        let (_, _, id, value) = best?;
        let ty = self
            .unless
            .get(id.index())
            .and_then(|m| m.as_ref())
            .and_then(|m| m.retype(value))
            .unwrap_or(id);
        Some((ty, value))
    }
}

impl PlainEngine {
    /// The leftmost-first winner over the unguarded sub-patterns at `pos`, as
    /// `(rank, branch_order, terminal id, matched slice)`, or `None`. Consults the
    /// dense sub-engine and (if present) the hybrid overflow sub-engine, re-merging
    /// the two by `(rank, branch_order)` — the global leftmost-first order — so the
    /// H4-12 partition never changes which terminal wins.
    fn match_at<'t>(&self, text: &'t str, pos: usize) -> Option<(usize, usize, SymbolId, &'t str)> {
        let input = Input::new(text)
            .span(pos..text.len())
            .anchored(Anchored::Yes);
        let mut best: Option<(usize, usize, SymbolId, &'t str)> = None;
        let mut consider = |dfa: &CombinedDfa, globals: &[usize]| {
            if let Some((local, end)) = dfa.search_fwd(&input) {
                if end > pos {
                    let (id, rank, bo) = self.map[globals[local]];
                    if best.is_none_or(|(r, b, _, _)| (rank, bo) < (r, b)) {
                        best = Some((rank, bo, id, &text[pos..end]));
                    }
                }
            }
        };
        consider(&self.dfa, &self.globals);
        if let Some((dfa, globals)) = &self.overflow {
            consider(dfa, globals);
        }
        best
    }
}

impl GuardedEngine {
    /// Drive the guarded all-matches DFA(s) over `text` from `pos`: enumerate every
    /// `(sub-pattern, end)` accept via an **overlapping** anchored search, keep per
    /// sub-pattern the **longest accept where its guard holds**, then select Lark's
    /// leftmost-first winner across the survivors by `(rank, branch_order)`. Returns the
    /// winning `(rank, branch_order, terminal id, matched slice)`, or `None`.
    ///
    /// The overlapping search is the `regex-automata`-blessed way to read the full
    /// accept-set out of a `MatchKind::All` DFA (it reports each distinct `(pattern,
    /// end)` once, including multiple ends for one pattern — `[0-9]+` accepting at every
    /// length). It is anchored at `pos` and stops when the DFA dies, so it is linear in
    /// the matched token's length, never forward-scanning. The dense and hybrid (H4-12)
    /// sub-engines feed the **same** per-sub `longest` accumulator, so the split is
    /// transparent to the leftmost-first selection.
    fn guarded_best<'t>(
        &self,
        text: &'t str,
        pos: usize,
    ) -> Option<(usize, usize, SymbolId, &'t str)> {
        let input = Input::new(text)
            .span(pos..text.len())
            .anchored(Anchored::Yes);
        let subs = &self.subs;

        // A leading guard — and every bounded-lookbehind guard — is a precondition on the
        // match start (`pos`), identical for every accept of that sub-pattern, so evaluate
        // it once. `false` = some precondition failed, so the sub-pattern is out entirely.
        let leading_ok: Vec<bool> = subs
            .iter()
            .map(|s| {
                let lead = match &s.leading {
                    None => true,
                    Some(g) => g.holds(text, pos),
                };
                lead && s.lookbehind.iter().all(|lb| lb.holds(text, pos))
            })
            .collect();

        // Longest accept end (exclusive) per sub-pattern where both guards held.
        let mut longest: Vec<Option<usize>> = vec![None; subs.len()];
        let mut accumulate = |dfa: &CombinedDfa, globals: &[usize]| {
            dfa.for_each_overlapping(&input, |local, end| {
                let pid = globals[local];
                if end <= pos {
                    return; // reject a zero-width accept
                }
                if !leading_ok[pid] {
                    return; // leading precondition failed → this sub-pattern can't match
                }
                let trailing_ok = match &subs[pid].trailing {
                    None => true,
                    Some(g) => g.holds(text, end),
                };
                if trailing_ok && longest[pid].is_none_or(|cur| end > cur) {
                    longest[pid] = Some(end);
                }
            });
        };
        accumulate(&self.dfa, &self.globals);
        if let Some((dfa, globals)) = &self.overflow {
            accumulate(dfa, globals);
        }

        // Lark's leftmost-first selection across the survivors: lowest terminal rank,
        // then lowest branch order within a terminal; the winner keeps its own longest
        // guard-held length.
        let mut best: Option<(usize, usize, SymbolId, usize)> = None;
        for (pid, end_opt) in longest.iter().enumerate() {
            let Some(end) = *end_opt else { continue };
            let s = &subs[pid];
            let key = (s.rank, s.branch_order);
            if best.is_none_or(|(r, b, _, _)| key < (r, b)) {
                best = Some((s.rank, s.branch_order, s.id, end));
            }
        }
        best.map(|(rank, bo, id, end)| (rank, bo, id, &text[pos..end]))
    }
}

/// Per-terminal dense determinization budget (`#349`/H4-12). A single source whose
/// eager dense DFA would exceed this is routed to the lazy/hybrid fallback instead of
/// blowing the determinizer to `2^N` states. The bound is **per source**, so a
/// well-behaved union is unaffected — only the pathological `.*a.{N}` family overflows.
///
/// Sizing rationale (ADR-0037): the dense `memory_usage()` of every bundled terminal
/// compiled alone sits at or below ~13 KiB (`python.STRING` 13.2 KiB, `python.LONG_STRING`
/// 12.9 KiB, the rest smaller — measured), so a 64 KiB budget clears the legitimate
/// ceiling by ~5× while sitting well below where the `.*a.{N}` family lands (≈39 KiB at
/// N=8, ≈156 KiB at N=10 — measured) and the dense size doubles per +1 in N. The
/// over-budget terminal still builds and lexes via the hybrid DFA (byte-identical
/// matches), so oracle parity is preserved; only the eager determinization is skipped.
///
/// Shared with [`guard`](super::guard): a guard body (`(?=S)` / `(?<=S)`) is likewise a
/// single dense-determinized source and is bounded by this same budget (issue #568), so
/// the two build sites cannot drift on where "too big to determinize eagerly" sits.
pub(super) const DENSE_PER_SOURCE_BUDGET: usize = 64 * 1024;

/// Compile `srcs` to one Thompson NFA (`build_many`, `PatternID` = index), then
/// determinize one anchored dense DFA under `match_kind`, **with no size limit**. The
/// NFA is match-kind-agnostic — `MatchKind` lives on the determinizer (leftmost-first
/// keeps the NFA's alternation priority; all surfaces every overlapping match). Captures
/// are dropped — the winning sub-pattern is read from `PatternID`.
///
/// Used for inputs known not to blow up determinization (the `fence.rs` tag literal); the
/// engine builders route through [`build_partitioned_dfa`], which bounds each source.
pub(super) fn build_combined_dfa(
    srcs: &[&str],
    match_kind: MatchKind,
) -> Result<dense::DFA<Vec<u32>>, GrammarError> {
    let nfa = build_nfa(srcs)?;
    let dfa = dense_from_nfa(&nfa, match_kind, None).map_err(|e| GrammarError::InvalidRegex {
        pattern: srcs.join("|"),
        reason: e.to_string(),
    })?;
    crate::perf::add_dense_build_bytes(dfa.memory_usage() as u64);
    Ok(dfa)
}

/// Build the combined NFA over `srcs` (captures dropped), the shared front half of every
/// dense/hybrid determinization.
fn build_nfa(srcs: &[&str]) -> Result<thompson::NFA, GrammarError> {
    thompson::NFA::compiler()
        .configure(thompson::Config::new().which_captures(thompson::WhichCaptures::None))
        .build_many(srcs)
        .map_err(|e| GrammarError::InvalidRegex {
            pattern: srcs.join("|"),
            reason: e.to_string(),
        })
}

/// Determinize one anchored dense DFA from `nfa` under `match_kind`, bounded by an
/// optional `dfa_size_limit`. Returns the typed [`dense::BuildError`] so the caller can
/// distinguish a **size-limit overflow** (`is_size_limit_exceeded()` → route this source
/// to the hybrid fallback) from an **unsupported-feature** error (e.g. a Unicode word
/// boundary the dense determinizer rejects), which must surface as a real `GrammarError`
/// rather than being silently rerouted.
fn dense_from_nfa(
    nfa: &thompson::NFA,
    match_kind: MatchKind,
    size_limit: Option<usize>,
) -> Result<dense::DFA<Vec<u32>>, dense::BuildError> {
    dense::Builder::new()
        .configure(
            dense::Config::new()
                .match_kind(match_kind)
                .start_kind(StartKind::Anchored)
                .dfa_size_limit(size_limit)
                .determinize_size_limit(size_limit),
        )
        .build_from_nfa(nfa)
}

/// Stage 2 driver — partition `srcs` (already in engine order) into the sources that fit
/// [`DENSE_PER_SOURCE_BUDGET`] under eager determinization and those that overflow it,
/// then build one **dense** DFA over the former and one **hybrid** (lazy) DFA over the
/// latter (the `#349`/H4-12 fallback). Returns the dense engine + its `local → global`
/// index map, and `Some((hybrid, map))` when any source overflowed (`None` in the common
/// all-fit case, so a normal grammar pays zero hybrid cost).
///
/// Only the **final** engine DFAs are counted into `dense_build_bytes`; the per-source
/// budget probes determinize each source alone but are never counted — the scaling gate
/// reads the engines we actually keep, and an over-budget source contributes ~0 (its
/// states are realized lazily, off the counter).
fn build_partitioned_dfa(
    srcs: &[&str],
    match_kind: MatchKind,
) -> Result<(CombinedDfa, Vec<usize>, Option<(CombinedDfa, Vec<usize>)>), GrammarError> {
    // Probe each source alone: a source whose bounded dense build overflows the budget is
    // routed to the hybrid fallback. Probing per-source (not the whole union) keeps the
    // partition at terminal granularity — one pathological terminal never forces its
    // well-behaved siblings off the eager engine.
    let mut dense_idx: Vec<usize> = Vec::new();
    let mut hybrid_idx: Vec<usize> = Vec::new();
    for (i, &src) in srcs.iter().enumerate() {
        let probe_nfa = build_nfa(&[src])?;
        match dense_from_nfa(&probe_nfa, match_kind, Some(DENSE_PER_SOURCE_BUDGET)) {
            Ok(_) => dense_idx.push(i),
            // Route to the hybrid fallback ONLY for a genuine "too big to determinize
            // eagerly" overflow (the H4-12 case — `is_size_limit_exceeded()` covers both
            // the DFA- and determinize-size limits and the too-many-states overflows). Any
            // *other* dense error (e.g. a Unicode word boundary the dense determinizer does
            // not support) is a real build error, not a budget overflow: surface it as an
            // `InvalidRegex` for this exact source instead of silently rerouting it to a
            // hybrid engine (which would either also fail with a misattributed message or,
            // worse, accept a pattern the eager path rejected — a silent behavior change).
            Err(e) if e.is_size_limit_exceeded() => hybrid_idx.push(i),
            Err(e) => {
                return Err(GrammarError::InvalidRegex {
                    pattern: src.to_string(),
                    reason: e.to_string(),
                })
            }
        }
    }

    // The dense engine over the in-budget sources. Every source fits the budget here, so
    // the union build cannot overflow (each part is bounded; the union of bounded parts
    // can still legitimately grow, so this build carries no size limit — it is the same
    // determinization the engine has always paid for well-behaved terminals).
    let dense = {
        let kept: Vec<&str> = dense_idx.iter().map(|&i| srcs[i]).collect();
        let nfa = build_nfa(&kept)?;
        let dfa =
            dense_from_nfa(&nfa, match_kind, None).map_err(|e| GrammarError::InvalidRegex {
                pattern: kept.join("|"),
                reason: e.to_string(),
            })?;
        crate::perf::add_dense_build_bytes(dfa.memory_usage() as u64);
        CombinedDfa::Dense(dfa)
    };

    let overflow = if hybrid_idx.is_empty() {
        None
    } else {
        let over: Vec<&str> = hybrid_idx.iter().map(|&i| srcs[i]).collect();
        let nfa = build_nfa(&over)?;
        let dfa = LazyDfa::builder()
            .configure(
                LazyDfa::config()
                    .match_kind(match_kind)
                    // Skip the cache-capacity sanity check: a deliberately large NFA
                    // (the .*a.{N} family) has a big worst-case state, but the realized
                    // cache stays small in practice. The lazy DFA bounds memory itself.
                    .skip_cache_capacity_check(true),
            )
            .build_from_nfa(nfa)
            .map_err(|e| GrammarError::InvalidRegex {
                pattern: over.join("|"),
                reason: e.to_string(),
            })?;
        let cache = RefCell::new(dfa.create_cache());
        // The lazy DFA realizes states on demand, so its build-time memory is a small
        // fixed cache — counting it keeps the scaling gate honest (it stays ~flat).
        crate::perf::add_dense_build_bytes(cache.borrow().memory_usage() as u64);
        Some((CombinedDfa::Hybrid { dfa, cache }, hybrid_idx))
    };

    Ok((dense, dense_idx, overflow))
}

/// The set of bytes any branch of the plain union `src` can begin a match with, or
/// `None` if it cannot be computed (so the prefilter is disabled — always run the
/// engine). Built from a **lazy** (hybrid) DFA so only the start state and its 256
/// transitions are realized — no full determinization, hence no blow-up on a large
/// terminal set. A byte is "possible" iff the anchored start state does not go dead
/// on it; non-accepting live transitions are kept too, so the set is an
/// over-approximation (the safe direction: it never drops a real start byte).
fn plain_start_bytes(src: &str) -> Option<Box<[bool; 256]>> {
    let dfa = LazyDfa::new(src).ok()?;
    let mut cache = dfa.create_cache();
    let anchored = Input::new("").anchored(Anchored::Yes);
    let start = dfa.start_state_forward(&mut cache, &anchored).ok()?;
    let mut set = Box::new([false; 256]);
    for b in 0u8..=255 {
        let next = dfa.next_state(&mut cache, start, b).ok()?;
        if !next.is_dead() && !next.is_quit() {
            set[b as usize] = true;
        }
    }
    Some(set)
}

/// **One lookaround terminal, matched alone** — the per-terminal analogue of the
/// combined [`DfaScanner`], for engines that match terminals individually: the Earley
/// dynamic lexer ([`DynamicMatcher`](super::dynamic::DynamicMatcher)) and the
/// `Scanner` reference backend's lowered side-probes. Internally it *is* a
/// single-terminal `DfaScanner` built through the same plan/routing/guard machinery,
/// so its semantics (greedy/lazy match end, guard evaluation, lookbehind windows over
/// the surrounding text) are the combined scanner's by construction — one lowering,
/// not two.
///
/// Build cost: only terminals the `regex` crate rejects pay for it (one small dense
/// DFA each); plain terminals never come near this type.
pub(crate) struct LoweredTerminalMatcher {
    scanner: DfaScanner,
}

impl LoweredTerminalMatcher {
    /// Build the matcher for one terminal, or fail with the same categorized scope
    /// error the combined build produces (`route_fancy_only_terminal` runs inside
    /// `DfaScanner::build`). `compile_err` is the `regex` crate's rejection message,
    /// quoted by the backtracking-only triage.
    pub(super) fn build(
        id: SymbolId,
        def: &TerminalDef,
        global_flags: u32,
        compile_err: &str,
    ) -> Result<Self, GrammarError> {
        // A fence-idiom terminal never routes (DfaScanner::build recognizes it
        // internally); everything else pre-checks the routing so the error
        // carries the engine's own message (DfaScanner::build re-derives the
        // same answer from the plan source, which includes the global prefix).
        if recognize_fence_idiom_from_def(def).is_none() {
            route_fancy_only_terminal(def, global_flags, compile_err)?;
        }
        let scanner = DfaScanner::build(&[(id, def)], global_flags)?;
        Ok(LoweredTerminalMatcher { scanner })
    }

    /// End of the non-empty match starting exactly at `pos` (the full `text` is
    /// passed so a lookbehind guard can see the bytes before `pos`).
    pub(super) fn match_end_at(&self, text: &str, pos: usize) -> Option<usize> {
        let (_, value) = self.scanner.match_at(text, pos)?;
        Some(pos + value.len())
    }

    /// End offset of a non-empty match anchored at the start of `sub`.
    pub(super) fn match_end_in(&self, sub: &str) -> Option<usize> {
        let (_, value) = self.scanner.match_at(sub, 0)?;
        Some(value.len())
    }
}
