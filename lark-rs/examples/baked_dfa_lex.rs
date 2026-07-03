//! **Spike experiment 1 (throwaway):** a directly-executable / *baked* JSON scanner
//! vs the default `regex-automata` DFA lexer — the top unproven lever from
//! `docs/notes/parser-optimization-research-2026-07.md` (finding B1: RE/flex
//! `--full`/`--fast` bake the FSM as a static opcode table / goto-switch code, so
//! "FSM construction overhead is eliminated … starts scanning immediately").
//!
//! ## The hypothesis under test
//!
//! lark-rs's lexer is a *table-interpreted* DFA (`regex-automata`): every byte drives
//! a `dfa.next_state(state, byte)` table lookup. B1 says a *baked* scanner — the FSM
//! compiled directly into branch/loop code (a `match` on the lead byte + inline
//! consumption loops, exactly flex `--fast`'s goto/switch) — should be faster because
//! it eliminates the transition-table indirection and stays in registers.
//!
//! **This is a hypothesis to measure, not a settled win** (the research is explicit:
//! RE/flex's evidence shows the *technique* works, not that lark-rs's DFA will lose).
//! A quick-and-dirty hand-baked scanner for ONE grammar (JSON) is enough for a
//! directional number — we do not need a general emitter.
//!
//! ## What is isolated, and the two gotchas the research flagged
//!
//! * **Scan-only** (the headline): both sides advance a cursor token-by-token,
//!   producing only `(kind, end)` — *no* `Token` materialization, *no* owned value
//!   `String`, *no* line/col bookkeeping. This isolates the pure *scanner engine*
//!   (`DfaScanner::match_at`'s table walk vs the baked `match`), which is exactly
//!   where B1 lives. Both are driven over the identical token boundaries (asserted
//!   equal), so the delta is a clean scanner isolation.
//! * **Full `lex()`** (end-to-end context): `BasicLexer::lex` (DFA) vs a baked full
//!   lexer that materializes the *same* `Token`s (owned value + positions). The
//!   scanner is only ~part of `lex()`; token materialization (the ~55%-of-parse lexer
//!   budget is mostly `memcpy`+`malloc` for token values, per the 2026-06-04 profile)
//!   is shared, so this shows whether a faster scanner moves the end-to-end needle.
//! * **Reused (built) vs one-shot (build+lex)** — the DFA pays a determinization
//!   *build* cost the baked scanner does not (it is compiled code). Reported as two
//!   columns: baking only helps the one-shot column.
//!
//! Recorded wall-clock trend (ADR-0007) — only same-session back-to-back ratios
//! travel. Run:
//!
//! ```text
//! cargo run --release --example baked_dfa_lex
//! ```

use std::hint::black_box;
use std::time::{Duration, Instant};

use lark_rs::{basic_lexer_conf, load_grammar, lower, BasicLexer, Lexer, LexerBackend};

const JSON_GRAMMAR: &str = r#"
    ?start: value
    ?value: object
          | array
          | string
          | SIGNED_NUMBER  -> number
          | "true"         -> true
          | "false"        -> false
          | "null"         -> null
    array  : "[" [value ("," value)*] "]"
    object : "{" [pair ("," pair)*] "}"
    pair   : string ":" value
    string : ESCAPED_STRING
    %import common.ESCAPED_STRING
    %import common.SIGNED_NUMBER
    %import common.WS
    %ignore WS
"#;

/// A JSON array of `records` flat objects — the same shape `parse.rs`/`lex_backends.rs`
/// generate, so the numbers line up with the existing lexer bench. All-ASCII, so a
/// byte offset equals a character offset (the `Token` position parity, #278).
fn gen_json(records: usize, fields: usize) -> String {
    let mut s = String::from("[");
    for r in 0..records {
        if r > 0 {
            s.push(',');
        }
        s.push('{');
        for f in 0..fields {
            if f > 0 {
                s.push(',');
            }
            s.push_str(&format!(
                "\"key{f}\": {}, \"name{f}\": \"value{r}_{f}\"",
                r * 10 + f
            ));
        }
        s.push('}');
    }
    s.push(']');
    s
}

