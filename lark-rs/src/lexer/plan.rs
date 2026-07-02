//! The deterministic scanner recipe: terminal selection, Python-style ordering,
//! and `unless` keyword retyping — shared by both runtime engines and baked
//! verbatim by the standalone generator, so all three agree by construction.

use std::collections::{HashMap, HashSet};

use regex::Regex;

use super::guard::{Guard, GuardContext, LookbehindGuardC};
use super::pattern::wrap_flags;
use super::route::route_fancy_only_terminal;
use crate::error::GrammarError;
use crate::grammar::intern::SymbolId;
use crate::grammar::terminal::{Pattern, TerminalDef};

/// The deterministic recipe for a combined scanner: the global-flag prefix, the
/// alternation members in order (each terminal id paired with its inline regex
/// source), and the `unless` keyword-retype map.
///
/// [`Scanner::build`](super::scanner::Scanner) consumes this to compile a runtime
/// scanner; the standalone parser generator (`crate::standalone`) bakes the very
/// same plan into `const` data, so a generated parser's lexer is byte-identical
/// to the in-process one.
#[derive(Debug, Clone)]
pub struct ScannerPlan {
    /// Leading inline-flag group for `g_regex_flags` (e.g. `(?i)`), or empty.
    pub global_prefix: String,
    /// `(terminal id, inline regex source)`, in alternation order.
    pub groups: Vec<(SymbolId, String)>,
    /// regex-terminal-id → its `unless` keyword candidates, in definition order.
    pub unless: HashMap<SymbolId, Vec<UnlessEntry>>,
}

/// One `unless` keyword candidate of a regex terminal: a string terminal the
/// regex fully matches, retyped after the fact (Python's `UnlessCallback`).
#[derive(Debug, Clone)]
pub struct UnlessEntry {
    /// The keyword's literal value, case-exact as written in the grammar.
    pub value: String,
    /// Case-insensitive retype: a `"..."i` keyword (or any keyword under a
    /// global `IGNORECASE`), matched like Python's flag-carrying unless scanner.
    pub ci: bool,
    /// The keyword terminal to retype to.
    pub keyword: SymbolId,
}

/// Compiled retype table for one regex terminal's `unless` keywords: exact
/// values in a hash map (the hot path — e.g. every `NAME` token probes it),
/// case-insensitive keywords as anchored `(?i:…)` regexes, matched in
/// definition order — the same semantics the keyword's own scanner pattern
/// would have. When both could apply, the exact match wins. (Python's
/// `UnlessCallback` is pure definition-order first-match; this diverges only
/// when one regex terminal `unless`-matches both `"kw"i` and a later `"kw"` —
/// on exact-cased input Python retypes to the `i` keyword, this table to the
/// exact one. The hash map is what keeps the per-token probe O(1).)
#[derive(Debug)]
pub(super) struct RetypeTable {
    exact: HashMap<String, SymbolId>,
    ci: Vec<(Regex, SymbolId)>,
}

impl RetypeTable {
    fn build(entries: &[UnlessEntry]) -> Result<Self, GrammarError> {
        let mut exact = HashMap::new();
        let mut ci = Vec::new();
        for e in entries {
            if e.ci {
                let src = format!("^(?i:{})$", regex::escape(&e.value));
                let re = Regex::new(&src).map_err(|err| GrammarError::InvalidRegex {
                    pattern: src.clone(),
                    reason: err.to_string(),
                })?;
                ci.push((re, e.keyword));
            } else {
                exact.entry(e.value.clone()).or_insert(e.keyword);
            }
        }
        Ok(RetypeTable { exact, ci })
    }

    pub(super) fn retype(&self, text: &str) -> Option<SymbolId> {
        if let Some(&k) = self.exact.get(text) {
            return Some(k);
        }
        self.ci
            .iter()
            .find(|(re, _)| re.is_match(text))
            .map(|(_, k)| *k)
    }

