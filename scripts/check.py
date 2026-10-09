# Copyright The eha-sdk Contributors
"""检查 SDK、官方工具与公共库，不访问设备；在 SDK 独立检出中也可运行。"""

import argparse
import os
from pathlib import Path
import subprocess


def main():
    argparse.ArgumentParser(description=__doc__).parse_args()
    root = Path(__file__).resolve().parents[1]
    commands = [
        ["cargo", "fmt", "--all", "--", "--check"],
        ["cargo", "test", "--workspace", "--all-targets", "--locked"],
        ["node", "--test", "crates/tool/webui/tests/telemetry.test.js"],
        ["cargo", "clippy", "--workspace", "--all-targets", "--locked", "--", "-D", "warnings"],
        ["cargo", "doc", "--workspace", "--no-deps", "--locked"],
        ["cargo", "check", "-p", "eha-sdk", "--no-default-features", "--target",
         "thumbv7em-none-eabihf", "--locked"],
        ["cargo", "build", "-p", "eha-tool", "--release", "--locked"],
    ]
    for command in commands:
        print("+ " + subprocess.list2cmdline(command), flush=True)
        environment = dict(os.environ)
        if command[1] == "doc":
            environment["RUSTDOCFLAGS"] = "-Dwarnings"
        subprocess.run(command, cwd=root, env=environment, check=True)


if __name__ == "__main__":
    main()