// ─── The baked, directly-executable JSON scanner ────────────────────────────────
//
// This is the whole experiment: a hand-written FSM as branch/loop code. `match_at`
// is one `match` on the lead byte (the "which pattern starts here" decision the DFA
// spends a start-state table lookup on) followed by an inline consumption loop per
// token class — no transition table, no `PatternID`, everything in registers. It
// reproduces the JSON grammar's token boundaries byte-for-byte (asserted below).

/// Token classes the JSON grammar produces (we only need the *boundary*, but a kind
/// keeps the correctness check meaningful and mirrors the DFA returning a `PatternID`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Punct,  // { } [ ] : ,
    String, // ESCAPED_STRING
    Number, // SIGNED_NUMBER
    True,
    False,
    Null,
    Ws, // WS (ignored by the parser, but a scanned token here)
}

/// Match one token starting exactly at `pos`, returning `(kind, end)` or `None`.
/// The baked analogue of `DfaScanner::match_at` — a switch on the lead byte, then an
/// inline loop. `#[inline]` so the loop body sees it as straight-line code.
#[inline]
fn baked_match_at(b: &[u8], pos: usize) -> Option<(Kind, usize)> {
    let c = *b.get(pos)?;
    match c {
        b'{' | b'}' | b'[' | b']' | b':' | b',' => Some((Kind::Punct, pos + 1)),
        b'"' => {
            // ESCAPED_STRING: '"' ( [^"\\\n] | \\. )* '"'
            let mut i = pos + 1;
            loop {
                match *b.get(i)? {
                    b'"' => return Some((Kind::String, i + 1)),
                    b'\n' => return None, // unterminated: a raw newline ends the class
                    b'\\' => {
                        // \\. — the backslash escapes the next byte (any byte).
                        b.get(i + 1)?;
                        i += 2;
                    }
                    _ => i += 1,
                }
            }
        }
        b'-' | b'+' | b'0'..=b'9' | b'.' => {
            // SIGNED_NUMBER: [+-]? ( digits ('.' digits?)? ([eE][+-]?digits)? | '.' digits ... )
            // Faithful greedy scan of the number grammar (FLOAT | INT under an optional sign).
            let mut i = pos;
            if b[i] == b'+' || b[i] == b'-' {
                i += 1;
            }
            let int_start = i;
            while matches!(b.get(i), Some(d) if d.is_ascii_digit()) {
                i += 1;
            }
            let had_int = i > int_start;
            // Fractional part: DECIMAL is `INT "." INT?` or `"." INT`.
            let mut had_frac = false;
            if matches!(b.get(i), Some(b'.')) {
                let dot = i;
                i += 1;
                let frac_start = i;
                while matches!(b.get(i), Some(d) if d.is_ascii_digit()) {
                    i += 1;
                }
                // `.` with no integer part before it needs digits after (`.5`); with an
                // integer part, `INT "." INT?` allows a bare trailing dot (`5.`).
                if !had_int && i == frac_start {
                    i = dot; // neither `INT.` nor `.INT` — back out the dot
                } else {
                    had_frac = true;
                }
            }
            if !had_int && !had_frac {
                return None; // a lone sign or lone dot is not a number
            }
            // Exponent: FLOAT allows `INT _EXP` / `DECIMAL _EXP?`; _EXP = [eE] [+-]? INT.
            if matches!(b.get(i), Some(b'e') | Some(b'E')) {
                let e = i;
                let mut j = i + 1;
                if matches!(b.get(j), Some(b'+') | Some(b'-')) {
                    j += 1;
                }
                let exp_start = j;
                while matches!(b.get(j), Some(d) if d.is_ascii_digit()) {
                    j += 1;
                }
                i = if j > exp_start { j } else { e }; // no exponent digits → back out
            }
            Some((Kind::Number, i))
        }
        b't' if b[pos..].starts_with(b"true") => Some((Kind::True, pos + 4)),
        b'f' if b[pos..].starts_with(b"false") => Some((Kind::False, pos + 5)),
        b'n' if b[pos..].starts_with(b"null") => Some((Kind::Null, pos + 4)),
        b' ' | b'\t' | b'\x0c' | b'\r' | b'\n' => {
            // WS: [ \t\f\r\n]+ — one maximal run is one token (matches the DFA).
            let mut i = pos + 1;
            while matches!(b.get(i), Some(b' ' | b'\t' | b'\x0c' | b'\r' | b'\n')) {
                i += 1;
            }
            Some((Kind::Ws, i))
        }
        _ => None,
    }
}

