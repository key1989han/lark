//! Per-terminal matching for Earley's **dynamic lexer** (Phase 2, Sprint 5).

use std::collections::HashMap;

use regex::Regex;
use regex_automata::{meta::Regex as MetaRegex, Anchored, Input};

use super::dfa::LoweredTerminalMatcher;
use super::plan::global_flag_prefix;
use super::record_scan_skip;
use super::LexerConf;
use crate::error::GrammarError;
use crate::grammar::intern::SymbolId;

/// A matcher for Earley's **dynamic lexer** (Phase 2, Sprint 5).
///
/// Unlike the [`Scanner`](super::scanner::Scanner), which scans one combined
/// alternation left-to-right and hands the parser a fixed token stream, the dynamic
/// lexer matches a *specific* terminal — the one an Earley item predicts — at a
/// given position, integrating scanning into the parse loop. Each terminal
/// therefore gets its own compiled regex, matched with a search *anchored* at the
/// query position ([`Anchored::Yes`] over `pos..len`), so a terminal that does not
/// match exactly at `pos` fails immediately instead of forward-scanning toward its
/// next match further down the input. That anchoring is what keeps the dynamic scan
/// O(n): an unanchored leftmost search (`regex::Regex::find_at`) would, at every
/// position where a *sparse* terminal misses, scan the rest of the input before
/// reporting a far-ahead match it then rejects — Python's `re.Pattern.match(text,
/// pos)` is anchored, so this is the per-terminal analog of the basic/contextual
/// `\G` fix (#104, ported here for #335).
///
/// There is **no `unless` keyword retyping** here: the parser context (which items
/// sit in the scan set) already decides which terminals to try, so `if`-vs-`iffy`
/// is resolved by the grammar, not by a lexer tie-break. Per-terminal flags
/// (`(?i:…)`) and `g_regex_flags` are preserved exactly as the basic lexer does.
pub struct DynamicMatcher {
    res: HashMap<SymbolId, TermRegex>,
    ignore: Vec<SymbolId>,
    names: Vec<String>,
}

/// One terminal's per-terminal matcher for the dynamic lexer: the `regex` crate for
/// the plain common case, the lowered single-terminal DFA for a lookaround terminal.
enum TermRegex {
    Plain(MetaRegex),
    Lowered(LoweredTerminalMatcher),
}

impl TermRegex {
    /// End of the non-empty match starting exactly at `pos`, or `None` — the
    /// contract `AnyRegex::match_end_at` had. The full `text` (not a suffix) is
    /// passed so a lookbehind can see the bytes before `pos`, exactly as the
    /// historical fancy probe could.
    ///
    /// The search is **anchored** at `pos` ([`Anchored::Yes`] over `pos..len`), so a
    /// miss at `pos` returns `None` immediately rather than forward-scanning toward a
    /// distant match — the per-terminal analog of the basic/contextual scanner's `\G`
    /// anchoring (#104). An unanchored `find_at` here made a sparse terminal in the
    /// per-position scan set O(n) per byte ⇒ O(n²) total (#335); Python's
    /// `re.Pattern.match(text, pos)` is anchored and stays O(n).
    fn match_end_at(&self, text: &str, pos: usize) -> Option<usize> {
        match self {
            TermRegex::Plain(re) => {
                let input = Input::new(text)
                    .span(pos..text.len())
                    .anchored(Anchored::Yes);
                let m = re.find(input);
                // Anchored: a hit (if any) starts exactly at `pos`, so the recorded
                // skip is 0 (flat per attempt) and a miss is charged a flat 1 —
                // never the forward-scan distance the unanchored search reported.
                record_scan_skip(pos, m.map(|_| pos));
                let m = m?;
                (m.end() > pos).then_some(m.end())
            }
            TermRegex::Lowered(m) => m.match_end_at(text, pos),
        }
    }

    /// End offset of a non-empty match anchored at the start of `sub`, or `None` —
    /// the contract `AnyRegex::match_end_in` had (used by the `dynamic_complete`
    /// scan, which re-matches against a truncated haystack).
    fn match_end_in(&self, sub: &str) -> Option<usize> {
        match self {
            TermRegex::Plain(re) => {
                let input = Input::new(sub).span(0..sub.len()).anchored(Anchored::Yes);
                let m = re.find(input)?;
                (m.end() > 0).then_some(m.end())
            }
            TermRegex::Lowered(m) => m.match_end_in(sub),
        }
    }
}

