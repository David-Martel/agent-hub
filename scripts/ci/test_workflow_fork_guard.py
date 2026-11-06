"""Fork code must never reach agent-hub's self-hosted runners or secrets.

Protects: the persistent self-hosted runners, and any repository secret, of this
PUBLIC repository.
Detects: a `pull_request` job on a self-hosted label, or one that reads
`secrets.*`, without the job-level same-repository guard; `pull_request_target`
anywhere; `issue_comment` reaching a self-hosted job; and a `workflow_run` job
that checks out code on a self-hosted runner.
Needs: the checked-in workflow files only. Stdlib only, because this suite also
runs on hosted Windows without PyYAML.
Breadcrumb: .github/workflows/ci.yml (job `if:` guards); git-guard's
tests/37-workflow-fork-guard.t.sh holds the same contract for git-guard.

Why a guard even though the fork-PR approval policy is `all_external_contributors`:
approval is a human click on a diff, and after it GitHub runs the fork's code on
whatever runner the job names. The guard keeps fork pull requests on
GitHub-hosted runners only.
"""

from __future__ import annotations

import re
import unittest
from pathlib import Path

WORKFLOWS = Path(__file__).resolve().parents[2] / ".github" / "workflows"

GUARD = "github.event.pull_request.head.repo.full_name == github.repository"
# A conjunct of a job `if:` that keeps the job off fork pull requests: the
# same-repo guard as a disjunction, or an event test that excludes pull_request.
SAFE_CONJUNCTS = {
    f"github.event_name != 'pull_request' || {GUARD}",
    f"github.event.pull_request == null || {GUARD}",
    "github.event_name != 'pull_request'",
    "github.event_name == 'workflow_dispatch'",
    "github.event_name == 'push'",
    "github.event_name == 'schedule'",
}
UNGUARDED = "lacks the same-repo guard"


def _strip_expr(text: str) -> str:
    text = " ".join(text.split())
    if text.startswith("${{") and text.endswith("}}"):
        text = text[3:-2].strip()
    return text


def _split_top(expr: str, op: str) -> list[str]:
    """Split on a top-level operator and drop one layer of enclosing parentheses."""
    parts, depth, start, i = [], 0, 0, 0
    while i < len(expr):
        ch = expr[i]
        if ch == "(":
            depth += 1
        elif ch == ")":
            depth -= 1
        elif expr.startswith(op, i) and depth == 0:
            parts.append(expr[start:i])
            start = i + len(op)
            i += len(op) - 1
        i += 1
    parts.append(expr[start:])
    return [_unwrap(part) for part in parts]


def _unwrap(part: str) -> str:
    """Drop parentheses only when one pair encloses the whole expression.

    `(a || b) && c()` starts with `(` and ends with `)` but is not enclosed.
    """
    part = part.strip()
    while part.startswith("(") and part.endswith(")"):
        depth = 0
        for k, ch in enumerate(part):
            depth += ch == "("
            depth -= ch == ")"
            if depth == 0 and k < len(part) - 1:
                return part
        part = part[1:-1].strip()
    return part


def is_guarded(if_expr: str) -> bool:
    """True only when the whole condition is false for every fork pull request.

    `||` binds looser than `&&`, so a top-level disjunction is guarded only when it
    IS one of the complete guard forms: any other branch could admit a fork on its
    own (codex review of #237). Otherwise one conjunct must be a guard.
    """
    expr = _unwrap(_strip_expr(if_expr))
    if len(_split_top(expr, "||")) > 1:
        return expr in SAFE_CONJUNCTS
    return any(c in SAFE_CONJUNCTS for c in _split_top(expr, "&&"))


def _indent(line: str) -> int:
    return len(line) - len(line.lstrip())


def _block_value(lines: list[str], i: int, indent: int) -> tuple[str, int]:
    """A scalar that may continue on deeper lines (plain, or `>`/`|` folded)."""
    _, _, rest = lines[i].strip().partition(":")
    rest = rest.strip()
    vals = [] if rest in {">", "|", ">-", "|-", ""} else [rest]
    j = i + 1
    while j < len(lines) and (not lines[j].strip() or _indent(lines[j]) > indent):
        if lines[j].strip() and not lines[j].strip().startswith("#"):
            vals.append(lines[j].strip())
        j += 1
    # YAML also allows a block sequence at the key's own indent (`runs-on:` then
    # `- self-hosted` on the same column). Skipping it read runs-on as empty and
    # passed a self-hosted job (codex review of #237).
    if not rest:
        while (
            j < len(lines)
            and _indent(lines[j]) == indent
            and lines[j].lstrip().startswith("- ")
        ):
            vals.append(lines[j].strip())
            j += 1
    return " ".join(vals), j


def _trigger_names(rest: str) -> set[str]:
    names = (t.strip(" []'\"") for t in rest.split(","))
    return {t for t in names if t}


