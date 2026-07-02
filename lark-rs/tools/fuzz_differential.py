#!/usr/bin/env python3
"""
Differential fuzzer for lark-rs — out-of-band discovery driver.

Fuzzing is a *discovery* activity: it runs explicitly (locally) or on a nightly
schedule, never on the PR critical path, and it does **not** commit the inputs it
generates. The committed regression corpus
(`tests/fixtures/oracles/fuzz/inputs.json`) holds only *minimized finds* — the
small set of inputs that actually exposed a lark-rs ↔ Python-Lark divergence —
exactly like the compliance bank holds real bugs, not random samples.

Four things live here:

  0. Random GRAMMAR fuzzing (`--fuzz-grammars`) — the grammar-level counterpart to
     the input fuzzing below. Instead of perturbing inputs to a FIXED grammar, it
     generates random small grammars (parameterized by production count, terminal
     set, and EBNF operator mix: `*`, `+`, `?`, `|`, `~N`, templates), keeps only
     those Python Lark (the oracle) builds, generates random inputs for each, and
     diffs lark-rs against the oracle ONLINE via the `differ` binary
     (`--grammar-file`). Finds are routed through the same divergence-preserving
     minimize as `--minimize`. This surfaces template-expansion, EBNF-operator,
     nullable-edge and rule-priority bugs no fixed grammar can. Deterministic given
     `--seed`; exits non-zero on a divergence (a nightly RED == a find to triage):

        python3 tools/fuzz_differential.py --fuzz-grammars --seed 7 -n 200 \
            --gg-finds-out /tmp/grammar_finds.json

     `--gg-seed-range A:B` sweeps every seed in [A, B] in one process. That is
     the *regression* tier of this mode: `scripts/fuzz-seed-range.sh` pins the
     committed once-verified-clean range (epic #208), so a RED there is a
     regression to fix, not a discovery find to triage. Each seed's batch is
     byte-identical to a standalone `--seed` run, so any find replays from its
     single seed.

  1. Discovery (default) — generate grammar-directed + mutated inputs for the
     trusted grammars and validate them against Python Lark (the oracle). This
     reports stats only; the actual lark-rs-vs-oracle diff happens in Rust. To
     hunt for divergences, dump a batch and replay it (the lark-rs `parse()` is
     what gets diffed, in `cargo test`):

        python3 tools/fuzz_differential.py --grammar all -n 4000 --seed 7 \
            --out /tmp/fuzz_batch.json
        LARK_FUZZ_INPUTS=/tmp/fuzz_batch.json python3 tools/generate_oracles.py
        cargo test --test test_fuzz_corpus      # RED == lark-rs diverged

     The nightly workflow (`.github/workflows/lark-rs-fuzz.yml`) does exactly
     this with fresh entropy and uploads the batch as an artifact on a RED, so a
     find is reproducible and triageable without committing the haystack.

  2. Minimizing a find (`--minimize`) — ddmin-style shrink to the smallest input
     that *still diverges* between lark-rs and Python Lark. Run this on the
     offending input from a red replay to get a tight repro before recording it.

     The minimizer calls into lark-rs via the thin `differ` binary
     (`src/bin/differ.rs`, parses stdin → prints the tree as oracle-shaped JSON)
     and diffs that tree against the Python oracle at every shrink step, accepting
     a candidate only when the divergence is PRESERVED — not merely when lark-rs
     still parses. Without that check the shrink could over-minimize to an input
     the two engines AGREE on, silently losing the divergence signal (issue #37).
     If the differ is unavailable or the seed input does not actually diverge, the
     minimizer falls back to the legacy parse/reject-preserving predicate and says
     so, so it is never silently weaker than before.

  3. Recording a find (`--record`) — append the single minimized input to the
     committed corpus so it is guarded forever:

        python3 tools/fuzz_differential.py --grammar arithmetic --record \
            --input "1" --note "start-rule expand1-to-bare-token gap (see \
            test_fuzz_corpus.rs)"

     A find belongs in the corpus only once lark-rs *agrees* with the oracle on
     it (a fixed bug → a green regression guard), OR it is paired with a
     documented carve-out in `test_fuzz_corpus.rs` for a known-open gap (the
     bare-token-root case is the worked example). Recording an un-carved open
     divergence would — correctly — turn CI red.

Determinism: generation is fully determined by `--seed`, so a nightly RED is
reproducible by re-running with the seed printed in its log.

Usage:
    python3 tools/fuzz_differential.py --fuzz-grammars --seed 7 -n 200
    python3 tools/fuzz_differential.py --grammar arithmetic -n 200
    python3 tools/fuzz_differential.py --grammar all -n 4000 --seed 7 --out batch.json
    python3 tools/fuzz_differential.py --grammar arithmetic --minimize "1 +++ 2"
    python3 tools/fuzz_differential.py --grammar arithmetic --record --input "1" --note "..."
"""

