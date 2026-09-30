<!-- Copyright The eha_controller Contributors -->

# 贡献指南

本仓是独立的 SDK 工作区。使用 Rust stable；不要求其他工程、产品配置或设备连接。

## 依赖

按用途自行安装所需依赖：

| 用途 | 依赖 |
|---|---|
| 构建与二次开发 | Git、Rust stable、Cargo、原生链接器；Rust 包由 `Cargo.toml`／`Cargo.lock` 管理 |
| 完整软件检查 | Python 3、Clippy、rustfmt、`thumbv7em-none-eabihf` target；Python 脚本仅使用标准库 |
| ODrive USB 独立只读（可选） | Python 3、`odrive==0.5.1.post0`（自带 Fibre）、PyUSB、libusb 1.0，以及 USB 驱动／访问权限 |

## 构建与检查

```sh
git clone https://github.com/EHAPower/eha-sdk.git
cd eha-sdk
cargo build --workspace --locked
cargo test --workspace --all-targets --locked
```

`eha-tool` 是独立仓库；同时开发时按其仓库的贡献指南准备依赖和运行检查。

完整软件检查：

```sh
rustup component add clippy rustfmt
rustup target add thumbv7em-none-eabihf
python3 scripts/check.py
```

入口运行测试、Clippy、API 文档和 ARM `no_std` 检查，不访问设备。局部变更按实际影响选择相称命令。

本仓版本独立维护在 `workspace.package.version`；修改后运行
`cargo update --workspace --offline` 更新自己的锁文件。消费者选择兼容的 SDK 修订并维护
自己的 `Cargo.lock`。
