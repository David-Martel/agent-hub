"""Pin the isolation and placement invariants of the ms01 agent-hub Docker runner.

What it protects: the owner approved a Docker-capable agent-hub runner on
minisforum-ms01 (2026-10-29) only as an isolated design. The job's Docker socket must be
the dedicated `ci-agenthub` account's own Podman socket, never the shared `vigil-ci`
account's, which would hand agent-hub jobs control of other repositories' runner
containers. The runner must carry exactly the placement-table labels, without
`windows-cross` until that image is qualified. It must run at idle priority on cores
that do not overlap the vigil-spark runner's 8-11, and the same bound must cover the
sibling containers jobs start.

What it detects: a drifted label set, a socket path from another account, a lost keep-id
mapping, a priority or core change in one file but not the others, and a registration
guard that accepts other labels.

What it needs: only the files under config/fleet/minisforum-ms01-agenthub/; no host,
network or Podman.

Breadcrumb: config/fleet/minisforum-ms01-agenthub/README.md and
docs/fleet-build-runners.md.
"""

import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
CARRIER = ROOT / "config/fleet/minisforum-ms01-agenthub"
PLACEMENT_LABELS = ["self-hosted", "Linux", "X64", "docker", "minisforum-ms01"]
VIGIL_SPARK_EXTRA_CORES = set(range(8, 12))  # vigil-spark runner 48 on ms01


def env_values(text):
    values = {}
    for line in text.splitlines():
        if line and not line.startswith("#") and "=" in line:
            key, value = line.split("=", 1)
            values[key] = value
    return values


def cores(spec):
    result = set()
    for part in spec.split(","):
        low, _, high = part.partition("-")
        result.update(range(int(low), int(high or low) + 1))
    return result


def violations(env_text, unit_text, setup_text, register_text):
    """Return every broken invariant; an empty list means the carrier files agree."""
    found = []
    env = env_values(env_text)
    if env.get("REPO") != "David-Martel/agent-hub":
        found.append("instance REPO is not David-Martel/agent-hub")
    labels = env.get("RUNNER_LABELS", "").split(",")
    if labels != PLACEMENT_LABELS:
        found.append(f"instance labels {labels} differ from the placement table")
    if "windows-cross" in labels:
        found.append("windows-cross needs the image-qualification receipt first")
    instance_cores = cores(env.get("CORES", "0-19"))
    if instance_cores & VIGIL_SPARK_EXTRA_CORES:
        found.append("instance cores overlap the vigil-spark runner's 8-11")

    if "-v %t/podman/podman.sock:/var/run/docker.sock" not in unit_text:
        found.append(
            "unit does not mount the account's own Podman socket as the Docker socket"
        )
    unit_directives = "\n".join(
        line for line in unit_text.splitlines() if not line.lstrip().startswith("#")
    )
    if re.search(
        r"/run/user/2001|vigil-ci|/var/run/docker\.sock:/var/run", unit_directives
    ):
        found.append("unit references another account's or the host's engine socket")
    if "--userns=keep-id:uid=1000,gid=1000" not in unit_text:
        found.append("unit lost the keep-id mapping the job needs to open the socket")
    for setting in ("Nice=19", "CPUSchedulingPolicy=idle", "IOSchedulingClass=idle"):
        if not re.search(rf"^{re.escape(setting)}$", unit_text, re.MULTILINE):
            found.append(f"unit lacks {setting}")
    unit_cores = re.search(r"^Environment=CORES=(\S+)$", unit_text, re.MULTILINE)
    if not unit_cores or cores(unit_cores.group(1)) != instance_cores:
        found.append("unit default CORES differs from the instance")

    if not re.search(r"^ACCOUNT=ci-agenthub$", setup_text, re.MULTILINE):
        found.append("setup-host.sh does not create the dedicated ci-agenthub account")
    setup_cores = re.search(r"^CORES=(\S+)", setup_text, re.MULTILINE)
    if not setup_cores or cores(setup_cores.group(1)) != instance_cores:
        found.append("slice/podman.service cores differ from the instance")
    for setting in (
        "AllowedCPUs=$CORES",
        "Nice=19",
        "CPUSchedulingPolicy=idle",
        "CPUAffinity=$CORES",
    ):
        if setting not in setup_text:
            found.append(
                f"setup-host.sh does not bound sibling containers with {setting}"
            )

    if f'[ "$labels" = {",".join(PLACEMENT_LABELS)} ]' not in register_text:
        found.append("register.sh does not refuse labels outside the placement table")
    if '[ "$REPO" = David-Martel/agent-hub ]' not in register_text:
        found.append("register.sh does not refuse other repositories")
    if "--userns=keep-id:uid=1000,gid=1000" not in register_text:
        found.append("register.sh uses a different user namespace from the unit")
    return found


def carrier_files():
    return (
        (CARRIER / "agenthub-docker-1.env").read_text(),
        (CARRIER / "ci-agenthub-runner@.service").read_text(),
        (CARRIER / "setup-host.sh").read_text(),
        (CARRIER / "register.sh").read_text(),
    )


class Ms01AgentHubRunnerTests(unittest.TestCase):
    def test_committed_carrier_files_hold_every_invariant(self):
        self.assertEqual(violations(*carrier_files()), [])

    def test_each_invariant_can_fail(self):
        # Positive controls: one mutation per invariant must be reported, or the
        # check cannot fail.
        env, unit, setup, register = carrier_files()
        mutations = {
            "windows-cross": (
                env.replace("minisforum-ms01\n", "minisforum-ms01,windows-cross\n"),
                unit,
                setup,
                register,
            ),
            "8-11": (env.replace("CORES=4-7", "CORES=8-11"), unit, setup, register),
            "own Podman socket": (
                env,
                unit.replace(
                    "%t/podman/podman.sock", "/run/user/2001/podman/podman.sock"
                ),
                setup,
                register,
            ),
            "keep-id": (
                env,
                unit.replace("--userns=keep-id:uid=1000,gid=1000", "--userns=auto"),
                setup,
                register,
            ),
            "Nice=19": (env, unit.replace("Nice=19", "Nice=0"), setup, register),
            "ci-agenthub account": (
                env,
                unit,
                setup.replace("ACCOUNT=ci-agenthub", "ACCOUNT=vigil-ci"),
                register,
            ),
            "AllowedCPUs": (
                env,
                unit,
                setup.replace("AllowedCPUs=$CORES", ""),
                register,
            ),
            "refuse labels": (
                env,
                unit,
                setup,
                register.replace(
                    "docker,minisforum-ms01 ]", "docker,minisforum-ms01,windows-cross ]"
                ),
            ),
        }
        for expected, files in mutations.items():
            with self.subTest(expected):
                self.assertTrue(
                    any(expected in message for message in violations(*files)),
                    f"mutation for {expected!r} went unreported",
                )


if __name__ == "__main__":
    unittest.main()
