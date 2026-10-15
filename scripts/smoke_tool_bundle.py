#!/usr/bin/env python3
# Copyright The eha-sdk Contributors
"""在本机解压刚生成的可信发行包，验证无系统 Python 的启动和 CAN 枚举；不打开设备。"""

import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile


def smoke(archive: Path) -> None:
    with tempfile.TemporaryDirectory(prefix="eha bundle smoke ") as temporary:
        destination = Path(temporary)
        shutil.unpack_archive(archive, destination)
        name = "eha-tool.exe" if os.name == "nt" else "eha-tool"
        binaries = list(destination.glob(f"*/{name}"))
        if len(binaries) != 1:
            raise RuntimeError(f"发行包须包含唯一的 {name}: {binaries}")
        binary = binaries[0].resolve()
        empty_path = destination / "empty path"
        empty_path.mkdir()
        environment = os.environ.copy()
        environment.update({
            "PATH": str(empty_path),
            "PYTHONHOME": "__invalid_pythonhome__",
            "PYTHONPATH": "__invalid_pythonpath__",
            "EHA_TOOL_DEVELOPER_PYTHON": "__invalid_developer_python__",
            "XDG_CONFIG_HOME": str(destination / "config"),
            "LOCALAPPDATA": str(destination / "config"),
            "APPDATA": str(destination / "config"),
        })
        for arguments in (["--version"], ["connection", "list"], ["connection", "adapters"]):
            result = subprocess.run(
                [str(binary), *arguments], check=True, cwd=binary.parent,
                env=environment, capture_output=True, text=True, encoding="utf-8", timeout=20,
            )
            print(result.stdout.strip())
            if arguments == ["connection", "list"] and json.loads(result.stdout).get("ok") is not True:
                raise RuntimeError("连接档案目录不可用")
            if arguments == ["connection", "adapters"] and not isinstance(json.loads(result.stdout), list):
                raise RuntimeError("CAN 适配器枚举未返回列表")
    print("发行包解压迁移、连接档案目录和随包 Python CAN 枚举自检通过（未连接设备）")


if __name__ == "__main__":
    sys.stdout.reconfigure(encoding="utf-8")
    sys.stderr.reconfigure(encoding="utf-8")
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("archive", type=Path, help="本机 target 的发行归档文件")
    args = parser.parse_args()
    smoke(args.archive.resolve())