import argparse
import json
import random
import re
import shutil
import subprocess
import sys
from pathlib import Path

SCRIPT_DIR = Path(__file__).parent
LARK_RS_DIR = SCRIPT_DIR.parent
INPUTS_PATH = LARK_RS_DIR / "tests" / "fixtures" / "oracles" / "fuzz" / "inputs.json"
GRAMMARS_DIR = LARK_RS_DIR / "tests" / "grammars"

# Import the in-repo Python Lark (our oracle), same as generate_oracles.py.
sys.path.insert(0, str(LARK_RS_DIR.parent))
from lark import Lark
from lark.exceptions import LarkError


# ─── Grammar-directed generators ─────────────────────────────────────────────
#
# Bespoke per-grammar generators emit mostly-valid strings (exercising the
# accept path and, crucially, tree *shape* — where lark-rs is most likely to
# drift: expand1, transparent splicing, operator precedence). A generic
# grammar-walker is a worthwhile future upgrade; these reliable hand generators
# get the differential loop paying off today.

_WS = ["", " ", "  ", " \t"]

# Cap unbounded repetition so generated inputs stay *legible*. Tree-shape
# coverage comes from nesting depth, not from 2000-char flat operator chains —
# and a divergence on a short input is something a human can actually read.
_MAX_REPEAT = 4


def _sp(rng):
    return rng.choice(_WS)


def _times(rng, p):
    """Yield while a biased coin keeps coming up, capped at _MAX_REPEAT."""
    n = 0
    while n < _MAX_REPEAT and rng.random() < p:
        n += 1
        yield n


def gen_arithmetic(rng, depth=4):
    def number():
        if rng.random() < 0.3:
            return f"{rng.randint(0, 999)}.{rng.randint(0, 99)}"
        return str(rng.randint(0, 9999))

    def name():
        first = rng.choice("abcxyz_")
        rest = "".join(rng.choice("abc_0123") for _ in range(rng.randint(0, 3)))
        return first + rest

    def atom(d):
        if d <= 0:
            return number() if rng.random() < 0.6 else name()
        r = rng.random()
        if r < 0.4:
            return number()
        if r < 0.6:
            return name()
        return f"({_sp(rng)}{expr(d - 1)}{_sp(rng)})"

    def factor(d):
        prefix = "".join(rng.choice(["+", "-"]) + _sp(rng) for _ in _times(rng, 0.25))
        return prefix + atom(d)

    def term(d):
        out = factor(d)
        for _ in _times(rng, 0.4):
            out += f"{_sp(rng)}{rng.choice(['*', '/'])}{_sp(rng)}{factor(d)}"
        return out

    def expr(d):
        out = term(d)
        for _ in _times(rng, 0.5):
            out += f"{_sp(rng)}{rng.choice(['+', '-'])}{_sp(rng)}{term(d)}"
        return out

    return expr(depth)


def gen_json(rng, depth=4):
    def string():
        # ESCAPED_STRING — keep the body to safe, unambiguous characters.
        body = "".join(
            rng.choice("abc ABZ019_:-") for _ in range(rng.randint(0, 6))
        )
        return f'"{body}"'

    def number():
        s = "-" if rng.random() < 0.3 else ""
        s += str(rng.randint(0, 9999))
        if rng.random() < 0.3:
            s += f".{rng.randint(0, 999)}"
        return s

    def scalar():
        return rng.choice([string(), number(), "true", "false", "null"])

    def value(d):
        if d <= 0 or rng.random() < 0.5:
            return scalar()
        if rng.random() < 0.5:
            n = rng.randint(0, 3)
            items = [f"{_sp(rng)}{value(d - 1)}" for _ in range(n)]
            return "[" + ",".join(items) + _sp(rng) + "]"
        n = rng.randint(0, 3)
        pairs = [f"{_sp(rng)}{string()}:{_sp(rng)}{value(d - 1)}" for _ in range(n)]
        return "{" + ",".join(pairs) + _sp(rng) + "}"

    return value(depth)


GENERATORS = {
    "arithmetic": gen_arithmetic,
    "json": gen_json,
}


# ─── Mutation (near-valid inputs probe the reject path + boundaries) ──────────

_MUTATION_CHARS = list("+-*/(){}[],.:\"0a \t")


