# Copyright The eha-sdk Contributors
"""检查 SDK、官方工具与公共库，不访问设备；在 SDK 独立检出中也可运行。"""

import argparse
import os
import re
from pathlib import Path
import subprocess
import sys


def check_numeric_policy(root):
    """自有 Rust 业务代码采用 binary32；整数时基和第三方依赖不受此扫描影响。"""
    forbidden = re.compile(r"\bf64\b|\b\w*f64\b|\b(?:as_f64|as_secs_f64|from_secs_f64|try_from_secs_f64)\b")
    failures = []
    for directory in ("src", "crates", "targets", "examples", "tests"):
        for path in sorted((root / directory).rglob("*.rs")):
            for line_number, line in enumerate(path.read_text().splitlines(), 1):
                if forbidden.search(line):
                    failures.append(f"{path.relative_to(root)}:{line_number}: {line.strip()}")
    if failures:
        raise SystemExit("自有 Rust 必须使用 f32，时间运算保持整数：\n" + "\n".join(failures))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--numeric-only", action="store_true", help="只检查自有 Rust 的 binary32 规则")
    parser.add_argument("--numeric-root", type=Path, help="仅数值检查时指定采用方工作区根目录")
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    if args.numeric_root is not None and not args.numeric_only:
        parser.error("--numeric-root 只能与 --numeric-only 一起使用")
    numeric_root = args.numeric_root.resolve() if args.numeric_root else root
    if not numeric_root.is_dir():
        parser.error("数值检查根目录不存在")
    check_numeric_policy(numeric_root)
    if args.numeric_only:
        return
    commands = [
        ["cargo", "fmt", "--all", "--", "--check"],
        [sys.executable, "-m", "unittest", "discover", "-s", "tests", "-p", "test_python_can.py"],
        ["cargo", "test", "--workspace", "--all-targets", "--locked"],
        ["node", "--test", *[
            str(path.relative_to(root))
            for path in sorted((root / "crates/tool/webui/tests").glob("*.test.js"))
        ]],
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