impl DynamicMatcher {
    /// Build a matcher from the same [`LexerConf`] the basic lexer uses, so both
    /// engines honour identical terminal patterns and global flags. A lookaround
    /// terminal lowers to its own single-terminal DFA ([`LoweredTerminalMatcher`]);
    /// a terminal the lowering refuses fails the build with the **same categorized
    /// scope error** the basic-lexer path produces (`docs/LOOKAROUND_SCOPE.md`), so
    /// the dynamic lexer accepts exactly the same grammars.
    pub fn new(conf: &LexerConf) -> Result<Self, GrammarError> {
        let prefix = global_flag_prefix(conf.global_flags);
        let mut res = HashMap::new();
        for (id, term) in &conf.terminals {
            let pat = term.pattern.to_inline_regex();
            // Reject zero-width terminals on the dynamic path. Python Lark's
            // `EarleyRegexpMatcher.__init__` rejects any terminal whose regexp can
            // derive the empty string (`get_regexp_width(t.pattern.to_regexp())[0] ==
            // 0`) with a *dynamic-lexer-specific* error (`parser_frontends.py:205`) —
            // distinct from the basic lexer's "Lexer does not allow zero-width
            // terminals". A nullable terminal would let the dynamic scan make no
            // progress at a position, so it is forbidden at construction time.
            //
            // We use the assertion-aware min-width oracle
            // (`lookaround::pattern_min_width_is_zero`, `width_range(...).0 == 0`),
            // the lark-rs equivalent of Python's `get_regexp_width(...)[0]`. It is the
            // gate for **every** terminal, including the lookaround terminals that
            // take the `Lowered` branch below: a `Regex::new(..).is_match("")` probe
            // can't see an assertion the `regex` crate refuses to compile (`/a*(?=b)/`,
            // min-width 0, would slip through to `Lowered` ungated), and it disagrees
            // with Python on a bare word boundary (`/\b/` is min-width 0 in Python but
            // `is_match("")` is false). The oracle matches Python's `min_width == 0`
            // rule exactly — it rejects on the *minimum*, so a pattern that can derive
            // empty (`/a?/`, `/x*y*/`, `/a*(?=b)/`, `/\b/`) is rejected even when it can
            // also match a non-empty string, while a non-nullable terminal (`/a+/`,
            // `/foo(?!bar)/`) still builds. A pattern the front-end can't parse falls
            // back to the `is_match("")` probe rather than over-rejecting.
            let zero_width = match crate::lookaround::pattern_min_width_is_zero(&pat) {
                Some(z) => z,
                None => Regex::new(&format!("{prefix}{pat}"))
                    .map(|re| re.is_match(""))
                    .unwrap_or(false),
            };
            if zero_width {
                return Err(GrammarError::Other {
                    msg: "Dynamic Earley doesn't allow zero-width regexps".to_string(),
                });
            }
            let src = format!("{prefix}{pat}");
            // `regex::Regex::new` is the routing oracle (same seam as `scanner.rs`):
            // a pattern the linear `regex` crate accepts is a *plain* terminal, one it
            // rejects (lookaround/backref) lowers into its own single-terminal DFA.
            // The plain terminal is then matched through `regex_automata`'s meta engine
            // so the per-position search can be **anchored** at `pos` (`Anchored::Yes`)
            // — `regex::Regex` has no anchored-at-position search, only the unanchored
            // `find_at` that caused the #335 O(n²) forward-scan. The meta engine is the
            // same one `regex::Regex` wraps and parses identical syntax, so a pattern
            // the probe accepted compiles here too.
            let compiled = match Regex::new(&src) {
                Ok(_) => TermRegex::Plain(MetaRegex::new(&src).map_err(|e| {
                    GrammarError::InvalidRegex {
                        pattern: src.clone(),
                        reason: e.to_string(),
                    }
                })?),
                Err(e) => TermRegex::Lowered(LoweredTerminalMatcher::build(
                    *id,
                    term,
                    conf.global_flags,
                    &e.to_string(),
                )?),
            };
            res.insert(*id, compiled);
        }
        Ok(DynamicMatcher {
            res,
            ignore: conf.ignore.clone(),
            names: conf.names(),
        })
    }

    /// Match terminal `id` starting exactly at byte `pos` in `text`. Returns the
    /// matched slice, or `None` if the terminal does not match there (or matches
    /// empty — a nullable terminal can never advance the scan).
    pub fn match_at<'t>(&self, id: SymbolId, text: &'t str, pos: usize) -> Option<&'t str> {
        let end = self.res.get(&id)?.match_end_at(text, pos)?;
        Some(&text[pos..end])
    }

    /// Match terminal `id` against the whole sub-slice `sub` (anchored at its
    /// start). Used by `dynamic_complete` to explore shorter tokenizations, which
    /// Python Lark does by re-matching against a truncated string `s[:-j]`.
    pub fn match_in<'t>(&self, id: SymbolId, sub: &'t str) -> Option<&'t str> {
        let end = self.res.get(&id)?.match_end_in(sub)?;
        Some(&sub[..end])
    }

    /// The `%ignore` terminal ids, tried between tokens by the dynamic scanner.
    pub fn ignore(&self) -> &[SymbolId] {
        &self.ignore
    }

    /// Display name of a terminal id (for the token's `type_`).
    pub fn name(&self, id: SymbolId) -> &str {
        self.names.get(id.index()).map(String::as_str).unwrap_or("")
    }
}