    /// Build the per-regex-terminal retype tables from a plan's unless map.
    pub(super) fn build_all(
        plan: &HashMap<SymbolId, Vec<UnlessEntry>>,
    ) -> Result<HashMap<SymbolId, RetypeTable>, GrammarError> {
        plan.iter()
            .map(|(id, entries)| Ok((*id, RetypeTable::build(entries)?)))
            .collect()
    }

    /// Dense (`SymbolId::index()`-indexed) variant of [`build_all`]: the winning
    /// terminal's per-token `unless` lookup becomes an array read, not a SipHash
    /// map probe (perf spike 2026-07-01). Empty when the plan has no `unless`
    /// entries, so grammars without keyword retyping pay one bounds check.
    pub(super) fn build_all_dense(
        plan: &HashMap<SymbolId, Vec<UnlessEntry>>,
    ) -> Result<Vec<Option<RetypeTable>>, GrammarError> {
        let mut dense: Vec<Option<RetypeTable>> = Vec::new();
        for (id, entries) in plan {
            if dense.len() <= id.index() {
                dense.resize_with(id.index() + 1, || None);
            }
            dense[id.index()] = Some(RetypeTable::build(entries)?);
        }
        Ok(dense)
    }
}

/// Compute the [`ScannerPlan`] for a candidate terminal set, applying exactly the
/// selection, ordering and `unless`-embedding rules `Scanner::build` relies on.
/// Factored out so the runtime lexer and the standalone code generator agree by
/// construction.
pub fn scanner_plan(
    terminals: &[(SymbolId, &TerminalDef)],
    global_flags: u32,
) -> Result<ScannerPlan, GrammarError> {
    let mut seen = HashSet::new();
    let terms: Vec<(SymbolId, &TerminalDef)> = terminals
        .iter()
        .copied()
        .filter(|(id, _)| seen.insert(*id))
        .collect();

    // unless: embed string terminals fully matched by a same-priority regex
    // terminal, and record the retype.
    let (unless, embedded) = compute_unless(&terms, global_flags)?;

    // Scanner terminals = everything not embedded, sorted Python-style.
    let mut scan: Vec<(SymbolId, &TerminalDef)> = terms
        .iter()
        .copied()
        .filter(|(id, _)| !embedded.contains(id))
        .collect();
    sort_terminals(&mut scan);

    let groups = scan
        .iter()
        .map(|(id, term)| (*id, term.pattern.to_inline_regex()))
        .collect();

    Ok(ScannerPlan {
        global_prefix: global_flag_prefix(global_flags),
        groups,
        unless,
    })
}

