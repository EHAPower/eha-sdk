<!-- Copyright The eha_controller Contributors -->

# 贡献指南

`eha-tool` 是独立公开仓，依赖同级目录中的 `../eha-sdk` 工作区，不依赖固件仓、产品配置或设备输入。
将两个仓库克隆到同一父目录后，在工具仓执行检查。命令、选项和默认值由 `eha-tool --help`
及子命令帮助维护。

## 依赖

按用途自行安装所需依赖：

| 用途 | 依赖 |
|---|---|
| Cargo 构建 | Git、Rust stable、Cargo、原生链接器、相邻 SDK；Rust 包由 `Cargo.toml`／`Cargo.lock` 管理 |
| 完整软件检查 | Python 3、Node.js 24、Clippy、rustfmt；Python 脚本仅使用标准库 |
| 更新前端 vendor 资源 | Node.js 24、npm；前端包由 `webui/package.json`／`package-lock.json` 管理 |
| 已构建工具运行 | 匹配主机系统和架构的可执行文件；页面资源已内嵌，EHA 操作不需要 Python 或 Node.js |
| ODrive USB 独立只读（可选） | [SDK 的 ODrive 依赖](https://github.com/EHAPower/eha-sdk/blob/main/CONTRIBUTING.md#依赖)；用 `--python`、`--odrive-python` 或 `EHA_ODRIVE_PYTHON` 指定解释器 |

## 构建与检查

```sh
git clone https://github.com/EHAPower/eha-sdk.git eha-sdk
git clone https://github.com/EHAPower/eha-tool.git eha-tool
cd eha-tool
cargo build --locked
cargo test --all-targets --locked
```

完整软件检查：

```sh
rustup component add clippy rustfmt
python3 scripts/check.py
```

入口运行 Rust 和 WebUI 测试、Clippy 与 release 构建，不访问设备。已有 vendor 资源可直接构建，不需要先安装 npm 依赖。修改 WebUI 图表依赖时，在 `webui/` 执行 `npm ci` 和 `npm run vendor`，一并维护锁文件、静态资源及许可证，再重新构建工具。

工具版本独立维护在 `workspace.package.version`；修改后运行 `cargo update --workspace --offline` 更新自己的锁文件。相邻 SDK 的版本变化时也用该命令刷新依赖记录，不要求同步提高工具版本。

设备测试需要另行明确对象、参数和结果证据；上述检查不访问设备。