/// Scan-only baked loop: advance token-by-token, returning the token count. No
/// `Token`, no owned value, no line/col — the pure scanner-engine isolation.
fn baked_scan_only(text: &str) -> usize {
    let b = text.as_bytes();
    let mut pos = 0usize;
    let mut n = 0usize;
    while pos < b.len() {
        match baked_match_at(b, pos) {
            Some((_, end)) => {
                debug_assert!(end > pos);
                pos = end;
                n += 1;
            }
            None => pos += 1, // never happens on valid JSON; matches the DFA loop's guard
        }
    }
    n
}

/// Scan-only DFA loop over the *same* work as `baked_scan_only`: `match_at` per
/// position, advancing by the matched slice, counting every token (WS included). This
/// is `BasicLexer::lex`'s inner scan with token materialization stripped away — the
/// apples-to-apples scanner-engine comparison against `baked_scan_only`.
fn dfa_scan_only(lexer: &BasicLexer, text: &str) -> usize {
    let mut pos = 0usize;
    let mut n = 0usize;
    while pos < text.len() {
        match lexer.match_at(text, pos) {
            Some((_, slice)) => {
                pos += slice.len();
                n += 1;
            }
            None => pos += 1,
        }
    }
    n
}

/// Collect the baked scanner's `(kind, start, end)` spans — for the correctness check.
fn baked_spans(text: &str) -> Vec<(Kind, usize, usize)> {
    let b = text.as_bytes();
    let mut pos = 0usize;
    let mut out = Vec::new();
    while pos < b.len() {
        match baked_match_at(b, pos) {
            Some((k, end)) => {
                out.push((k, pos, end));
                pos = end;
            }
            None => pos += 1,
        }
    }
    out
}

// ─── measurement (copied estimator from lex_backends.rs) ────────────────────────

struct Stat {
    min_ns: f64,
    median_ns: f64,
}

