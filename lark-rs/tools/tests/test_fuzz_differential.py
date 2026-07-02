#!/usr/bin/env python3
"""Failed-first / regression demonstration for the online divergence-preserving
minimizer (issue #37).

The gap: `--minimize` used to shrink a diverging find while preserving only
*parse-success*. That can over-minimize to a case where lark-rs and Python Lark
actually AGREE — silently losing the divergence signal the fuzzer found.

This test reproduces that gap deterministically without needing a live lark-rs
bug: it points the minimizer at a *fake* `differ` binary that injects a known,
controlled divergence (it rejects any input containing `*`, which Python Lark's
arithmetic grammar happily parses). Then:

  * the legacy parse/reject-preserving predicate over-minimizes `"1*1"` down to
    `"1"`, on which the fake differ and Python AGREE — the divergence is lost
    (this is the bug, asserted to still hold for the legacy predicate); and
  * the new divergence-preserving predicate keeps the `*`, so the shrunk result
    still diverges — the signal is preserved (the fix).

Run directly:  python3 tools/tests/test_fuzz_differential.py
"""

import json
import stat
import subprocess
import sys
import tempfile
from pathlib import Path

TOOLS_DIR = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(TOOLS_DIR))

import fuzz_differential as fz  # noqa: E402


# A fake `differ` binary: parses stdin and prints oracle-shaped JSON, but lies by
# REJECTING any input containing '*'. Python Lark's arithmetic grammar parses
# "1*1" fine, so '*'-containing inputs are exactly the injected divergence class.
# Everything else mirrors Python's accept/reject (here: accept iff non-empty and
# only digits/`+`/`*`, which is enough for the digit-only agreeing cases we shrink to).
_FAKE_DIFFER = """#!/usr/bin/env python3
import sys
data = sys.stdin.read()
# Inject a divergence: pretend lark-rs cannot parse anything containing '*'.
if '*' in data or data == '' or any(c not in '0123456789+*' for c in data):
    print('{"ok": false, "tree": null}')
else:
    # Agree with Python on a plain digit/`+` expression by emitting a tree. The
    # exact shape does not matter for the accept/reject divergence we test; a
    # token root is the simplest valid oracle-shaped node.
    print('{"ok": true, "tree": {"type": "token", "token_type": "NUMBER", "value": "%s"}}' % data)
"""


def _write_fake_differ(tmpdir):
    path = Path(tmpdir) / "fake_differ"
    path.write_text(_FAKE_DIFFER)
    path.chmod(path.stat().st_mode | stat.S_IEXEC | stat.S_IRWXU)
    return str(path)


# A fake differ for the --fuzz-grammars find path: rejects EVERY input (valid
# oracle-shaped JSON, exit 0), so any input the generated grammar's Python
# parser accepts is a controlled accept/reject divergence — no live lark-rs bug
# needed. It ignores argv, so --grammar-file invocations work unchanged.
_REJECT_ALL_DIFFER = """#!/usr/bin/env python3
import sys
sys.stdin.read()
print('{"ok": false, "tree": null}')
"""


def _run_tool(args, cwd):
    return subprocess.run(
        [sys.executable, str(TOOLS_DIR / "fuzz_differential.py"), *args],
        cwd=cwd, capture_output=True, text=True, encoding="utf-8")