def mutate(rng, s):
    if not s:
        return rng.choice(_MUTATION_CHARS)
    chars = list(s)
    for _ in range(rng.randint(1, 3)):
        op = rng.randint(0, 3)
        i = rng.randrange(len(chars))
        if op == 0 and len(chars) > 1:  # delete
            del chars[i]
        elif op == 1:  # insert
            chars.insert(i, rng.choice(_MUTATION_CHARS))
        elif op == 2:  # duplicate
            chars.insert(i, chars[i])
        else:  # replace
            chars[i] = rng.choice(_MUTATION_CHARS)
        if not chars:
            chars = [rng.choice(_MUTATION_CHARS)]
    return "".join(chars)


# ─── Random grammar generation (issue #38) ───────────────────────────────────
#
# Input fuzzing (above) hunts for divergences in a FIXED, trusted grammar. Random
# *grammar* fuzzing instead perturbs the grammar itself — that is where
# template-expansion, EBNF-operator-interaction, nullable-edge and rule-priority
# bugs live, none of which a fixed grammar can surface. The generator emits small
# grammars parameterized by:
#
#   * production count        (--gg-rules)
#   * terminal set            (a small alphabet of single-char string terminals)
#   * EBNF operator mix       (`*`, `+`, `?`, `|`, `~N`, and a template instance)
#
# It deliberately stays in the LALR-buildable, contextual-lexable subset the fuzz
# oracle uses (`load_parser`): a `start` rule plus N helper rules over a handful
# of literal terminals. Many random grammars are still LALR-rejected (conflicts)
# or otherwise unbuildable — Python Lark is the oracle, so any grammar IT rejects
# is skipped (step 2 of the Done-when). Generation is fully seeded, so a nightly
# find replays from the seed in its log.

# Single-character literal terminals the generated rules are built from. Kept tiny
# and unambiguous so the LALR contextual lexer has a fighting chance to build, and
# so a divergence lands on a legible input.
_GG_TERMINALS = ["a", "b", "c", "d"]


def _gg_atom(rng, nonterminals):
    """A single grammar symbol: a literal terminal or a reference to a helper rule."""
    if nonterminals and rng.random() < 0.45:
        return rng.choice(nonterminals)
    return f'"{rng.choice(_GG_TERMINALS)}"'


def _gg_term_with_op(rng, nonterminals, allow_template_ref):
    """One item in a rule alternative: an atom optionally wrapped in an EBNF
    operator (`*`, `+`, `?`, `~N`, or a grouped run), or a template instantiation."""
    if allow_template_ref and rng.random() < 0.2:
        # Instantiate the generated template `rep{X}` with a random argument.
        return f'rep{{{_gg_atom(rng, nonterminals)}}}'

    atom = _gg_atom(rng, nonterminals)
    r = rng.random()
    if r < 0.30:
        return atom                      # bare
    if r < 0.45:
        return atom + "?"                # optional (nullable edge)
    if r < 0.60:
        return atom + "*"                # zero-or-more (nullable edge)
    if r < 0.72:
        return atom + "+"                # one-or-more
    if r < 0.85:
        n = rng.randint(1, 3)
        return atom + f"~{n}"            # exact repetition ~N
    # A grouped alternation run, itself optionally repeated.
    inner = " | ".join(_gg_atom(rng, nonterminals) for _ in range(rng.randint(2, 3)))
    suffix = rng.choice(["", "*", "+", "?"])
    return f"({inner}){suffix}"


def _gg_alternative(rng, nonterminals, allow_template_ref):
    """One alternative of a rule: a sequence of 1-3 operator-wrapped items."""
    n = rng.randint(1, 3)
    return " ".join(
        _gg_term_with_op(rng, nonterminals, allow_template_ref) for _ in range(n))


def generate_grammar(rng, n_rules):
    """Build one random small grammar (as `.lark` text), deterministically given
    `rng`. Always has a `start` rule and `n_rules` helper rules `r0..r{n-1}`, plus
    a parameterized template `rep{x}: x x?` to exercise template expansion.

    Returns the grammar text. Whether it actually BUILDS is decided by Python Lark
    (the oracle) at the call site — unbuildable grammars are skipped."""
    nonterminals = [f"r{i}" for i in range(n_rules)]

    lines = []
    # start references the helper rules + literals; `|` gives it 1-2 alternatives.
    start_alts = [_gg_alternative(rng, nonterminals, allow_template_ref=True)
                  for _ in range(rng.randint(1, 2))]
    lines.append("start: " + " | ".join(start_alts))

    for i, nt in enumerate(nonterminals):
        # A helper rule may reference rules defined AFTER it (forward refs are fine
        # in Lark) but never itself-only-recursively in a way that can't terminate;
        # an alternative containing a bare literal keeps every rule reachable to a
        # terminal. Each rule gets 1-2 `|` alternatives.
        peers = [p for p in nonterminals if p != nt]
        alts = []
        for _ in range(rng.randint(1, 2)):
            alts.append(_gg_alternative(rng, peers, allow_template_ref=False))
        # Guarantee a non-recursive escape hatch (a bare literal alternative) so the
        # rule's language is non-empty and inputs are generatable.
        alts.append(f'"{rng.choice(_GG_TERMINALS)}"')
        lines.append(f"{nt}: " + " | ".join(alts))

    # A parameterized template exercising template expansion (`rep{x}: x x?`).
    lines.append("rep{x}: x x?")

    lines.append('%ignore " "')
    return "\n".join(lines) + "\n"


