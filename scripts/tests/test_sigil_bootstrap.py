#!/usr/bin/env python3

from __future__ import annotations

import base64
import hashlib
import importlib.util
import io
import os
import pathlib
import re
import subprocess
import tarfile
import tempfile
import unittest


REPO_DIR = pathlib.Path(__file__).resolve().parents[2]
BOOTSTRAP_PATH = REPO_DIR / "website" / "install-sigil"
MODULE_PATH = REPO_DIR / "scripts" / "verify-sigil-bootstrap.py"
SPEC = importlib.util.spec_from_file_location("sigil_bootstrap_verifier", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
VERIFIER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(VERIFIER)

PUBLISHER_KEY = base64.b64encode(b"Ed" + b"A" * 8 + b"B" * 32).decode("ascii")
OTHER_PUBLISHER_KEY = base64.b64encode(b"Ed" + b"C" * 8 + b"D" * 32).decode("ascii")
RELEASE_TAG = "v1.2.3-alpha.1"
ASSET_NAME = f"sigil-{RELEASE_TAG}-bazzite-x86_64.tar.gz"
ASSET_BASE = (
    "https://github.com/FelineStateMachine/goq/releases/download/"
    f"{RELEASE_TAG}/{ASSET_NAME}"
)
MINISIGN_VERSION = "0.12"
MINISIGN_ASSET_NAME = f"minisign-{MINISIGN_VERSION}-linux.tar.gz"
MINISIGN_URL = (
    "https://github.com/jedisct1/minisign/releases/download/0.12/"
    f"{MINISIGN_ASSET_NAME}"
)
MINISIGN_SHA256 = (
    "9a599b48ba6eb7b1e80f12f36b94ceca7c00b7a5173c95c3efc88d9822957e73"
)
ZERO_SHA256 = "0" * 64
MINISIGN_CORE_MEMBERS = {
    "minisign-linux/",
    "minisign-linux/aarch64/",
    "minisign-linux/x86_64/",
    "minisign-linux/aarch64/minisign",
    "minisign-linux/x86_64/minisign",
}
MINISIGN_APPLEDOUBLE_MEMBERS = {
    "._minisign-linux",
    "minisign-linux/._aarch64",
    "minisign-linux/._x86_64",
    "minisign-linux/aarch64/._minisign",
    "minisign-linux/x86_64/._minisign",
}


def bootstrap_with_pins(
    source: str,
    *,
    publisher_key: str = PUBLISHER_KEY,
    release_tag: str = RELEASE_TAG,
    minisign_sha256: str | None = None,
) -> str:
    source, publisher_replacements = re.subn(
        r'^readonly publisher_key="[^"\n]*"$',
        f'readonly publisher_key="{publisher_key}"',
        source,
        count=1,
        flags=re.MULTILINE,
    )
    source, tag_replacements = re.subn(
        r'^readonly release_tag="[^"\n]*"$',
        f'readonly release_tag="{release_tag}"',
        source,
        count=1,
        flags=re.MULTILINE,
    )
    if minisign_sha256 is not None:
        source, minisign_replacements = re.subn(
            r'^readonly minisign_sha256="[^"\n]*"$',
            f'readonly minisign_sha256="{minisign_sha256}"',
            source,
            count=1,
            flags=re.MULTILINE,
        )
    else:
        minisign_replacements = 1
    if (
        publisher_replacements != 1
        or tag_replacements != 1
        or minisign_replacements != 1
    ):
        raise AssertionError("bootstrap fixture pins must each occur exactly once")
    return source


class SigilBootstrapContractTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.bootstrap_source = BOOTSTRAP_PATH.read_text(encoding="utf-8")

    def test_closed_channel_requires_all_three_sentinels(self) -> None:
        closed = bootstrap_with_pins(
            self.bootstrap_source,
            publisher_key="unconfigured",
            release_tag="unconfigured",
        )
        self.assertEqual(
            VERIFIER.verify_contract(closed, "unconfigured\n"),
            "closed",
        )
        partial_states = [
            (
                bootstrap_with_pins(self.bootstrap_source, release_tag="unconfigured"),
                "unconfigured\n",
            ),
            (
                bootstrap_with_pins(self.bootstrap_source, publisher_key="unconfigured"),
                "unconfigured\n",
            ),
            (closed, f"untrusted comment: fixture\n{PUBLISHER_KEY}\n"),
            (bootstrap_with_pins(self.bootstrap_source), "unconfigured\n"),
        ]
        for bootstrap, public_key in partial_states:
            with self.subTest(bootstrap=bootstrap[:80], public_key=public_key[:30]):
                with self.assertRaisesRegex(VERIFIER.VerificationError, "partially configured"):
                    VERIFIER.verify_contract(bootstrap, public_key)

    def test_open_channel_binds_exact_repository_key_and_release_tag(self) -> None:
        public_key = f"untrusted comment: Sigil fixture key\n{PUBLISHER_KEY}\n"
        self.assertEqual(
            VERIFIER.verify_contract(bootstrap_with_pins(self.bootstrap_source), public_key),
            "open",
        )
        with self.assertRaisesRegex(VERIFIER.VerificationError, "does not match"):
            VERIFIER.verify_contract(
                bootstrap_with_pins(self.bootstrap_source),
                f"untrusted comment: wrong\n{OTHER_PUBLISHER_KEY}\n",
            )

    def test_malformed_pins_and_repository_drift_fail_closed(self) -> None:
        configured = bootstrap_with_pins(self.bootstrap_source)
        public_key = f"untrusted comment: fixture\n{PUBLISHER_KEY}\n"
        for malformed_tag in (
            "main",
            "v1",
            "v1.2",
            "v1.2.3/asset",
            "v1.2.3+build",
        ):
            with self.subTest(tag=malformed_tag):
                with self.assertRaisesRegex(VERIFIER.VerificationError, "tag is malformed"):
                    VERIFIER.verify_contract(
                        bootstrap_with_pins(self.bootstrap_source, release_tag=malformed_tag),
                        public_key,
                    )

        with self.assertRaisesRegex(VERIFIER.VerificationError, "publisher key is malformed"):
            VERIFIER.verify_contract(
                bootstrap_with_pins(self.bootstrap_source, publisher_key="not-a-key"),
                public_key,
            )
        with self.assertRaisesRegex(VERIFIER.VerificationError, "unexpected content"):
            VERIFIER.verify_contract(configured, f"trusted comment: no\n{PUBLISHER_KEY}\n")
        with self.assertRaisesRegex(VERIFIER.VerificationError, "release repository"):
            VERIFIER.verify_contract(
                configured.replace("FelineStateMachine/goq", "attacker/goq", 1),
                public_key,
            )
        with self.assertRaisesRegex(VERIFIER.VerificationError, "exactly once"):
            VERIFIER.verify_contract(
                configured + f'\nreadonly publisher_key="{PUBLISHER_KEY}"\n',
                public_key,
            )

    def test_verifier_provisioning_is_bound_to_reviewed_exact_pins(self) -> None:
        pins = VERIFIER.parse_bootstrap_pins(self.bootstrap_source)
        self.assertEqual(pins["minisign_version"], MINISIGN_VERSION)
        self.assertEqual(pins["minisign_url"], MINISIGN_URL)
        self.assertEqual(pins["minisign_sha256"], MINISIGN_SHA256)

        public_key = "unconfigured\n"
        replacements = {
            'readonly minisign_version="0.12"': 'readonly minisign_version="0.13"',
            f'readonly minisign_url="{MINISIGN_URL}"': (
                'readonly minisign_url="https://example.invalid/minisign.tar.gz"'
            ),
            f'readonly minisign_sha256="{MINISIGN_SHA256}"': (
                f'readonly minisign_sha256="{ZERO_SHA256}"'
            ),
        }
        for original, replacement in replacements.items():
            with self.subTest(pin=original.split("=", 1)[0]):
                mutated = self.bootstrap_source.replace(original, replacement, 1)
                with self.assertRaises(VERIFIER.VerificationError):
                    VERIFIER.verify_contract(mutated, public_key)

        with self.assertRaisesRegex(VERIFIER.VerificationError, "exactly once"):
            VERIFIER.verify_contract(
                self.bootstrap_source
                + f'\nreadonly minisign_sha256="{MINISIGN_SHA256}"\n',
                public_key,
            )

    def test_rejects_structurally_invalid_minisign_public_keys(self) -> None:
        invalid_keys = {
            "wrong algorithm": base64.b64encode(b"ED" + b"A" * 40).decode("ascii"),
            "short payload": base64.b64encode(b"Ed" + b"A" * 39).decode("ascii"),
            "long payload": base64.b64encode(b"Ed" + b"A" * 41).decode("ascii"),
            "invalid alphabet": "RW" + "!" * 54,
            "invalid padding": PUBLISHER_KEY[:-1] + "=",
        }
        for name, key in invalid_keys.items():
            with self.subTest(name=name):
                with self.assertRaisesRegex(VERIFIER.VerificationError, "malformed"):
                    VERIFIER.verify_contract(
                        bootstrap_with_pins(self.bootstrap_source, publisher_key=key),
                        f"untrusted comment: fixture\n{key}\n",
                    )

    def test_current_repository_state_is_a_complete_valid_contract(self) -> None:
        public_key = (REPO_DIR / "release" / "sigil-minisign.pub").read_text(encoding="utf-8")
        self.assertIn(
            VERIFIER.verify_contract(self.bootstrap_source, public_key),
            {"closed", "open"},
        )


class SigilBootstrapBehaviorTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.root = pathlib.Path(self.temporary.name)
        self.tmp_dir = self.root / "tmp"
        self.tmp_dir.mkdir()
        self.fixture_dir = self.root / "assets"
        self.fixture_dir.mkdir()
        self.fake_bin = self.root / "bin"
        self.fake_bin.mkdir()
        self.curl_log = self.root / "curl.log"
        self.stage_marker = self.root / "stage.marker"
        self.forbidden_marker = self.root / "forbidden.marker"
        self.minisign_marker = self.root / "minisign.marker"
        self.bootstrap = self.root / "install-sigil"
        self.write_command_stubs()
        minisign_digest = self.write_minisign_fixture()
        self.write_release_fixture()
        self.bootstrap.write_text(
            bootstrap_with_pins(
                BOOTSTRAP_PATH.read_text(encoding="utf-8"),
                minisign_sha256=minisign_digest,
            ),
            encoding="utf-8",
        )
        self.bootstrap.chmod(0o755)

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def write_stub(self, name: str, source: str) -> None:
        path = self.fake_bin / name
        path.write_text(source, encoding="utf-8")
        path.chmod(0o755)

    def write_command_stubs(self) -> None:
        self.write_stub(
            "uname",
            """#!/usr/bin/env bash
set -euo pipefail
case "${1:-}" in
  -s) printf '%s\n' "${BOOTSTRAP_TEST_OS:-Linux}" ;;
  -m) printf '%s\n' "${BOOTSTRAP_TEST_ARCH:-x86_64}" ;;
  *) exit 64 ;;
esac
""",
        )
        self.write_stub(
            "id",
            """#!/usr/bin/env bash
set -euo pipefail
[[ "${1:-}" == -u ]] || exit 64
printf '%s\n' "${BOOTSTRAP_TEST_UID:-1000}"
""",
        )
        self.write_stub(
            "grep",
            """#!/usr/bin/env bash
set -euo pipefail
if [[ "${*: -1}" == /etc/os-release ]]; then
  [[ "${BOOTSTRAP_TEST_BAZZITE:-1}" == 1 ]]
  exit
fi
exec /usr/bin/grep "$@"
""",
        )
        self.write_stub(
            "curl",
            """#!/usr/bin/env bash
set -euo pipefail
output=''
url=''
proto=''
tls=''
fail=0
silent=0
show_error=0
location=0
while (( $# )); do
  case "$1" in
    --proto) proto="$2"; shift 2 ;;
    --tlsv1.2) tls=1; shift ;;
    --fail) fail=1; shift ;;
    --silent) silent=1; shift ;;
    --show-error) show_error=1; shift ;;
    --location) location=1; shift ;;
    --retry|--output) option="$1"; value="$2"; shift 2; [[ "$option" == --output ]] && output="$value" ;;
    --retry-all-errors) shift ;;
    https://*) url="$1"; shift ;;
    *) exit 64 ;;
  esac
done
[[ "$proto" == =https && "$tls" == 1 && "$fail" == 1 && "$silent" == 1 \
  && "$show_error" == 1 && "$location" == 1 && -n "$output" && -n "$url" ]] || exit 64
name="${url##*/}"
printf '%s\n' "$url" >>"$BOOTSTRAP_CURL_LOG"
if [[ -n "${BOOTSTRAP_TEST_CURL_PARTIAL:-}" && "$name" == *"$BOOTSTRAP_TEST_CURL_PARTIAL" ]]; then
  printf partial >"$output"
  exit 22
fi
cp -- "$BOOTSTRAP_FIXTURE_DIR/$name" "$output"
""",
        )
        self.write_stub(
            "minisign",
            """#!/usr/bin/env bash
set -euo pipefail
printf 'PATH minisign invoked\n' >>"$BOOTSTRAP_FORBIDDEN_MARKER"
exit 99
""",
        )
        self.write_stub(
            "systemctl",
            """#!/usr/bin/env bash
set -euo pipefail
printf 'systemctl %s\n' "$*" >>"$BOOTSTRAP_FORBIDDEN_MARKER"
exit 99
""",
        )

    def write_minisign_fixture(self, *, variant: str = "valid") -> str:
        archive_path = self.fixture_dir / MINISIGN_ASSET_NAME
        verifier = b"""#!/usr/bin/env bash
set -euo pipefail
printf 'invoked\\n' >>"$BOOTSTRAP_MINISIGN_MARKER"
[[ "${BOOTSTRAP_TEST_MINISIGN:-ok}" == ok ]] || exit 1
message=''
signature=''
key=''
while (( $# )); do
  case "$1" in
    -Vm) message="$2"; shift 2 ;;
    -x) signature="$2"; shift 2 ;;
    -P) key="$2"; shift 2 ;;
    *) exit 64 ;;
  esac
done
[[ -s "$message" && -s "$signature" && "$key" == "$BOOTSTRAP_EXPECTED_KEY" ]]
"""
        with tarfile.open(archive_path, "w:gz") as archive:
            self.add_archive_directory(archive, "minisign-linux/")
            self.add_archive_directory(archive, "minisign-linux/aarch64/")
            self.add_archive_directory(archive, "minisign-linux/x86_64/")
            self.add_archive_file(archive, "._minisign-linux", b"metadata\n", 0o644)
            self.add_archive_file(
                archive, "minisign-linux/._aarch64", b"metadata\n", 0o644
            )
            self.add_archive_file(
                archive, "minisign-linux/._x86_64", b"metadata\n", 0o644
            )
            self.add_archive_file(
                archive,
                "minisign-linux/aarch64/._minisign",
                b"metadata\n",
                0o644,
            )
            self.add_archive_file(
                archive,
                "minisign-linux/x86_64/._minisign",
                b"metadata\n",
                0o644,
            )
            self.add_archive_file(
                archive,
                "minisign-linux/aarch64/minisign",
                b"fixture aarch64 binary\n",
                0o755,
            )
            if variant == "symlink":
                link = tarfile.TarInfo("minisign-linux/x86_64/minisign")
                link.type = tarfile.SYMTYPE
                link.linkname = "../aarch64/minisign"
                archive.addfile(link)
            else:
                self.add_archive_file(
                    archive,
                    "minisign-linux/x86_64/minisign",
                    verifier,
                    0o755,
                )
            if variant == "unexpected":
                self.add_archive_file(
                    archive,
                    "minisign-linux/README",
                    b"unexpected\n",
                    0o644,
                )
        return hashlib.sha256(archive_path.read_bytes()).hexdigest()

    def repin_minisign_fixture(self, *, variant: str) -> None:
        digest = self.write_minisign_fixture(variant=variant)
        self.bootstrap.write_text(
            bootstrap_with_pins(
                BOOTSTRAP_PATH.read_text(encoding="utf-8"),
                minisign_sha256=digest,
            ),
            encoding="utf-8",
        )
        self.bootstrap.chmod(0o755)

    def write_release_fixture(
        self,
        *,
        variant: str = "valid",
        valid_checksum: bool = True,
    ) -> None:
        archive_path = self.fixture_dir / ASSET_NAME
        with tarfile.open(archive_path, "w:gz") as archive:
            self.add_archive_file(
                archive,
                "payload/stage-this-release.sh",
                b"#!/usr/bin/env bash\nset -euo pipefail\nprintf 'staged\\n' >>\"$BOOTSTRAP_STAGE_MARKER\"\n",
                0o755,
            )
            self.add_archive_file(
                archive,
                "payload/install-bazzite-package.sh",
                b"#!/usr/bin/env bash\nset -euo pipefail\nprintf 'forbidden\\n' >>\"$BOOTSTRAP_FORBIDDEN_MARKER\"\n",
                0o755,
            )
            if variant == "escape":
                self.add_archive_file(archive, "../escape", b"no\n", 0o644)
            if variant == "symlink":
                link = tarfile.TarInfo("payload/unsafe-link")
                link.type = tarfile.SYMTYPE
                link.linkname = "stage-this-release.sh"
                archive.addfile(link)

        digest = hashlib.sha256(archive_path.read_bytes()).hexdigest()
        if not valid_checksum:
            digest = "0" * 64
        (self.fixture_dir / f"{ASSET_NAME}.sha256").write_text(
            f"{digest}  {ASSET_NAME}\n", encoding="utf-8"
        )
        (self.fixture_dir / f"{ASSET_NAME}.minisig").write_text(
            "fixture signature\n", encoding="utf-8"
        )

    @staticmethod
    def add_archive_directory(archive: tarfile.TarFile, name: str) -> None:
        info = tarfile.TarInfo(name)
        info.type = tarfile.DIRTYPE
        info.mode = 0o755
        archive.addfile(info)

    @staticmethod
    def add_archive_file(
        archive: tarfile.TarFile,
        name: str,
        payload: bytes,
        mode: int,
    ) -> None:
        info = tarfile.TarInfo(name)
        info.size = len(payload)
        info.mode = mode
        archive.addfile(info, io.BytesIO(payload))

    def run_bootstrap(self, **overrides: str) -> subprocess.CompletedProcess[str]:
        environment = os.environ.copy()
        environment.update(
            {
                "PATH": f"{self.fake_bin}:{environment['PATH']}",
                "TMPDIR": str(self.tmp_dir),
                "BOOTSTRAP_FIXTURE_DIR": str(self.fixture_dir),
                "BOOTSTRAP_EXPECTED_KEY": PUBLISHER_KEY,
                "BOOTSTRAP_CURL_LOG": str(self.curl_log),
                "BOOTSTRAP_STAGE_MARKER": str(self.stage_marker),
                "BOOTSTRAP_FORBIDDEN_MARKER": str(self.forbidden_marker),
                "BOOTSTRAP_MINISIGN_MARKER": str(self.minisign_marker),
            }
        )
        environment.update(overrides)
        return subprocess.run(
            [str(self.bootstrap)],
            check=False,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            env=environment,
        )

    def assert_rejected(
        self, expected: str, **overrides: str
    ) -> subprocess.CompletedProcess[str]:
        result = self.run_bootstrap(**overrides)
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn(expected, result.stderr)
        self.assertFalse(self.stage_marker.exists())
        self.assertFalse(self.forbidden_marker.exists())
        self.assertEqual(list(self.tmp_dir.glob("goq-sigil-install.*")), [])
        return result

    def test_rejects_wrong_os_arch_root_and_non_bazzite(self) -> None:
        cases = [
            ("Sigil requires Linux", {"BOOTSTRAP_TEST_OS": "Darwin"}),
            ("Sigil currently requires x86_64", {"BOOTSTRAP_TEST_ARCH": "aarch64"}),
            ("not root", {"BOOTSTRAP_TEST_UID": "0"}),
            ("currently supports Bazzite", {"BOOTSTRAP_TEST_BAZZITE": "0"}),
        ]
        for expected, overrides in cases:
            with self.subTest(expected=expected):
                self.assert_rejected(expected, **overrides)

    def test_verifier_download_and_digest_fail_before_any_verifier_executes(self) -> None:
        self.assert_rejected(
            "Minisign verifier download failed",
            BOOTSTRAP_TEST_CURL_PARTIAL=MINISIGN_ASSET_NAME,
        )
        self.assertFalse(self.minisign_marker.exists())
        self.assertEqual(
            self.curl_log.read_text(encoding="utf-8").splitlines(),
            [MINISIGN_URL],
        )

        (self.fixture_dir / MINISIGN_ASSET_NAME).write_bytes(b"")
        self.assert_rejected("Minisign verifier download is empty")
        self.assertFalse(self.minisign_marker.exists())

        digest = self.write_minisign_fixture()
        self.assertNotEqual(digest, ZERO_SHA256)
        with (self.fixture_dir / MINISIGN_ASSET_NAME).open("ab") as archive:
            archive.write(b"corrupt")
        self.assert_rejected("Minisign verifier checksum does not match")
        self.assertFalse(self.minisign_marker.exists())

    def test_verifier_archive_shape_and_types_fail_before_execution(self) -> None:
        self.repin_minisign_fixture(variant="unexpected")
        self.assert_rejected("Minisign verifier archive has unexpected contents")
        self.assertFalse(self.minisign_marker.exists())

        self.repin_minisign_fixture(variant="symlink")
        self.assert_rejected("Minisign verifier archive contains a special file or link")
        self.assertFalse(self.minisign_marker.exists())

    def test_verifier_fixture_models_the_full_official_archive_layout(self) -> None:
        with tarfile.open(self.fixture_dir / MINISIGN_ASSET_NAME, "r:gz") as archive:
            self.assertEqual(
                {member.name.rstrip("/") for member in archive.getmembers()},
                {
                    member.rstrip("/")
                    for member in MINISIGN_CORE_MEMBERS
                    | MINISIGN_APPLEDOUBLE_MEMBERS
                },
            )

    def test_rejects_partial_download_bad_signature_and_bad_checksum(self) -> None:
        self.assert_rejected(
            "signed release asset download failed",
            BOOTSTRAP_TEST_CURL_PARTIAL=".sha256",
        )
        self.assertEqual(
            self.curl_log.read_text(encoding="utf-8").splitlines(),
            [MINISIGN_URL, ASSET_BASE, f"{ASSET_BASE}.sha256"],
        )
        (self.fixture_dir / f"{ASSET_NAME}.sha256").write_text("", encoding="utf-8")
        self.assert_rejected("signed release asset download is empty")
        self.write_release_fixture()
        self.assert_rejected(
            "publisher signature is invalid",
            BOOTSTRAP_TEST_MINISIGN="fail",
        )
        self.assertEqual(self.minisign_marker.read_text(encoding="utf-8"), "invoked\n")
        self.write_release_fixture(valid_checksum=False)
        self.assert_rejected("release checksum does not match")

    def test_rejects_malformed_checksum_declarations(self) -> None:
        checksum_path = self.fixture_dir / f"{ASSET_NAME}.sha256"
        valid = checksum_path.read_text(encoding="utf-8")
        checksum_path.write_text(valid + valid, encoding="utf-8")
        self.assert_rejected("release checksum file is malformed")

        self.write_release_fixture()
        digest = hashlib.sha256((self.fixture_dir / ASSET_NAME).read_bytes()).hexdigest()
        checksum_path.write_text(f"{digest}  wrong.tar.gz\n", encoding="utf-8")
        self.assert_rejected("release checksum declaration is malformed")

    def test_rejects_escaping_paths_and_links_before_extraction(self) -> None:
        self.write_release_fixture(variant="escape")
        self.assert_rejected("escapes its payload root")
        self.write_release_fixture(variant="symlink")
        self.assert_rejected("special file or link")

    def test_success_invokes_only_the_package_stager_and_cleans_up(self) -> None:
        result = self.run_bootstrap()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(self.stage_marker.read_text(encoding="utf-8"), "staged\n")
        self.assertEqual(self.minisign_marker.read_text(encoding="utf-8"), "invoked\n")
        self.assertFalse(self.forbidden_marker.exists())
        self.assertIn(f"Sigil runtime {RELEASE_TAG} is installed.", result.stdout)
        self.assertIn(
            ".local/libexec/sigil-spark/current/docs/sigil-host-activation.md",
            result.stdout,
        )
        self.assertNotIn("fresh Bazzite runbook", result.stdout)
        self.assertEqual(list(self.tmp_dir.glob("goq-sigil-install.*")), [])


if __name__ == "__main__":
    unittest.main()
