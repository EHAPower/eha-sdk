# Copyright The eha-sdk Contributors
"""发行目录布局、失败清理和压缩包迁移行为；不下载或执行第三方代码。"""

import argparse
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile
import unittest
from unittest.mock import patch
import zipfile


spec = importlib.util.spec_from_file_location(
    "package_tool", Path(__file__).resolve().parents[1] / "scripts" / "package_tool.py"
)
package_tool = importlib.util.module_from_spec(spec)
spec.loader.exec_module(package_tool)

LINUX = "aarch64-unknown-linux-gnu"
WINDOWS = "x86_64-pc-windows-msvc"


class PackageToolTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)

    def fixture(self, target):
        windows = target == WINDOWS
        interpreter = "python/python.exe" if windows else "python/bin/python3"
        library = "python/Lib" if windows else "python/lib/python3.13"
        license_path = "python/LICENSE.txt" if windows else f"{library}/LICENSE.txt"
        files = {
            interpreter: b"fixture interpreter",
            license_path: b"CPython license",
            f"{library}/site-packages/example.dist-info/METADATA": b"Name: example",
            f"{library}/site-packages/example.dist-info/LICENSE": b"dependency license",
        }
        archive = self.root / "python.tar.gz"
        with tarfile.open(archive, "w:gz") as output:
            for name, data in files.items():
                member = tarfile.TarInfo(name)
                member.size = len(data)
                member.mode = 0o755 if name == interpreter else 0o644
                output.addfile(member, io.BytesIO(data))
            if not windows:
                member = tarfile.TarInfo("python/bin/python-alias")
                member.type = tarfile.SYMTYPE
                member.linkname = "../bin/python3"
                output.addfile(member)
        manifest = self.root / "manifest.json"
        manifest.write_text(json.dumps({
            "format_version": 1,
            "targets": {target: {
                "archive_name": archive.name,
                "url": "https://example.invalid/python.tar.gz",
                "sha256": package_tool.sha256(archive),
            }},
        }), encoding="utf-8")
        binary = self.root / "source binary"
        binary.write_bytes(b"fixture eha-tool")
        binary.chmod(0o755)
        requirements = self.root / "requirements.lock"
        requirements.write_text("", encoding="utf-8")
        return argparse.Namespace(
            binary=binary, target=target, output=self.root / "eha tool bundle",
            archive=True, runtime_archive=archive, manifest=manifest,
            requirements=requirements,
        )

    def package(self, args, health_error=None):
        with patch.object(package_tool, "validate_host"), \
                patch.object(package_tool, "install_dependencies"), \
                patch.object(package_tool, "check_runtime", side_effect=health_error):
            package_tool.package(args)

    def test_commands_print_utf8_with_a_legacy_console_encoding(self):
        scripts = Path(__file__).resolve().parents[1] / "scripts"
        environment = {**os.environ, "PYTHONIOENCODING": "cp1252"}
        for script in ("package_tool.py", "smoke_tool_bundle.py"):
            with self.subTest(script=script):
                result = subprocess.run(
                    [sys.executable, str(scripts / script), "--help"],
                    env=environment, capture_output=True, timeout=10,
                )
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn("发行包", result.stdout.decode("utf-8"))

    def test_native_targets_and_architecture_aliases(self):
        for target, (system, architecture) in package_tool.TARGETS.items():
            for machine in (("AMD64", "x86_64") if architecture == "amd64" else ("ARM64", "aarch64")):
                with self.subTest(target=target, machine=machine), \
                        patch.object(package_tool.platform, "system", return_value=system), \
                        patch.object(package_tool.platform, "machine", return_value=machine):
                    package_tool.validate_host(target)

    def test_wrong_host_is_rejected_before_download(self):
        args = self.fixture(WINDOWS)
        with patch.object(package_tool.platform, "system", return_value="Linux"), \
                patch.object(package_tool.platform, "machine", return_value="aarch64"), \
                patch.object(package_tool, "obtain_archive") as download:
            with self.assertRaisesRegex(RuntimeError, "OS／架构"):
                package_tool.package(args)
            download.assert_not_called()
        self.assertFalse(args.output.exists())

    def test_windows_zip_contains_executable_runtime_and_licenses(self):
        args = self.fixture(WINDOWS)
        self.package(args)
        destination = self.root / "relocated with spaces"
        with zipfile.ZipFile(str(args.output) + ".zip") as archive:
            archive.extractall(destination)
        bundle = destination / args.output.name
        self.assertEqual((bundle / "eha-tool.exe").read_bytes(), b"fixture eha-tool")
        self.assertTrue((bundle / "runtime/python/python.exe").is_file())
        self.assertEqual((bundle / "runtime/licenses/CPython/LICENSE.txt").read_bytes(), b"CPython license")
        self.assertEqual((bundle / "runtime/licenses/example.dist-info/LICENSE").read_bytes(), b"dependency license")

    def test_unix_archive_keeps_executable_permissions(self):
        args = self.fixture(LINUX)
        self.package(args)
        with tarfile.open(str(args.output) + ".tar.gz", "r:gz") as archive:
            for name in ("eha-tool", "runtime/python/bin/python3"):
                member = archive.getmember(f"{args.output.name}/{name}")
                self.assertEqual(member.mode & 0o111, 0o111)
            self.assertEqual(archive.extractfile(f"{args.output.name}/runtime/licenses/CPython/LICENSE.txt").read(), b"CPython license")
            self.assertEqual(archive.getmember(f"{args.output.name}/runtime/python/bin/python-alias").linkname, "../bin/python3")

    def test_failed_health_check_does_not_publish_partial_bundle(self):
        args = self.fixture(LINUX)
        with self.assertRaisesRegex(RuntimeError, "health failure"):
            self.package(args, RuntimeError("health failure"))
        self.assertFalse(args.output.exists())
        self.assertFalse(Path(str(args.output) + ".tar.gz").exists())
        self.assertEqual(list(self.root.glob(".eha tool bundle.*")), [])

    def test_existing_archive_is_not_overwritten(self):
        args = self.fixture(WINDOWS)
        existing = Path(str(args.output) + ".zip")
        existing.write_bytes(b"keep me")
        with self.assertRaisesRegex(RuntimeError, "拒绝覆盖"):
            self.package(args)
        self.assertEqual(existing.read_bytes(), b"keep me")
        self.assertFalse(args.output.exists())

    def test_mismatched_runtime_layout_is_rejected(self):
        args = self.fixture(WINDOWS)
        with self.assertRaisesRegex(RuntimeError, "预期解释器"):
            package_tool.extract_runtime(args.runtime_archive, self.root / "runtime", LINUX)

    def test_archive_rejects_windows_and_posix_escape_paths(self):
        for name in ("/python/file", "python/../../file", "python/..\\file", "python/C:/file"):
            with self.subTest(name=name), self.assertRaises(RuntimeError):
                package_tool.validate_member(tarfile.TarInfo(name))
        for link in ("../../outside", "/outside", "C:/outside", "..\\outside"):
            member = tarfile.TarInfo("python/bin/python3")
            member.type = tarfile.SYMTYPE
            member.linkname = link
            with self.subTest(link=link), self.assertRaises(RuntimeError):
                package_tool.validate_member(member)

    def test_tar_hardlinks_and_symlinks_use_their_respective_roots(self):
        member = tarfile.TarInfo("python/bin/python3")
        member.type = tarfile.LNKTYPE
        member.linkname = "python/bin/python3.13"
        package_tool.validate_member(member)
        self.assertEqual(str(package_tool.normalized_link(member)), "python/bin/python3.13")
        member.type = tarfile.SYMTYPE
        member.linkname = "python3.13"
        package_tool.validate_member(member)
        self.assertEqual(str(package_tool.normalized_link(member)), "python/bin/python3.13")


if __name__ == "__main__":
    unittest.main()