def generate_grammar_input(rng, parser, max_len=12):
    """Generate a random input string for a built grammar. The generator is
    grammar-agnostic: it samples short strings over the terminal alphabet (plus a
    little whitespace), so most are near-valid and some are rejects — both
    exercise the differ. Bounded length keeps a find legible."""
    n = rng.randint(0, max_len)
    pieces = []
    for _ in range(n):
        if rng.random() < 0.2:
            pieces.append(" ")
        else:
            pieces.append(rng.choice(_GG_TERMINALS))
    return "".join(pieces)


# ─── Oracle plumbing ─────────────────────────────────────────────────────────

def load_parser(grammar):
    text = (GRAMMARS_DIR / f"{grammar}.lark").read_text()
    return Lark(text, parser="lalr", lexer="contextual", start="start",
                maybe_placeholders=False)


def parses(parser, text):
    try:
        parser.parse(text)
        return True
    except LarkError:
        return False


def tree_to_dict(node):
    """Serialise a Python-Lark parse tree to the same dict shape the differ binary
    prints (and generate_oracles.py freezes), so the two are directly comparable."""
    # Imported lazily: only the divergence-preserving minimize path needs it.
    from lark import Tree, Token
    if isinstance(node, Tree):
        return {"type": "tree", "data": node.data,
                "children": [tree_to_dict(c) for c in node.children]}
    if isinstance(node, Token):
        return {"type": "token", "token_type": str(node.type), "value": str(node)}
    return {"type": "unknown", "repr": repr(node)}


def oracle_result(parser, text):
    """The Python-Lark oracle verdict for `text`: (ok, tree_dict | None)."""
    try:
        return True, tree_to_dict(parser.parse(text))
    except LarkError:
        return False, None


# ─── lark-rs bridge (the online differ) ──────────────────────────────────────
#
# The minimizer asks lark-rs what it produces for a candidate via the thin
# `differ` binary, then compares that against the Python oracle. This is the
# "online Rust-side differ" of issue #37: it lets the shrink loop *preserve the
# divergence* rather than merely preserving parse-success.

def find_differ_binary(explicit=None):
    """Locate (building if needed) the `differ` binary. Returns its path, or None
    if it cannot be built (the caller then falls back to the legacy predicate)."""
    if explicit:
        p = Path(explicit)
        return str(p) if p.exists() else None

    # Prefer an already-built binary (release, then debug) to stay fast.
    for profile in ("release", "debug"):
        cand = LARK_RS_DIR / "target" / profile / "differ"
        if cand.exists():
            return str(cand)

    # Not built yet — build it once (debug is enough for a tree print).
    if shutil.which("cargo") is None:
        return None
    print("building the `differ` binary (cargo build --bin differ)...",
          file=sys.stderr)
    proc = subprocess.run(
        ["cargo", "build", "--bin", "differ"],
        cwd=str(LARK_RS_DIR), capture_output=True, text=True, encoding="utf-8")
    if proc.returncode != 0:
        print(f"  cargo build failed:\n{proc.stderr}", file=sys.stderr)
        return None
    cand = LARK_RS_DIR / "target" / "debug" / "differ"
    return str(cand) if cand.exists() else None