/// Verify a [`ScannerPlan`] is hostable by the **pure-`regex` standalone runtime**
/// (issue #280, bounty RC10 + V1/V2). The in-process lexer routes a regex-rejected
/// terminal through the DFA-lowering refusal seam, so a lowered-lookaround terminal
/// (e.g. `python.STRING`) builds fine on the `regex-automata` backend; the standalone
/// runtime, by contrast, compiles each baked group on the plain `regex` crate
/// (`Scanner::new` → `Regex::new(...).expect("baked scanner regex is valid")`). A
/// terminal the plain crate cannot host — lookaround (RC10), an unsupported anchor
/// like `\Z` (V1), or an oversized bounded repeat past the crate's size limit (V2) —
/// would therefore be baked verbatim and **panic** the generated parser at first use.
///
/// This is the standalone backend's analogue of the engine-build refusal seam: it
/// compiles **exactly what the runtime compiles** (each group's inline source, then
/// the combined alternation under the same global prefix), so it rejects exactly the
/// terminals the runtime would panic on. A per-terminal failure is first routed
/// through [`route_fancy_only_terminal`] for the categorized lookaround/backtracking
/// message; anything the seam does not classify (e.g. the size limit) falls back to a
/// clear "not standalone-able" error. Either way the contract holds: a grammar the
/// pure-`regex` runtime cannot host is refused at generation time, never baked into a
/// panicking artifact.
///
/// `terminals` is the same id→def map `scanner_plan` was built from, used to recover a
/// failing group's [`TerminalDef`] for the refusal seam and the error message.
pub fn check_standalone_regex_hostable(
    plan: &ScannerPlan,
    terminals: &[(SymbolId, &TerminalDef)],
    global_flags: u32,
) -> Result<(), GrammarError> {
    let def_of = |id: SymbolId| {
        terminals
            .iter()
            .find(|(tid, _)| *tid == id)
            .map(|(_, t)| *t)
    };

    // Per-group: compile each baked inline source on the plain `regex` crate, under the
    // global prefix — the same `{prefix}{inline}` probe `DfaScanner`'s build uses to
    // decide whether a terminal needs the refusal seam, so attribution agrees by
    // construction. A failure here is the precise terminal a generated parser would
    // panic on. (The runtime wraps each group in a named capture before combining; the
    // capture cannot change *whether* the source compiles, only the combined size —
    // which the aggregate pass below checks separately — so the bare source is the
    // faithful per-terminal probe, and keeps a routed error message free of the
    // synthetic `(?P<g…>)` wrapper.)
    for (id, rx) in &plan.groups {
        let probe = format!("{}{}", plan.global_prefix, rx);
        let Err(err) = Regex::new(&probe) else {
            continue;
        };
        // Every group id comes from `terminals`, so the def is always present.
        let def = def_of(*id).expect("scanner-plan group id must have a TerminalDef");
        // A regex-rejected terminal: route through THE refusal seam so the standalone
        // backend reports the *same* categorized lookaround/backtracking error the
        // engine paths do (parity by construction). The seam either returns the
        // categorized error (lookaround/backtracking), or — for a terminal it lowers
        // successfully (the DFA can host it but the plain runtime cannot, e.g.
        // `python.STRING`) — returns `Ok`, in which case we synthesize the
        // not-standalone-able refusal ourselves.
        match route_fancy_only_terminal(def, global_flags, &err.to_string()) {
            Err(scope_err) => return Err(scope_err),
            Ok(_) => {
                return Err(GrammarError::Other {
                    msg: format!(
                        "standalone generation cannot host terminal {:?}: its pattern \
                         uses lookaround the in-process DFA lexer lowers but the \
                         pure-`regex` standalone runtime cannot compile (the regex \
                         engine said: {err}). Such a grammar is not standalone-able.",
                        def.name
                    ),
                });
            }
        }
    }

    // Combined alternation: the per-group pass above probes each terminal in isolation,
    // but the runtime ultimately compiles the *joined* scanner. This pass mirrors
    // `Scanner::new`'s `Regex::new(&pattern)` exactly so any failure that only the
    // assembled alternation exhibits — e.g. tripping the `regex` crate's compiled-size
    // limit once all the (already-oversized, e.g. V2 `[a-z]{200000}`, or merely large)
    // groups are summed — is still caught at bake time rather than panicking the
    // generated parser. The per-group pass catches the common single-terminal case (and
    // categorizes it via the seam); this is the aggregate backstop.
    let parts: Vec<String> = plan
        .groups
        .iter()
        .map(|(id, rx)| format!("(?P<g{}>{})", id.0, rx))
        .collect();
    if !parts.is_empty() {
        let combined = format!("{}{}", plan.global_prefix, parts.join("|"));
        if let Err(err) = Regex::new(&combined) {
            return Err(GrammarError::Other {
                msg: format!(
                    "standalone generation cannot host this grammar's combined scanner: \
                     the pure-`regex` standalone runtime fails to compile it (the regex \
                     engine said: {err}). Such a grammar is not standalone-able."
                ),
            });
        }
    }

    Ok(())
}