def test_seed_range_find_path():
    """Pins the --gg-seed-range find/report plumbing (epic #208):

    * a range sweep routes finds through the report path with the full replay
      recipe (seed + count/gg_rules/gg_inputs) and exits 1;
    * a seed's slice of a range run is byte-identical to a standalone --seed
      run (same grammar/input finds);
    * --gg-seed-range without --fuzz-grammars is a loud argparse error, never
      a silent fall-through to the unrelated discovery mode."""
    with tempfile.TemporaryDirectory() as tmpdir:
        fake = Path(tmpdir) / "reject_all_differ"
        fake.write_text(_REJECT_ALL_DIFFER)
        fake.chmod(fake.stat().st_mode | stat.S_IEXEC | stat.S_IRWXU)

        common = ["--fuzz-grammars", "-n", "10", "--gg-inputs", "8",
                  "--differ-bin", str(fake)]

        # Range sweep: finds carry seed + recipe, exit code 1 (a find REDs CI).
        finds_out = Path(tmpdir) / "range_finds.json"
        proc = _run_tool(common + [
            "--gg-seed-range", "13:14",
            "--gg-scratch-dir", str(Path(tmpdir) / "range_scratch"),
            "--gg-finds-out", str(finds_out)], tmpdir)
        assert proc.returncode == 1, \
            f"range sweep with a rejecting differ must exit 1, got " \
            f"{proc.returncode}\n{proc.stdout}\n{proc.stderr}"
        reports = json.loads(finds_out.read_text())
        assert reports, "expected at least one find from seeds 13:14"
        for r in reports:
            assert r["seed"] in (13, 14), r
            assert (r["count"], r["gg_rules"], r["gg_inputs"]) == (10, 4, 8), \
                f"report must carry its replay recipe: {r}"

        # Determinism: the range's seed-13 slice == a standalone --seed 13 run.
        solo_out = Path(tmpdir) / "solo_finds.json"
        proc = _run_tool(common + [
            "--seed", "13",
            "--gg-scratch-dir", str(Path(tmpdir) / "solo_scratch"),
            "--gg-finds-out", str(solo_out)], tmpdir)
        assert proc.returncode == 1, proc.stdout + proc.stderr
        solo = json.loads(solo_out.read_text())

        def key(r):  # grammar_file paths differ by scratch dir; compare content
            return (r["seed"], r["grammar"], r["input"], r["minimized_input"])

        range_13 = sorted(key(r) for r in reports if r["seed"] == 13)
        assert range_13 == sorted(key(r) for r in solo), \
            "seed 13's range slice must equal its standalone run"

        # The guard: --gg-seed-range without --fuzz-grammars must error out
        # (argparse exit 2), not silently run the discovery mode and exit 0.
        proc = _run_tool(["--gg-seed-range", "13:14"], tmpdir)
        assert proc.returncode == 2 and "--fuzz-grammars" in proc.stderr, \
            f"expected a loud argparse error, got exit {proc.returncode}: " \
            f"{proc.stderr}"

    print("OK: --gg-seed-range find path carries seed+recipe, range slices "
          "replay standalone, and the flag is loud without --fuzz-grammars.")


def main():
    parser = fz.load_parser("arithmetic")
    seed = "1*1"

    # Sanity: Python Lark parses the seed; the fake differ rejects it → divergence.
    assert fz.parses(parser, seed), "seed must parse in Python Lark"

    with tempfile.TemporaryDirectory() as tmpdir:
        fake = _write_fake_differ(tmpdir)

        rs_ok, _ = fz.larkrs_result(fake, "arithmetic", seed)
        assert rs_ok is False, "fake differ should reject the '*' seed"
        assert fz.diverges(parser, fake, "arithmetic", seed), \
            "seed must diverge (Python accepts, fake differ rejects)"

        # ── The BUG: the legacy parse-preserving predicate over-minimizes ──────
        # It only preserves parse-success, so it shrinks '1*1' down to a minimal
        # parsing input ('1') on which the two engines AGREE — divergence lost.
        def parse_pred(s):
            return fz.parses(parser, s)

        legacy_small = fz.minimize(parser, seed, parse_pred)
        legacy_diverges = fz.diverges(parser, fake, "arithmetic", legacy_small)
        assert not legacy_diverges, (
            f"expected the legacy predicate to OVER-minimize to an agreeing case, "
            f"but {legacy_small!r} still diverges")
        print(f"[legacy] minimized {seed!r} -> {legacy_small!r} "
              f"(diverges={legacy_diverges})  <- over-minimized, signal lost")

        # ── The FIX: divergence-preserving predicate keeps the divergence ──────
        def diverge_pred(s):
            if s == "":
                return False
            return fz.diverges(parser, fake, "arithmetic", s)

        fixed_small = fz.minimize(parser, seed, diverge_pred)
        fixed_diverges = fz.diverges(parser, fake, "arithmetic", fixed_small)
        assert fixed_diverges, (
            f"divergence-preserving minimize must keep the divergence, but "
            f"{fixed_small!r} agrees")
        assert "*" in fixed_small, (
            f"the preserved divergence requires a '*', but got {fixed_small!r}")
        print(f"[fixed]  minimized {seed!r} -> {fixed_small!r} "
              f"(diverges={fixed_diverges})  <- divergence preserved")

    print("OK: over-minimization reproduced for the legacy predicate and "
          "prevented by the divergence-preserving predicate.")

    test_seed_range_find_path()


if __name__ == "__main__":
    main()
