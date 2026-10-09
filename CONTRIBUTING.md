<!-- Copyright The eha-sdk Contributors -->

# 贡献指南

本仓是独立的 SDK 与官方工具工作区。使用 Rust stable；不要求其他工程、产品配置或设备连接。

## 依赖

按用途自行安装所需依赖：

| 用途 | 依赖 |
|---|---|
| SDK 与工具构建、二次开发 | Git、Rust stable、Cargo、原生链接器；Rust 包由 `Cargo.toml`／`Cargo.lock` 管理 |
| 完整软件检查 | Python 3、Node.js 24、Clippy、rustfmt、`thumbv7em-none-eabihf` target；Python 脚本仅使用标准库 |
| 更新工具 Web UI vendor 资源 | Node.js 24、npm；前端包由 `crates/tool/webui/package.json`／`package-lock.json` 管理 |
| 已构建工具运行 | 匹配主机系统和架构的可执行文件；页面资源已内嵌，EHA 操作不需要 Python 或 Node.js |
| ODrive USB 独立只读（可选） | Python 3、`odrive==0.5.1.post0`（自带 Fibre）、PyUSB、libusb 1.0，以及 USB 驱动／访问权限 |

## 构建与检查

```sh
git clone https://github.com/EHAPower/eha-sdk.git
cd eha-sdk
cargo build --workspace --locked
cargo test --workspace --all-targets --locked
```

构建当前主机的 release 工具：

```sh
cargo build -p eha-tool --release --locked
```

产物位于 `target/release/eha-tool`（Windows 为 `eha-tool.exe`）。可直接运行该路径，或执行
`cargo install --path crates/tool --locked` 后从 Cargo 的可执行文件目录运行。主机产物须匹配实际
运行系统与架构。

完整软件检查：

```sh
rustup component add clippy rustfmt
rustup target add thumbv7em-none-eabihf
python3 scripts/check.py
```

入口运行 SDK 与工具的格式、Rust 测试、Web UI 遥测测试、Clippy、API 文档、SDK ARM `no_std`
检查和工具 release 构建，不访问设备。局部变更按实际影响选择相称命令。已有 vendor 资源可直接构建，
不需要先安装 npm 依赖。

修改 Web UI 图表依赖时，从仓库根执行：

```sh
cd crates/tool/webui
npm ci
npm run vendor
cd ../../..
cargo build -p eha-tool --release --locked
```

一并维护前端清单、锁文件、静态资源及许可证；第三方资源保留原始版权声明。

SDK 与工具共用 `workspace.package.version`；修改后运行
`cargo update --workspace --offline` 更新工作区锁文件。消费者选择兼容的 SDK 修订并维护
自己的 `Cargo.lock`。