/// For each regex terminal, find the same-priority string terminals it fully
/// matches; those become retype candidates, applied after the fact. Mirrors
/// Python Lark's `_create_unless`, including its two case-insensitivity rules:
///
///   * a keyword is **embedded** (dropped from the alternation, the regex
///     terminal matches in its stead) only when its flags are a subset of the
///     regex terminal's (`strtok.pattern.flags <= retok.pattern.flags`) — a
///     `"kw"i` under a case-sensitive regex stays in the alternation, since
///     the regex cannot match every casing the keyword accepts;
///   * the retype test itself honours the keyword's own flags (Python builds
///     the `UnlessCallback` scanner from the keywords' patterns), so a `NAME`
///     match retypes to a `"kw"i` keyword on *any* casing.
///
/// Also returns the embedded-keyword id set for the alternation filter.
#[allow(clippy::type_complexity)]
fn compute_unless(
    terms: &[(SymbolId, &TerminalDef)],
    global_flags: u32,
) -> Result<(HashMap<SymbolId, Vec<UnlessEntry>>, HashSet<SymbolId>), GrammarError> {
    let res: Vec<&(SymbolId, &TerminalDef)> = terms
        .iter()
        .filter(|(_, t)| matches!(t.pattern, Pattern::Re(_)))
        .collect();
    let strs: Vec<&(SymbolId, &TerminalDef)> = terms
        .iter()
        .filter(|(_, t)| matches!(t.pattern, Pattern::Str(_)))
        .collect();
    if res.is_empty() || strs.is_empty() {
        return Ok((HashMap::new(), HashSet::new()));
    }

    // The whole-string ("full match") membership test for one regex terminal: the
    // anchored `regex` crate for the plain common case; a lookaround terminal is
    // routed through THE refusal seam and full-matched via its lowered branches
    // (each `^(?:branch)$` is lookaround-free, so `is_match` under the anchors is
    // pure language membership — greedy/lazy is irrelevant — plus the branch's
    // guards evaluated within the keyword value: leading at 0, trailing at the end
    // [EOI semantics, matching the assertion's view under `^…$`], lookbehinds at
    // their fixed offsets). A terminal the seam REFUSES is skipped silently here:
    // the engine build that follows reports the one canonical categorized error
    // (`docs/LOOKAROUND_SCOPE.md`), so no duplicate/diverging message is produced.
    enum FullMatcher {
        Plain(Regex),
        Lowered(Vec<(Regex, Option<Guard>, Option<Guard>, Vec<LookbehindGuardC>)>),
        Refused,
    }
    impl FullMatcher {
        fn is_full(&self, value: &str) -> bool {
            match self {
                FullMatcher::Plain(re) => re.is_match(value),
                FullMatcher::Lowered(branches) => {
                    branches.iter().any(|(re, leading, trailing, behinds)| {
                        re.is_match(value)
                            && leading.as_ref().is_none_or(|g| g.holds(value, 0))
                            && trailing
                                .as_ref()
                                .is_none_or(|g| g.holds(value, value.len()))
                            && behinds.iter().all(|g| g.holds(value, 0))
                    })
                }
                FullMatcher::Refused => false,
            }
        }
    }

    let prefix = global_flag_prefix(global_flags);
    let global_ci = global_flags & crate::grammar::terminal::flags::IGNORECASE != 0;
    let mut unless: HashMap<SymbolId, Vec<UnlessEntry>> = HashMap::new();
    let mut embedded: HashSet<SymbolId> = HashSet::new();
    for (re_id, re_t) in &res {
        let full_src = format!("{}^(?:{})$", prefix, re_t.pattern.to_inline_regex());
        let full = match Regex::new(&full_src) {
            Ok(re) => FullMatcher::Plain(re),
            Err(e) => match route_fancy_only_terminal(re_t, global_flags, &e.to_string()) {
                Ok((branches, flags)) => {
                    // The same guard-compilation context the combined DfaScanner
                    // build threads — one compilation path, no drift.
                    let ctx = GuardContext {
                        prefix: &prefix,
                        flags,
                    };
                    let mut compiled = Vec::new();
                    for br in &branches {
                        let re_src = format!("{prefix}^(?:{})$", wrap_flags(flags, &br.regex));
                        let re = Regex::new(&re_src).map_err(|e| GrammarError::InvalidRegex {
                            pattern: re_src.clone(),
                            reason: e.to_string(),
                        })?;
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
                        let behinds = br
                            .lookbehind
                            .iter()
                            .map(|g| ctx.compile_lookbehind(g))
                            .collect::<Result<Vec<_>, _>>()?;
                        compiled.push((re, leading, trailing, behinds));
                    }
                    FullMatcher::Lowered(compiled)
                }
                Err(_) => FullMatcher::Refused,
            },
        };
        for (s_id, s_t) in &strs {
            if s_t.priority != re_t.priority {
                continue;
            }
            let pat = match &s_t.pattern {
                Pattern::Str(p) => p,
                Pattern::Re(_) => continue,
            };
            // Membership is tested on the case-exact value (Python matches
            // `strtok.pattern.value` against the regex without the keyword's
            // own flags).
            if full.is_full(&pat.value) {
                unless.entry(*re_id).or_default().push(UnlessEntry {
                    value: pat.value.clone(),
                    // Under a global IGNORECASE every keyword retypes
                    // case-insensitively (Python passes `g_regex_flags` into
                    // the unless scanner).
                    ci: pat.ci || global_ci,
                    keyword: *s_id,
                });
                // Python: `if strtok.pattern.flags <= retok.pattern.flags:
                // embedded_strs.add(strtok)`. A case-sensitive keyword has no
                // flags, so it always embeds; a `"kw"i` embeds only under an
                // IGNORECASE regex terminal. (A named regex terminal's
                // grammar-level flags are baked into its pattern source, so
                // its `flags` field reads 0 here — the conservative outcome
                // is keeping the keyword in the alternation, which is
                // behaviour-preserving: the keyword's own `(?i:…)` group
                // simply competes alongside the regex, and the retype map
                // covers the case where the regex wins.)
                let re_flags = match &re_t.pattern {
                    Pattern::Re(r) => r.flags,
                    Pattern::Str(_) => 0,
                };
                let re_ci =
                    re_flags & crate::grammar::terminal::flags::IGNORECASE != 0 || global_ci;
                if !pat.ci || re_ci {
                    embedded.insert(*s_id);
                }
            }
        }
    }
    Ok((unless, embedded))
}