def larkrs_result(differ_bin, grammar, text, grammar_file=None):
    """Run `text` through lark-rs via the differ binary: (ok, tree_dict | None).

    When `grammar_file` is given, the differ loads the grammar from that path
    (`--grammar-file`, used by --fuzz-grammars for random grammars that have no
    committed fixture); otherwise it loads the trusted fixture by name
    (`--grammar <grammar>`).

    Raises RuntimeError if the differ cannot be invoked or returns malformed
    output — a hard failure the caller surfaces rather than silently treating as
    'no divergence' (which would re-introduce the over-minimization bug)."""
    args = ([differ_bin, "--grammar-file", grammar_file] if grammar_file
            else [differ_bin, "--grammar", grammar])
    # Force UTF-8 so a non-ASCII candidate or token value can't raise a locale
    # encode/decode error under a C/POSIX locale (nightly cron/CI).
    proc = subprocess.run(
        args, input=text, capture_output=True, text=True, encoding="utf-8")
    if proc.returncode != 0:
        raise RuntimeError(
            f"differ exited {proc.returncode} for grammar {grammar!r}: "
            f"{proc.stderr.strip()}")
    try:
        out = json.loads(proc.stdout)
        return bool(out["ok"]), out.get("tree")
    except (json.JSONDecodeError, KeyError) as e:
        raise RuntimeError(f"differ produced malformed output {proc.stdout!r}: {e}")


def diverges(parser, differ_bin, grammar, text, grammar_file=None):
    """True iff lark-rs and Python Lark disagree on `text` — either on accept/reject
    or on the produced tree. This is the predicate the minimizer preserves."""
    py_ok, py_tree = oracle_result(parser, text)
    rs_ok, rs_tree = larkrs_result(differ_bin, grammar, text, grammar_file=grammar_file)
    if py_ok != rs_ok:
        return True
    # Both accepted or both rejected; a tree mismatch (only meaningful when both
    # accepted) is also a divergence.
    return py_ok and py_tree != rs_tree


