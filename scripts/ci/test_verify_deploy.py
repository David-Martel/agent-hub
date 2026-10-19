"""Wire cloud/agentbus/tools/test_verify_deploy.py into scripts/ci discovery."""

from __future__ import annotations

import importlib.util
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SUITE = ROOT / "cloud/agentbus/tools/test_verify_deploy.py"
SPEC = importlib.util.spec_from_file_location("cloud_verify_deploy_tests", SUITE)
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)

VerifyDeployTests = MODULE.VerifyDeployTests


if __name__ == "__main__":
    unittest.main()
