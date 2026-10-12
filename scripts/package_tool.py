#!/usr/bin/env python3
# Copyright The eha-sdk Contributors
"""构造携带私有 Python CAN 运行时的单目录 eha-tool 发行包。

这是发行作者工具。普通用户只运行输出目录中的 eha-tool，既不需要 pip，也不需要
venv；CAN 通道和驱动仍由部署方按 python-can 的公共配置提供。
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import shutil
import subprocess
import sys
import tarfile
import tempfile
import urllib.request


ROOT = Path(__file__).resolve().parent.parent
DEFAULT_MANIFEST = ROOT / "runtime" / "manifest.json"
DEFAULT_REQUIREMENTS = ROOT / "runtime" / "requirements.lock"
HEALTH_CHECK = (
    "import can, serial; "
    "assert can.__version__ == '4.6.1'; "
    "assert serial.VERSION == '3.5'"
)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=Path, help="已构建的 eha-tool 可执行文件")
    parser.add_argument("--target", required=True, help="Rust target triple，例如 aarch64-apple-darwin")
    parser.add_argument("--output", required=True, type=Path, help="必须不存在的发行目录")
    parser.add_argument(
        "--runtime-archive",
        type=Path,
        help="已下载的 python-build-standalone archive；省略时按 manifest 下载",
    )
    parser.add_argument("--manifest", type=Path, default=DEFAULT_MANIFEST)
    parser.add_argument("--requirements", type=Path, default=DEFAULT_REQUIREMENTS)
    return parser.parse_args()


def read_manifest(path: Path, target: str) -> dict[str, str]:
    try:
        document = json.loads(path.read_text(encoding="utf-8"))
        targets = document["targets"]
        entry = targets.get(target)
        if entry is None:
            available = ", ".join(sorted(targets))
            raise RuntimeError(
                f"尚未提供 {target} 的私有 Python 运行时；当前可打包 target: {available}"
            )
        archive_name = entry["archive_name"]
        url = entry["url"]
        checksum = entry["sha256"]
    except RuntimeError:
        raise
    except (OSError, KeyError, TypeError, json.JSONDecodeError) as error:
        raise RuntimeError(f"无效的运行时 manifest {path}: {error}") from error
    if document.get("format_version") != 1:
        raise RuntimeError("不支持的运行时 manifest format_version")
    if not all(isinstance(value, str) and value for value in (archive_name, url, checksum)):
        raise RuntimeError(f"manifest 中 {target} 的运行时条目不完整")
    if len(checksum) != 64 or any(character not in "0123456789abcdef" for character in checksum):
        raise RuntimeError(f"manifest 中 {target} 的 SHA-256 无效")
    return {"archive_name": archive_name, "url": url, "sha256": checksum}


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as file:
        for block in iter(lambda: file.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def download(url: str, destination: Path) -> None:
    with urllib.request.urlopen(url, timeout=60) as response, destination.open("wb") as file:
        shutil.copyfileobj(response, file)


def obtain_archive(
    supplied: Path | None, entry: dict[str, str], temporary: Path
) -> Path:
    if supplied is None:
        archive = temporary / entry["archive_name"]
        print(f"下载私有 Python 运行时: {entry['url']}")
        download(entry["url"], archive)
    else:
        archive = supplied.resolve()
        if not archive.is_file():
            raise RuntimeError(f"--runtime-archive 不是普通文件: {archive}")
    actual = sha256(archive)
    if actual != entry["sha256"]:
        raise RuntimeError(
            f"Python 运行时 SHA-256 不匹配: 期望 {entry['sha256']}，实际 {actual}"
        )
    return archive


def normalized_link(member: tarfile.TarInfo) -> PurePosixPath:
    target = PurePosixPath(member.linkname)
    if target.is_absolute():
        raise RuntimeError(f"运行时 archive 包含绝对链接: {member.name}")
    parts: list[str] = []
    for part in (*PurePosixPath(member.name).parent.parts, *target.parts):
        if part in ("", "."):
            continue
        if part == "..":
            if not parts:
                raise RuntimeError(f"运行时 archive 的链接越出根目录: {member.name}")
            parts.pop()
        else:
            parts.append(part)
    return PurePosixPath(*parts)


def validate_member(member: tarfile.TarInfo) -> None:
    path = PurePosixPath(member.name)
    if path.is_absolute() or ".." in path.parts or not path.parts or path.parts[0] != "python":
        raise RuntimeError(f"运行时 archive 包含越界路径: {member.name}")
    if member.issym() or member.islnk():
        link = normalized_link(member)
        if not link.parts or link.parts[0] != "python":
            raise RuntimeError(f"运行时 archive 的链接越出 python 根: {member.name}")
    elif not (member.isfile() or member.isdir()):
        raise RuntimeError(f"运行时 archive 包含不支持的条目: {member.name}")


def extract_runtime(archive: Path, runtime: Path) -> Path:
    with tarfile.open(archive, "r:gz") as bundle:
        members = bundle.getmembers()
        for member in members:
            validate_member(member)
        if sys.version_info >= (3, 12):
            bundle.extractall(runtime, members=members, filter="data")
        else:
            # 已在上方逐项拒绝绝对路径、`..`、越界链接和特殊文件；兼容仍受支持的
            # Python 3.9--3.11，它们没有 tarfile 的 filter 参数。
            bundle.extractall(runtime, members=members)
    python = runtime / "python" / "bin" / "python3"
    if not python.is_file():
        raise RuntimeError(f"运行时 archive 未提供预期解释器: {python}")
    return python


def install_dependencies(python: Path, requirements: Path) -> None:
    if not requirements.is_file():
        raise RuntimeError(f"依赖锁文件不存在: {requirements}")
    environment = os.environ.copy()
    environment["PIP_NO_INPUT"] = "1"
    for name in tuple(environment):
        if name.startswith("PYTHON"):
            environment.pop(name)
    command = [
        str(python),
        "-I",
        "-m",
        "pip",
        "install",
        "--disable-pip-version-check",
        "--no-cache-dir",
        "--no-deps",
        "--only-binary=:all:",
        "--require-hashes",
        "-r",
        str(requirements.resolve()),
    ]
    print("安装锁定的 python-can 运行时依赖")
    subprocess.run(command, check=True, env=environment)


def copy_licenses(runtime: Path) -> None:
    licenses = runtime / "licenses"
    licenses.mkdir()
    python_licenses = list((runtime / "python" / "lib").glob("python*/LICENSE.txt"))
    if len(python_licenses) != 1:
        raise RuntimeError("运行时中未找到唯一的 CPython LICENSE.txt")
    target = licenses / "CPython"
    target.mkdir()
    shutil.copy2(python_licenses[0], target / "LICENSE.txt")

    for info in sorted((runtime / "python" / "lib").glob("python*/site-packages/*.dist-info")):
        destination = licenses / info.name
        source = info / "licenses"
        if source.is_dir():
            shutil.copytree(source, destination)
        else:
            metadata = info / "METADATA"
            if metadata.is_file():
                destination.mkdir()
                shutil.copy2(metadata, destination / "METADATA")


def check_runtime(python: Path) -> None:
    subprocess.run([str(python), "-I", "-c", HEALTH_CHECK], check=True)


def package(args: argparse.Namespace) -> None:
    binary = args.binary.resolve()
    manifest = args.manifest.resolve()
    requirements = args.requirements.resolve()
    output = args.output.absolute()
    if not binary.is_file():
        raise RuntimeError(f"--binary 不是普通文件: {binary}")
    if output.exists():
        raise RuntimeError(f"--output 必须不存在，拒绝覆盖已有路径: {output}")
    if output.parent == output:
        raise RuntimeError("--output 不能是文件系统根目录")
    entry = read_manifest(manifest, args.target)

    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix=f".{output.name}.", dir=output.parent) as temporary_text:
        temporary = Path(temporary_text)
        archive = obtain_archive(args.runtime_archive, entry, temporary)
        bundle = temporary / output.name
        bundle.mkdir()
        shutil.copy2(binary, bundle / "eha-tool")
        runtime = bundle / "runtime"
        runtime.mkdir()
        python = extract_runtime(archive, runtime)
        install_dependencies(python, requirements)
        copy_licenses(runtime)
        check_runtime(python)
        os.replace(bundle, output)
    print(f"已创建 eha-tool 发行目录: {output}")


def main() -> int:
    try:
        package(parse_args())
    except (OSError, RuntimeError, subprocess.CalledProcessError, tarfile.TarError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
