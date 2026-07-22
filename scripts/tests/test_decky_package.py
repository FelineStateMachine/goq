from __future__ import annotations

import hashlib
import json
import os
import pathlib
import subprocess
import sys
import tempfile
import unittest
import zipfile


REPOSITORY = pathlib.Path(__file__).resolve().parents[2]
PACKAGER = REPOSITORY / "scripts/package-decky-plugin.py"
COMMIT = "a" * 40


class DeckyPackageTests(unittest.TestCase):
    def build(self, output: pathlib.Path) -> pathlib.Path:
        environment = dict(os.environ)
        environment["GOQ_SOURCE_COMMIT"] = COMMIT
        subprocess.run(
            [sys.executable, str(PACKAGER), str(output)],
            cwd=REPOSITORY,
            env=environment,
            check=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=10,
        )
        return output / "goq-sigil-v0.1.0.zip"

    def test_archive_is_deterministic_bounded_and_provenance_bound(self):
        with tempfile.TemporaryDirectory() as first, tempfile.TemporaryDirectory() as second:
            first_archive = self.build(pathlib.Path(first))
            second_archive = self.build(pathlib.Path(second))
            self.assertEqual(
                hashlib.sha256(first_archive.read_bytes()).digest(),
                hashlib.sha256(second_archive.read_bytes()).digest(),
            )
            with zipfile.ZipFile(first_archive) as archive:
                names = archive.namelist()
                self.assertEqual(names, sorted(names))
                self.assertIn("goq-sigil/dist/index.js", names)
                self.assertIn("goq-sigil/main.py", names)
                self.assertFalse(any("tests/" in name for name in names))
                self.assertFalse(any(name.endswith((".toml", ".key")) for name in names))
                provenance = json.loads(
                    archive.read("goq-sigil/provenance.json").decode("utf-8")
                )
                self.assertEqual(provenance["source_commit"], COMMIT)
                self.assertEqual(provenance["compatibility"]["appliance_status_schema"], 2)


if __name__ == "__main__":
    unittest.main()
