#!/usr/bin/env python3

import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


PROMOTION_SCRIPT = Path(__file__).with_name("promote_local_package.sh")
TARGET = "x86_64-unknown-linux-gnu"
RELEASE_NAME = f"local-test-{TARGET}"


class PromoteLocalPackageTest(unittest.TestCase):
    def test_validation_does_not_change_installation(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            package, checksums = create_candidate(root)
            codex_home = root / "codex-home"

            result = run_script(root, package, checksums, codex_home, promote=False)

            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("Validation only", result.stdout)
            self.assertFalse(codex_home.exists())

    def test_promotion_selects_package_and_schedules_detached_daemon_update(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            package, checksums = create_candidate(root)
            codex_home = root / "codex-home"
            bin_dir = root / "visible-bin"

            result = run_script(root, package, checksums, codex_home, promote=True)

            self.assertEqual(result.returncode, 0, result.stderr)
            standalone = codex_home / "packages/standalone"
            installed = standalone / "releases" / RELEASE_NAME
            self.assertEqual((standalone / "current").resolve(), installed)
            self.assertEqual((bin_dir / "codex").resolve(), installed / "bin/codex")
            self.assertEqual(
                (installed / "bin/codex").read_bytes(),
                (package / "bin/codex").read_bytes(),
            )
            self.assertEqual(os.readlink(installed / "codex"), "bin/codex")
            self.assertEqual(
                (standalone / "selection-history").read_text().splitlines()[:2],
                [RELEASE_NAME, ""],
            )
            scheduled = (root / "systemd-run.args").read_text().splitlines()
            self.assertIn("--on-active=3s", scheduled)
            self.assertEqual(
                scheduled[-6:],
                [
                    str(installed / "bin/codex"),
                    "app-server",
                    "daemon",
                    "update",
                    "--from-cli",
                    "--yes",
                ],
            )


def create_candidate(root: Path) -> tuple[Path, Path]:
    package = root / "candidate"
    (package / "bin").mkdir(parents=True)
    codex = package / "bin/codex"
    codex.write_text(
        "#!/bin/sh\n"
        "if [ \"${1:-}\" = \"--version\" ]; then\n"
        "  echo 'codex-cli 0.0.0'\n"
        "fi\n"
    )
    codex.chmod(0o755)
    host = package / "bin/codex-code-mode-host"
    host.write_text("#!/bin/sh\nexit 0\n")
    host.chmod(0o755)
    (package / "codex-package.json").write_text(
        json.dumps(
            {
                "layoutVersion": 1,
                "version": "0.0.0",
                "target": TARGET,
                "variant": "codex",
                "entrypoint": "bin/codex",
                "resourcesDir": "codex-resources",
                "pathDir": "codex-path",
            }
        )
    )
    checksums = root / "SHA256SUMS"
    checksums.write_text(
        f"{sha256(codex)}  bin/codex\n"
        f"{sha256(host)}  bin/codex-code-mode-host\n"
    )
    return package, checksums


def sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def run_script(
    root: Path,
    package: Path,
    checksums: Path,
    codex_home: Path,
    *,
    promote: bool,
) -> subprocess.CompletedProcess[str]:
    fake_bin = root / "fake-bin"
    fake_bin.mkdir()
    systemd_run = fake_bin / "systemd-run"
    systemd_run.write_text(
        "#!/bin/sh\n"
        "printf '%s\\n' \"$@\" > \"$PROMOTION_TEST_SYSTEMD_LOG\"\n"
    )
    systemd_run.chmod(0o755)
    command = [
        str(PROMOTION_SCRIPT),
        "--package-dir",
        str(package),
        "--release-name",
        RELEASE_NAME,
        "--checksums",
        str(checksums),
        "--codex-home",
        str(codex_home),
        "--bin-dir",
        str(root / "visible-bin"),
    ]
    if promote:
        command.append("--promote")
    env = os.environ.copy()
    env["PATH"] = f"{fake_bin}:{env['PATH']}"
    env["PROMOTION_TEST_SYSTEMD_LOG"] = str(root / "systemd-run.args")
    return subprocess.run(command, text=True, capture_output=True, env=env, check=False)


if __name__ == "__main__":
    unittest.main()