def parse_workflow(text: str) -> tuple[set[str], dict[str, dict[str, str]]]:
    """Triggers and, per job, its `runs-on`, `if` and full text.

    Fits this repository's style: two-space indentation, `on:` inline or as a
    mapping, and jobs under a top-level `jobs:`.
    """
    lines = text.splitlines()
    triggers: set[str] = set()
    jobs: dict[str, dict[str, str]] = {}
    i = 0
    while i < len(lines):
        line = lines[i]
        if re.match(r"^(on|\"on\"|'on'):", line):
            triggers |= _trigger_names(line.split(":", 1)[1].strip())
            j = i + 1
            while j < len(lines) and (not lines[j].strip() or lines[j][0] == " "):
                m = re.match(r"^  ([A-Za-z_]+):", lines[j])
                m = m or re.match(r"^  - ([A-Za-z_]+)\s*$", lines[j])
                if m:
                    triggers.add(m.group(1))
                j += 1
            i = j
            continue
        if line.startswith("jobs:"):
            j = i + 1
            current = None
            while j < len(lines) and (not lines[j].strip() or lines[j][0] == " "):
                m = re.match(r"^  ([A-Za-z0-9_-]+):\s*$", lines[j])
                if m:
                    current = m.group(1)
                    jobs[current] = {"runs-on": "", "if": "", "text": ""}
                    j += 1
                    continue
                if current is not None:
                    jobs[current]["text"] += lines[j] + "\n"
                    k = re.match(r"^    (runs-on|if):", lines[j])
                    if k:
                        value, nxt = _block_value(lines, j, 4)
                        jobs[current][k.group(1)] = value
                        for extra in lines[j + 1 : nxt]:
                            jobs[current]["text"] += extra + "\n"
                        j = nxt
                        continue
                j += 1
            i = j
            continue
        i += 1
    return triggers, jobs


def is_self_hosted(runs_on: str) -> bool:
    # An expression may resolve to a self-hosted label: treat it as self-hosted.
    return "self-hosted" in runs_on or "${{" in runs_on


def violations(name: str, text: str) -> list[str]:
    triggers, jobs = parse_workflow(text)
    found = []
    if "pull_request_target" in triggers:
        found.append(f"{name}: pull_request_target runs fork code with base secrets")
    for job, spec in jobs.items():
        where = f"{name}:{job}"
        self_hosted = is_self_hosted(spec["runs-on"])
        if self_hosted and "issue_comment" in triggers:
            found.append(f"{where}: issue_comment starts a self-hosted job")
        body = spec["text"]
        runs_code = "actions/checkout" in body or re.search(r"uses:\s*\./", body)
        if self_hosted and "workflow_run" in triggers and runs_code:
            found.append(f"{where}: workflow_run checks out code on self-hosted")
        exposed = self_hosted or "secrets." in body
        if "pull_request" in triggers and exposed and not is_guarded(spec["if"]):
            found.append(f"{where}: pull_request job {UNGUARDED}")
    return found


HEAD = "on:\n  pull_request:\n  push:\njobs:\n"
UNSAFE = {
    "unguarded self-hosted": HEAD + "  a:\n    runs-on: [self-hosted, Linux]\n",
    "expression runs-on": HEAD + "  a:\n    runs-on: ${{ fromJSON(matrix.r) }}\n",
    "secret on hosted": HEAD
    + "  a:\n    runs-on: ubuntu-latest\n    steps:\n"
    + "      - run: echo ${{ secrets.T }}\n",
    "or-weakened guard": HEAD
    + "  a:\n    runs-on: [self-hosted]\n"
    + "    if: ${{ always() || github.event_name != 'pull_request' }}\n",
    "pull_request_target": "on:\n  pull_request_target:\njobs:\n"
    + "  a:\n    runs-on: ubuntu-latest\n",
    "issue_comment": "on:\n  issue_comment:\njobs:\n"
    + "  a:\n    runs-on: [self-hosted]\n",
    "or after and": HEAD
    + "  a:\n    runs-on: [self-hosted]\n"
    + "    if: github.event_name == 'pull_request' || github.event_name == 'push'"
    + " && github.event_name != 'pull_request'\n",
    "sequence at key indent": HEAD
    + "  a:\n    runs-on:\n    - self-hosted\n    - Linux\n",
    "workflow_run checkout": "on:\n  workflow_run:\njobs:\n"
    + "  a:\n    runs-on: [self-hosted]\n    steps:\n"
    + "      - uses: actions/checkout@v7\n",
}
GUARDED = (
    HEAD
    + "  a:\n    runs-on: [self-hosted, Linux]\n"
    + f"    if: github.event_name != 'pull_request' || {GUARD}\n"
    + "  b:\n    runs-on: [self-hosted]\n"
    + "    if: ${{ github.event_name != 'pull_request' && !cancelled() }}\n"
    + "  c:\n    runs-on: ubuntu-latest\n"
    + "  d:\n    runs-on:\n    - self-hosted\n"
    + f"    if: (github.event_name != 'pull_request' || {GUARD}) && !cancelled()\n"
)


class WorkflowForkGuardTest(unittest.TestCase):
    def test_fork_code_reaches_no_self_hosted_runner_or_secret(self) -> None:
        """Protects/Detects: see the module docstring. Lists each unguarded job."""
        files = sorted(WORKFLOWS.glob("*.yml")) + sorted(WORKFLOWS.glob("*.yaml"))
        self.assertTrue(files, f"no workflows under {WORKFLOWS}")
        problems = []
        for path in files:
            problems += violations(path.name, path.read_text(encoding="utf-8"))
        self.assertEqual(problems, [], "\n".join(problems))

    def test_checker_refuses_each_unsafe_shape_and_passes_a_guarded_one(self) -> None:
        """Protects: the checker itself. Detects: a check that cannot fail.

        Each unsafe fixture must be refused on its own (negative cases); the
        guarded fixture must pass (positive control).
        """
        for label, text in UNSAFE.items():
            with self.subTest(label):
                self.assertTrue(violations("fixture.yml", text), label)
        self.assertEqual(violations("fixture.yml", GUARDED), [])


if __name__ == "__main__":
    unittest.main()
