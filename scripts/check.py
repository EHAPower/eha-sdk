# Copyright The eha_controller Contributors
"""检查工具的 Rust 与 WebUI，并构建 release 产物；不访问设备。"""

import argparse
from pathlib import Path
import subprocess


def main():
    argparse.ArgumentParser(description=__doc__).parse_args()
    root = Path(__file__).resolve().parents[1]
    commands = [
        ["cargo", "fmt", "--all", "--", "--check"],
        ["cargo", "test", "--all-targets", "--locked"],
        ["node", "--test", *[str(path) for path in sorted((root / "webui/tests").glob("*.test.js"))]],
        ["cargo", "clippy", "--all-targets", "--locked", "--", "-D", "warnings"],
        ["cargo", "build", "--release", "--locked"],
    ]
    for command in commands:
        print("+ " + subprocess.list2cmdline(command), flush=True)
        subprocess.run(command, cwd=root, check=True)


if __name__ == "__main__":
    main()
