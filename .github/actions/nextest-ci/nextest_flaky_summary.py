# Vendored from David-Martel/vigil-utils .github/actions/nextest-ci
# at 2b9e990e5804cb89f3f281900bf0d2959e060afd (PR #1127), synced by hand.
# ruff: noqa: E501 -- the body stays byte-identical to upstream (120 columns there).
"""Write the flaky tests found in a cargo-nextest JUnit file to a Markdown summary.

nextest records a test that failed and then passed on retry as a passing testcase that
carries ``flakyFailure`` (failed) or ``flakyError`` (execution error) children.
Usage: nextest_flaky_summary.py [--require] [--allow-flaky] JUNIT_XML [SUMMARY_MD].
Prints the flaky test names, appends a table to SUMMARY_MD when given, emits one annotation per
flaky test, and exits 1 when any test was flaky. The retries exist to name the race ("passed on
attempt 2"), not to merge it: `cargo test` has no retries, so a race already failed the job, and
nextest must not make the gate more lenient. --allow-flaky is the explicit opt-in to report
flaky tests as warnings and exit 0.
A missing or malformed file is reported as such; with --require (passed by the action after a
green run) it also exits 1, because a green run that left no report means the flaky check has
stopped reading anything.
"""

from __future__ import annotations

import sys
import xml.etree.ElementTree as ET
from pathlib import Path

FLAGS = ("--require", "--allow-flaky")


def flaky_tests(junit: Path) -> list[tuple[str, int]]:
    """Return (name, failed attempts) for every testcase with flakyFailure or flakyError children."""
    found = []
    for case in ET.parse(junit).getroot().iter("testcase"):
        attempts = len(case.findall("flakyFailure")) + len(case.findall("flakyError"))
        if attempts:
            found.append((f"{case.get('classname', '')}::{case.get('name', '')}", attempts))
    return sorted(found)


def render(found: list[tuple[str, int]]) -> str:
    if not found:
        return "### nextest flaky tests\n\nNone: every test passed on its first attempt.\n"
    rows = "\n".join(f"| `{name}` | {n} |" for name, n in found)
    return f"### nextest flaky tests\n\n| test | failed attempts before pass |\n|---|---|\n{rows}\n"


def main(argv: list[str]) -> int:
    require = "--require" in argv
    allow_flaky = "--allow-flaky" in argv
    argv = [a for a in argv if a not in FLAGS]
    if not argv:
        print("usage: nextest_flaky_summary.py [--require] [--allow-flaky] JUNIT_XML [SUMMARY_MD]", file=sys.stderr)
        return 0
    junit = Path(argv[0])
    found: list[tuple[str, int]] = []
    rc = 0
    try:
        found = flaky_tests(junit)
        text = render(found)
    except (OSError, ET.ParseError) as exc:
        text = f"### nextest flaky tests\n\nNo usable JUnit file at `{junit}`: {exc}\n"
        if require:
            print(f"::error title=nextest JUnit missing::{junit}: {exc}")
            rc = 1
    if found and not allow_flaky:
        text += "\nFlaky tests fail this job (pass --allow-flaky, or `fail-on-flaky: false`, to opt out).\n"
        rc = 1
    print(text)
    if len(argv) > 1:
        with open(argv[1], "a", encoding="utf-8") as fh:
            fh.write(text + "\n")
    level = "warning" if allow_flaky else "error"
    for name, n in found:
        print(f"::{level} title=flaky test::{name} failed {n} time(s) before passing")
    return rc


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