/// The leading inline-flag group (`(?i)`, `(?im)`, …) for Lark's `g_regex_flags`,
/// or an empty string when no global flags are set. Placed at the very start of a
/// pattern it applies to the entire combined regex (every alternation branch),
/// mirroring `re.compile(pattern, flags=g_regex_flags)`.
pub(super) fn global_flag_prefix(global_flags: u32) -> String {
    let letters = crate::grammar::terminal::flag_letters(global_flags);
    if letters.is_empty() {
        String::new()
    } else {
        format!("(?{letters})")
    }
}

/// Python Lark's terminal ordering: `(-priority, -max_width, -len(value), name)`
/// (`lark/lexer.py:583`). An *unbounded* regex (`max_width = ∞`, mapped here from
/// `None → usize::MAX`) sorts ahead of any finite-width terminal, so the
/// leftmost-first alternation matches it greedily; a *finite* regex sorts by its
/// real character width, not as unbounded (#268, RC5). The third key is the raw
/// pattern length — Python's `len(pattern.value)`, the source with flags stored
/// separately — so a flagged terminal's baked `(?i:…)` wrapper does not leak a
/// phantom rank boost into the tiebreak (#268, N2). `as_regex_str().len()` would
/// reintroduce both bugs (byte length of the *wrapped* source), so use
/// [`Pattern::raw_value_len`].
fn sort_terminals(terms: &mut [(SymbolId, &TerminalDef)]) {
    terms.sort_by(|(a_id, a), (b_id, b)| {
        let aw = a.pattern.max_width().unwrap_or(usize::MAX);
        let bw = b.pattern.max_width().unwrap_or(usize::MAX);
        b.priority
            .cmp(&a.priority)
            .then_with(|| bw.cmp(&aw))
            .then_with(|| b.pattern.raw_value_len().cmp(&a.pattern.raw_value_len()))
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a_id.cmp(b_id))
    });
}