def minimize(parser, text, predicate):
    """ddmin-style shrink: smallest substring still satisfying `predicate`.

    The divergence-preserving predicate (`diverges`, which also runs lark-rs via
    the differ binary) is what `--minimize` uses by default; the legacy
    parse/reject-preserving predicate is the documented fallback. The shrink loop
    itself is predicate-agnostic.
    """
    best = text
    changed = True
    while changed:
        changed = False
        n = max(1, len(best) // 2)
        while n >= 1:
            i = 0
            while i < len(best):
                cand = best[:i] + best[i + n:]
                if cand != best and predicate(cand):
                    best = cand
                    changed = True
                else:
                    i += n
            n //= 2
    return best


# ─── Corpus I/O ──────────────────────────────────────────────────────────────

def load_inputs():
    if INPUTS_PATH.exists():
        return json.loads(INPUTS_PATH.read_text())
    return []


def save_inputs(entries):
    INPUTS_PATH.parent.mkdir(parents=True, exist_ok=True)
    INPUTS_PATH.write_text(json.dumps(entries, indent=2, ensure_ascii=False) + "\n")


# ─── Discovery batch ─────────────────────────────────────────────────────────

def generate_batch(grammars, count, seed, depth, do_mutate):
    """Generate `count` (grammar, input) pairs, deterministically given `seed`."""
    rng = random.Random(seed)
    batch = []
    for _ in range(count):
        grammar = rng.choice(grammars)
        s = GENERATORS[grammar](rng, depth)
        batch.append((grammar, s))
        if do_mutate and rng.random() < 0.5:
            batch.append((grammar, mutate(rng, s)))
    return batch


def _fuzz_grammars_one_seed(seed, args, differ_bin, scratch_dir):
    """One deterministic --fuzz-grammars batch for `seed`. A fresh RNG per seed
    keeps every seed's batch byte-identical whether it runs standalone
    (`--seed S`) or as one slice of a `--gg-seed-range` sweep — a range find
    always replays from its single seed."""
    rng = random.Random(seed)

    n_generated = 0
    n_built = 0
    n_inputs = 0
    finds = []  # (grammar_text, grammar_file, input, seed) tuples that diverged

    for gi in range(args.count):
        n_rules = rng.randint(1, args.gg_rules)
        grammar_text = generate_grammar(rng, n_rules)
        n_generated += 1

        # Step 2: Python Lark is the oracle — skip any grammar IT rejects.
        try:
            parser = Lark(grammar_text, parser="lalr", lexer="contextual",
                          start="start", maybe_placeholders=False)
        except LarkError:
            continue
        except Exception:
            # Non-LarkError build failures (e.g. an unsupported template shape) are
            # also "the oracle won't build it" — skip, don't crash the batch.
            continue
        n_built += 1

        # Write the exact built text to a scratch file for the differ.
        grammar_file = scratch_dir / f"gg_{seed}_{gi}.lark"
        grammar_file.write_text(grammar_text)
        grammar_file_str = str(grammar_file)

        # Step 3: generate inputs and diff lark-rs against the oracle.
        for _ in range(args.gg_inputs):
            inp = generate_grammar_input(rng, parser)
            n_inputs += 1
            try:
                # Pass the scratch path as the grammar *label* too, so a differ
                # RuntimeError names the exact .lark file (reproducibility), not an
                # opaque '<random>'.
                if diverges(parser, differ_bin, grammar_file_str, inp,
                            grammar_file=grammar_file_str):
                    finds.append((grammar_text, grammar_file_str, inp, seed))
                    break  # one find per grammar is plenty to triage
            except RuntimeError as e:
                # A differ invocation failure is a real, reportable problem (e.g. a
                # grammar lark-rs cannot build that Python can) — surface it as a
                # find so it is not silently lost.
                print(f"differ error on grammar {grammar_file_str} input {inp!r}: {e}",
                      file=sys.stderr)
                finds.append((grammar_text, grammar_file_str, inp, seed))
                break

    print(f"fuzz-grammars (seed={seed}): generated {n_generated}, "
          f"{n_built} built in Python Lark, {n_inputs} inputs diffed, "
          f"{len(finds)} grammar(s) diverged")
    return n_generated, n_built, n_inputs, finds


def fuzz_grammars(args, ap):
    """`--fuzz-grammars` mode: generate random grammars, validate each against the
    Python-Lark oracle (skip rejects), generate inputs, diff lark-rs against the
    oracle online (via the differ binary), and route any find through
    minimize+record.

    This is the grammar-level counterpart to input fuzzing: it surfaces
    template-expansion, EBNF-operator, nullable-edge and priority bugs a fixed
    grammar cannot. Deterministic given --seed: a nightly find replays from the
    seed in its log. With --gg-seed-range A:B it sweeps every seed in [A, B]
    in one process — the *regression* tier over a committed, once-verified-clean
    range (epic #208), as opposed to the fresh-entropy discovery tier.
    """
    # --fuzz-grammars has no legacy fallback: the only way to diff lark-rs against
    # the oracle for a random grammar is the online differ, so --no-differ is
    # meaningless here. Reject it up front rather than crashing mid-batch when the
    # first diverges() call hits a None binary.
    if args.no_differ:
        ap.error("--fuzz-grammars requires the differ binary; --no-differ is not "
                 "supported (there is no legacy fallback for a random grammar)")

    if args.gg_seed_range:
        m = re.fullmatch(r"(\d+):(\d+)", args.gg_seed_range)
        if not m:
            ap.error("--gg-seed-range must be START:END (e.g. 1:300)")
        lo, hi = int(m.group(1)), int(m.group(2))
        if lo > hi:
            ap.error("--gg-seed-range START must be <= END")
        seeds = range(lo, hi + 1)
    else:
        seeds = [args.seed]

    differ_bin = find_differ_binary(args.differ_bin)
    if differ_bin is None:
        print("error: --fuzz-grammars needs the `differ` binary to diff lark-rs "
              "against the oracle; build failed or cargo is unavailable.",
              file=sys.stderr)
        return 2

    # A scratch dir to hold each random grammar as a real `.lark` file so the
    # differ (`--grammar-file`) and Python Lark load IDENTICAL text. Never committed.
    # Clear stale `gg_*.lark` from a prior run so a leftover file (e.g. from a
    # larger earlier count, or a same-seed re-run) can never be mistaken for a find
    # of THIS run — the grammar_file path in a report must point at this run's text.
    scratch_dir = Path(args.gg_scratch_dir) if args.gg_scratch_dir else \
        Path(LARK_RS_DIR) / "target" / "fuzz_grammars"
    scratch_dir.mkdir(parents=True, exist_ok=True)
    for stale in scratch_dir.glob("gg_*.lark"):
        stale.unlink()

    n_generated = 0
    n_built = 0
    n_inputs = 0
    finds = []
    for seed in seeds:
        g, b, i, f = _fuzz_grammars_one_seed(seed, args, differ_bin, scratch_dir)
        n_generated += g
        n_built += b
        n_inputs += i
        finds.extend(f)

    if len(seeds) > 1:
        print(f"fuzz-grammars seed-range {seeds[0]}..{seeds[-1]}: "
              f"generated {n_generated}, {n_built} built in Python Lark, "
              f"{n_inputs} inputs diffed, {len(finds)} grammar(s) diverged")

    if not finds:
        print("(no divergence — clean run)")
        return 0

    # Step 4: route each find through the existing minimize workflow and emit a
    # ready-to-record report. We do NOT auto-commit (recording a random grammar's
    # find needs a human-written grammar fixture + note); we print the minimized
    # input and the grammar so a maintainer can triage and `--record` it.
    print(f"\n{len(finds)} divergence(s) found — minimized repros:")
    reports = []
    for grammar_text, grammar_file_str, inp, seed in finds:
        parser = Lark(grammar_text, parser="lalr", lexer="contextual",
                      start="start", maybe_placeholders=False)

        # Divergence-preserving shrink (the #37 predicate), reused verbatim: only
        # accept a candidate that STILL diverges; a differ error mid-shrink counts
        # as 'does not satisfy' so a flaky call can never over-minimize to an
        # agreeing input.
        def diverge_pred(s, _parser=parser, _gf=grammar_file_str):
            if s == "":
                return False
            try:
                return diverges(_parser, differ_bin, _gf, s, grammar_file=_gf)
            except RuntimeError:
                return False

        small = minimize(parser, inp, diverge_pred)
        report = {
            "seed": seed,
            "grammar": grammar_text,
            "grammar_file": grammar_file_str,
            "input": inp,
            "minimized_input": small,
        }
        reports.append(report)
        print(json.dumps(report, ensure_ascii=False))

    if args.gg_finds_out:
        Path(args.gg_finds_out).write_text(
            json.dumps(reports, indent=2, ensure_ascii=False) + "\n")
        print(f"wrote {len(reports)} find(s) -> {args.gg_finds_out}")

    # A divergence is the signal the nightly job keys on: exit non-zero so CI flags
    # it (RED == a new find to triage), mirroring the input-fuzz replay's RED.
    return 1


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    grammar_choices = sorted(GENERATORS) + ["all"]
    ap.add_argument("--grammar", choices=grammar_choices, default="arithmetic",
                    help="grammar to fuzz ('all' fans out over every generator)")
    ap.add_argument("-n", "--count", type=int, default=100,
                    help="number of inputs to generate")
    ap.add_argument("--seed", type=int, default=0, help="RNG seed (deterministic)")
    ap.add_argument("--depth", type=int, default=4, help="max generation depth")
    ap.add_argument("--mutate", action="store_true",
                    help="also emit mutated (near-valid) variants")
    ap.add_argument("--out", metavar="FILE",
                    help="write the generated batch to FILE (a scratch corpus for "
                         "replay/nightly discovery); never the committed corpus")
    ap.add_argument("--minimize", metavar="INPUT",
                    help="shrink INPUT to a minimal core that still DIVERGES between "
                         "lark-rs and Python Lark (via the differ binary), and exit; "
                         "falls back to parse/reject-preserving if the input does not "
                         "diverge or the differ is unavailable")
    ap.add_argument("--differ-bin", metavar="PATH",
                    help="path to the lark-rs `differ` binary (default: build/locate "
                         "target/{release,debug}/differ)")
    ap.add_argument("--no-differ", action="store_true",
                    help="force the legacy parse/reject-preserving minimize (skip the "
                         "online lark-rs differ entirely)")
    ap.add_argument("--record", action="store_true",
                    help="append a single minimized find (--input/--note) to the "
                         "committed regression corpus and exit")
    ap.add_argument("--input", metavar="STR", help="the input to --record")
    ap.add_argument("--note", metavar="STR",
                    help="why this find is guarded (required by --record)")
    # ── Random GRAMMAR fuzzing (issue #38) ───────────────────────────────────
    ap.add_argument("--fuzz-grammars", action="store_true",
                    help="generate random small grammars (skip those Python Lark "
                         "rejects), generate inputs, and diff lark-rs against the "
                         "oracle online via the differ binary; routes finds through "
                         "minimize. Exits non-zero on a divergence (a nightly find).")
    ap.add_argument("--gg-rules", type=int, default=4, metavar="N",
                    help="max helper-rule (production) count per generated grammar "
                         "(--fuzz-grammars); each grammar gets 1..N")
    ap.add_argument("--gg-inputs", type=int, default=20, metavar="N",
                    help="random inputs to diff per built grammar (--fuzz-grammars)")
    ap.add_argument("--gg-seed-range", metavar="A:B",
                    help="sweep --fuzz-grammars over every seed in [A, B] "
                         "(inclusive) in one process, overriding --seed; each "
                         "seed's batch is byte-identical to a standalone --seed "
                         "run. This is the committed-range REGRESSION tier "
                         "(scripts/fuzz-seed-range.sh, epic #208) — a divergence "
                         "here is a regression, not a discovery find")
    ap.add_argument("--gg-scratch-dir", metavar="DIR",
                    help="where to write the random `.lark` files the differ reads "
                         "(default: target/fuzz_grammars; never committed)")
    ap.add_argument("--gg-finds-out", metavar="FILE",
                    help="write the minimized grammar-fuzz finds to FILE as JSON "
                         "(for the nightly artifact upload)")
    args = ap.parse_args()

    # ── Random grammar fuzzing (its own self-contained discovery loop) ───────
    if args.fuzz_grammars:
        sys.exit(fuzz_grammars(args, ap))

    grammars = sorted(GENERATORS) if args.grammar == "all" else [args.grammar]

    # ── Record a minimized find into the committed corpus ────────────────────
    if args.record:
        if args.grammar == "all" or not args.input or not args.note:
            ap.error("--record requires a concrete --grammar, --input and --note")
        entries = load_inputs()
        if any(e["grammar"] == args.grammar and e["input"] == args.input
               for e in entries):
            print("already recorded — nothing to do")
            return
        verdict = "parses" if parses(load_parser(args.grammar), args.input) \
            else "rejects"
        entries.append({"grammar": args.grammar, "input": args.input,
                        "note": args.note})
        save_inputs(entries)
        rel = INPUTS_PATH.relative_to(LARK_RS_DIR)
        print(f"recorded [{args.grammar}] {args.input!r} (Python Lark {verdict}) -> {rel}")
        print("next: python3 tools/generate_oracles.py && "
              "cargo test --test test_fuzz_corpus")
        return

    # ── Minimize a single input ──────────────────────────────────────────────
    if args.minimize is not None:
        if args.grammar == "all":
            ap.error("--minimize requires a concrete --grammar")
        parser = load_parser(args.grammar)
        seed = args.minimize

        # Legacy parse/reject-preserving predicate (the documented fallback).
        def parse_pred(s):
            if parses(parser, seed):
                return parses(parser, s)
            return not parses(parser, s) and s != ""

        mode = "parse/reject-preserving (legacy)"
        pred = parse_pred

        # Default: divergence-preserving. Only engage it when (a) the differ is
        # available and (b) the SEED input actually diverges — otherwise there is
        # no divergence to preserve and the parse-preserving fallback is correct.
        if not args.no_differ:
            differ_bin = find_differ_binary(args.differ_bin)
            if differ_bin is None:
                print("warning: differ binary unavailable — falling back to the "
                      "legacy parse/reject-preserving minimize.", file=sys.stderr)
            else:
                try:
                    seed_diverges = diverges(parser, differ_bin, args.grammar, seed)
                except RuntimeError as e:
                    print(f"warning: differ call failed ({e}) — falling back to the "
                          "legacy parse/reject-preserving minimize.", file=sys.stderr)
                    seed_diverges = None
                if seed_diverges is True:
                    mode = "divergence-preserving (lark-rs vs Python Lark)"
                    # Only accept a candidate that still diverges. A differ error
                    # mid-shrink is treated as 'does not satisfy' (reject the
                    # reduction) so a flaky call can never silently widen the input
                    # down to an agreeing case.
                    def diverge_pred(s):
                        if s == "":
                            return False
                        try:
                            return diverges(parser, differ_bin, args.grammar, s)
                        except RuntimeError:
                            return False
                    pred = diverge_pred
                elif seed_diverges is False:
                    print("warning: the seed input does NOT diverge (lark-rs agrees "
                          "with Python Lark) — nothing to preserve; using the legacy "
                          "parse/reject-preserving minimize.", file=sys.stderr)

        print(f"minimizing under: {mode}", file=sys.stderr)
        small = minimize(parser, seed, pred)
        print(json.dumps({"grammar": args.grammar, "input": small}, ensure_ascii=False))
        return

    # ── Discovery: generate, validate against the oracle, report ─────────────
    batch = generate_batch(grammars, args.count, args.seed, args.depth, args.mutate)
    parsers = {}  # grammar -> Lark, built once
    n_pass = 0
    for grammar, s in batch:
        if grammar not in parsers:
            parsers[grammar] = load_parser(grammar)
        if parses(parsers[grammar], s):
            n_pass += 1

    print(f"generated {len(batch)} inputs over {grammars} (seed={args.seed}): "
          f"{n_pass} parse, {len(batch) - n_pass} reject")

    if args.out:
        # De-dupe; a scratch batch for the Rust replay to diff lark-rs against.
        seen = set()
        scratch = []
        for grammar, s in batch:
            key = (grammar, s)
            if key not in seen:
                seen.add(key)
                scratch.append({"grammar": grammar, "input": s})
        Path(args.out).write_text(
            json.dumps(scratch, indent=2, ensure_ascii=False) + "\n")
        print(f"wrote {len(scratch)} de-duped inputs -> {args.out}")
        print(f"next: LARK_FUZZ_INPUTS={args.out} python3 tools/generate_oracles.py && "
              "cargo test --test test_fuzz_corpus")
    else:
        print("(discovery only — pass --out FILE to replay, "
              "or --record to keep a minimized find)")


if __name__ == "__main__":
    main()
