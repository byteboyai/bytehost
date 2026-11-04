"""Joint checks must not skip consumers or leave a candidate lock behind."""
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / "joint-build.sh"

class JointBuildTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="joint-build-test-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.consumer = self.root / "dozer"
        self.consumer.mkdir()
        (self.consumer / "Cargo.toml").write_text("[workspace]\nmembers=[]\n")
        (self.consumer / "Cargo.lock").write_text("original lock\n")
        binary = self.root / "bin"
        binary.mkdir()
        cargo = binary / "cargo"
        cargo.write_text('#!/bin/sh\nprintf "candidate lock\\n" > Cargo.lock\nprintf "%s\\n" "$@" > cargo-args.txt\nexit "${CARGO_RESULT:-0}"\n')
        cargo.chmod(0o755)
        self.env = dict(os.environ, CONSUMERS_ROOT=str(self.root), PATH=str(binary)+os.pathsep+os.environ["PATH"])

    def run_check(self, selection="dozer"):
        return subprocess.run(["bash", str(SCRIPT), selection], env=self.env, capture_output=True, text=True, timeout=10)

    def test_missing_consumer_is_a_failure(self):
        result = self.run_check("digger")
        self.assertNotEqual(result.returncode, 0)
        self.assertNotIn("联合构建通过", result.stderr)

    def test_success_restores_the_original_lock(self):
        result = self.run_check()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((self.consumer / "Cargo.lock").read_text(), "original lock\n")

    def test_new_git_accounts_crate_is_part_of_the_candidate(self):
        result = self.run_check()
        self.assertEqual(result.returncode, 0, result.stderr)
        arguments = (self.consumer / "cargo-args.txt").read_text()
        self.assertIn('bytehost-git-accounts.path=', arguments)
        self.assertIn('--all-targets', arguments)

    def test_failure_restores_the_original_lock(self):
        self.env["CARGO_RESULT"] = "42"
        result = self.run_check()
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual((self.consumer / "Cargo.lock").read_text(), "original lock\n")
        self.assertNotIn("联合构建通过", result.stderr)

if __name__ == "__main__":
    unittest.main()