fn measure<F: FnMut()>(mut f: F) -> Stat {
    let mut iters = 1usize;
    loop {
        let t = Instant::now();
        for _ in 0..iters {
            f();
        }
        if t.elapsed() >= Duration::from_millis(1) || iters >= 1 << 22 {
            break;
        }
        iters = (iters * 2).max(1);
    }
    let mut samples: Vec<f64> = Vec::new();
    let overall = Instant::now();
    while samples.len() < 50 && overall.elapsed() < Duration::from_millis(1500) {
        let t = Instant::now();
        for _ in 0..iters {
            f();
        }
        samples.push(t.elapsed().as_nanos() as f64 / iters as f64);
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    Stat {
        min_ns: samples[0],
        median_ns: samples[samples.len() / 2],
    }
}

fn mbps(bytes: usize, ns: f64) -> f64 {
    bytes as f64 / ns * 1e3
}

fn build_lexer(backend: LexerBackend) -> BasicLexer {
    let g = load_grammar(JSON_GRAMMAR, &["start".to_string()], true, false)
        .expect("json grammar loads");
    let cg = lower(&g);
    let conf = basic_lexer_conf(&cg, 0).with_backend(backend);
    BasicLexer::new(&conf).expect("json lexer builds")
}

fn main() {
    println!("# Spike exp-1: baked/directly-executable JSON scanner vs regex-automata DFA lexer");
    println!("# wall-clock is a recorded trend (ADR-0007) — only same-session ratios travel\n");

    let dfa = build_lexer(LexerBackend::Dfa);
    let regex = build_lexer(LexerBackend::Regex);

    let workloads = [
        ("json_small", 4usize, 3usize),
        ("json_medium", 64, 4),
        ("json_large", 512, 5),
    ];

    // ── Correctness: the baked scanner must reproduce the DFA's token boundaries ──
    // (else the throughput comparison times divergent work). Compare the non-ignored
    // token spans: the baked scan drops WS, and `BasicLexer::lex` filters it, so the
    // two visible-token streams must be span-identical. Input is ASCII ⇒ char==byte.
    println!("── correctness (baked spans vs BasicLexer::lex, visible tokens) ──");
    for (name, records, fields) in workloads {
        let input = gen_json(records, fields);
        let toks = dfa.lex(&input).expect("dfa lexes");
        let baked: Vec<(usize, usize)> = baked_spans(&input)
            .into_iter()
            .filter(|(k, _, _)| *k != Kind::Ws)
            .map(|(_, s, e)| (s, e))
            .collect();
        let dfa_spans: Vec<(usize, usize)> = toks
            .iter()
            .map(|t| (t.start_pos(), t.end_pos()))
            .filter(|(s, e)| e > s) // drop the synthetic zero-width $END token lex() appends
            .collect();
        assert_eq!(
            baked, dfa_spans,
            "baked scanner diverged from the DFA on {name} — comparison invalid"
        );
        // Also assert the scan-only loops agree on the *total* token count (WS incl.).
        assert_eq!(
            baked_scan_only(&input),
            dfa_scan_only(&dfa, &input),
            "scan-only token counts diverge on {name}"
        );
        println!(
            "  {name:<12} {} visible tokens, {} total (baked==dfa) ✓",
            dfa_spans.len(),
            baked_scan_only(&input)
        );
    }
    println!();

    // ── A. Scan-only: the pure scanner-engine isolation (the B1 headline) ─────────
    println!("── A. SCAN-ONLY throughput (no Token materialization — pure scanner engine) ──");
    println!(
        "  {:<12}{:>10}{:>13}{:>13}{:>13}{:>12}",
        "workload", "bytes", "dfa MB/s", "baked MB/s", "regexEng*", "baked/dfa"
    );
    for (name, records, fields) in workloads {
        let input = gen_json(records, fields);
        let n = input.len();
        let d = measure(|| {
            black_box(dfa_scan_only(&dfa, black_box(&input)));
        });
        let bk = measure(|| {
            black_box(baked_scan_only(black_box(&input)));
        });
        let rx = measure(|| {
            black_box(dfa_scan_only(&regex, black_box(&input)));
        });
        let dfa_mbps = mbps(n, d.median_ns);
        let baked_mbps = mbps(n, bk.median_ns);
        let regex_mbps = mbps(n, rx.median_ns);
        println!(
            "  {name:<12}{n:>10}{dfa_mbps:>13.1}{baked_mbps:>13.1}{regex_mbps:>13.1}{:>11.2}x",
            bk.median_ns / d.median_ns
        );
        println!(
            "BENCH\tscan_only\t{name}\t{n}\t{:.0}\t{:.0}\t{baked_mbps:.1}",
            bk.median_ns, bk.min_ns
        );
    }
    println!("  (* regexEng = the `regex`-crate scanner through the same match_at loop)\n");

    // ── B. Full lex(): DFA vs a baked full lexer materializing the SAME tokens ────
    println!("── B. FULL lex() throughput (Token materialization included — end-to-end) ──");
    println!(
        "  {:<12}{:>10}{:>13}{:>13}{:>12}",
        "workload", "bytes", "dfa MB/s", "baked MB/s", "baked/dfa"
    );
    for (name, records, fields) in workloads {
        let input = gen_json(records, fields);
        let n = input.len();
        let d = measure(|| {
            black_box(dfa.lex(black_box(&input)).unwrap());
        });
        let bk = measure(|| {
            black_box(baked_lex_full(black_box(&input)));
        });
        let dfa_mbps = mbps(n, d.median_ns);
        let baked_mbps = mbps(n, bk.median_ns);
        println!(
            "  {name:<12}{n:>10}{dfa_mbps:>13.1}{baked_mbps:>13.1}{:>11.2}x",
            bk.median_ns / d.median_ns
        );
    }
    println!();

    // ── C. Reused vs one-shot: baking moves the FSM build off the hot path ────────
    // One-shot = build the scanner + lex the largest input once; reused = lex only.
    let (name, records, fields) = workloads[2];
    let input = gen_json(records, fields);
    let n = input.len();
    println!("── C. build cost — reused (lex only) vs one-shot (build+lex), {name} ──");
    let build = measure(|| {
        let lx = build_lexer(LexerBackend::Dfa);
        black_box(lx.lex(black_box(&input)).unwrap());
    });
    let reuse = measure(|| {
        black_box(dfa.lex(black_box(&input)).unwrap());
    });
    // The baked scanner has NO build step (it is compiled code), so one-shot == reused.
    let baked_reuse = measure(|| {
        black_box(baked_lex_full(black_box(&input)));
    });
    println!(
        "  regex-automata DFA : reused {:.3} ms   one-shot(build+lex) {:.3} ms   (build ≈ {:.3} ms)",
        reuse.median_ns / 1e6,
        build.median_ns / 1e6,
        (build.median_ns - reuse.median_ns).max(0.0) / 1e6
    );
    println!(
        "  baked scanner      : reused {:.3} ms   one-shot(build+lex) {:.3} ms   (build ≈ 0 — compiled code)",
        baked_reuse.median_ns / 1e6,
        baked_reuse.median_ns / 1e6,
    );
    println!(
        "\n  → on the {n}-byte input: baked scan-only is the B1 signal; full-lex ratio shows\n    \
         how much Token materialization dilutes it. See docs/notes for the verdict."
    );
}

// ─── baked full lexer: the same Tokens BasicLexer::lex builds ───────────────────
//
// Mirrors `BasicLexer::lex` on the owned path: materialize a `Token` per *visible*
// (non-WS) token with an owned value `String` and char positions, tracking line/col
// like `LexCursor`. This is the fair end-to-end competitor for `dfa.lex()`.

use lark_rs::Token;

fn baked_lex_full(text: &str) -> Vec<Token> {
    let b = text.as_bytes();
    let mut pos = 0usize;
    let mut char_pos = 0usize;
    let mut line = 1usize;
    let mut col = 1usize;
    let mut out = Vec::new();
    while pos < b.len() {
        match baked_match_at(b, pos) {
            Some((kind, end)) => {
                let slice = &text[pos..end];
                let nchars = slice.chars().count();
                if kind != Kind::Ws {
                    let ty = match kind {
                        Kind::Punct => "PUNCT",
                        Kind::String => "ESCAPED_STRING",
                        Kind::Number => "SIGNED_NUMBER",
                        Kind::True => "TRUE",
                        Kind::False => "FALSE",
                        Kind::Null => "NULL",
                        Kind::Ws => unreachable!(),
                    };
                    out.push(Token::new(ty, slice).with_position(
                        line,
                        col,
                        char_pos,
                        char_pos + nchars,
                    ));
                }
                // advance line/col over the slice (LexCursor::feed)
                for ch in slice.chars() {
                    if ch == '\n' {
                        line += 1;
                        col = 1;
                    } else {
                        col += 1;
                    }
                }
                char_pos += nchars;
                pos = end;
            }
            None => {
                pos += 1;
                char_pos += 1;
                col += 1;
            }
        }
    }
    out
}
