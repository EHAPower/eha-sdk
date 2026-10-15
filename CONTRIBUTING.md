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
| 已构建工具运行（仅 USB） | 匹配主机系统和架构的可执行文件；页面资源已内嵌，EHA 操作不需要 Python 或 Node.js |
| 已构建工具运行（外部 CAN） | 普通发行 bundle 随附私有 `runtime/python`，可直接使用已支持 CANable2/SLCAN 连接档案和其中已包含依赖的高级 `python-can` 后端；后端另有厂商 Python 包或 SDK 时，调用方通过 `--can-python` 明确提供解释器，并负责驱动、权限和总线参数。安装、打包和持久配置见 [CAN 接入指南](CAN接入指南.md#主机-can-通道) |
| ODrive USB 独立只读（可选） | Python 3、`odrive==0.5.1.post0`（自带 Fibre）、PyUSB、libusb 1.0，以及 USB 驱动／访问权限 |

普通 CAN bundle 的根目录只有 `eha-tool` 与私有 `runtime/python/...`。运行时隔离导入锁定的 `python-can==4.6.1`、`pyserial==3.5` 及其依赖，并保留 CPython 与各分发包许可证到 `runtime/licenses`；损坏或不匹配时工具在五秒健康检查内失败，绝不改用 `PATH` 的 Python。源码构建只可显式设置 `EHA_TOOL_DEVELOPER_PYTHON`，`--can-python PATH` 始终是高级覆盖。

发行包由 `.github/workflows/tool-bundles.yml` 在对应系统和架构的原生 runner 上制作：macOS arm64、Linux GNU glibc amd64/arm64、Windows amd64/arm64。Linux job 固定 Ubuntu 22.04 作为 glibc 2.35 基线；Windows 使用 MSVC runner，分别面向 Windows 10/11 amd64 与 Windows 11 arm64。工作流上传归档产物，不会自行创建 Release；发布者应在测试后选择和发布这些产物。

本地制包也必须在与目标相同的 OS 和 CPU 架构上执行。先在该主机构建 release 工具，再执行：

```sh
python3 scripts/package_tool.py --binary /abs/target/release/eha-tool \
  --target TARGET --output /abs/eha-tool-TARGET --archive
```

Windows 制包前，在 PowerShell 设置 `$env:RUSTFLAGS = '-C target-feature=+crt-static'`，并执行
`cargo build -p eha-tool --release --locked --target TARGET`；`--binary` 选择
`target/TARGET/release/eha-tool.exe`。这让主程序静态链接 CRT，避免依赖用户另装 VC 运行库；
随包 Python 仍使用自己目录中的 DLL。CI 还检查主程序的 DLL 导入，拒绝动态 VC/UCRT 依赖。
构建选项见 [Rust CRT 链接说明](https://doc.rust-lang.org/reference/linkage.html#static-and-dynamic-c-runtimes)。

其中 `TARGET` 为 `aarch64-apple-darwin`、`x86_64-unknown-linux-gnu`、`aarch64-unknown-linux-gnu`、`x86_64-pc-windows-msvc` 或 `aarch64-pc-windows-msvc`。`--archive` 在 Windows 输出 `.zip`，在 Unix 输出 `.tar.gz`，同时保留完整目录；不能只分发根可执行文件。制包会下载固定的 Python 3.13.16 runtime（PBS 20261009），按哈希安装锁定的 `python-can==4.6.1`、`pyserial==3.5` 和依赖，并将许可证写入 `runtime/licenses`。用户运行已生成的发行包时不需要联网安装 Python 或依赖。不要以本地源码虚拟环境替代正式 bundle 的运行时检查。

用 `python3 scripts/smoke_tool_bundle.py ARCHIVE` 检查刚生成的本机发行包；工作流也使用这一入口。它在带空格的新目录解压，移除系统 Python 的搜索路径并设置无效 Python 环境，验证工具启动、连接档案目录和 CAN 枚举，不打开适配器或连接控制器。

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

入口首先检查自有 Rust 的 binary32 规则，再运行 SDK 与工具的格式、Rust 测试、Web UI 遥测测试、Clippy、API 文档、SDK ARM `no_std`
检查和工具 release 构建，不访问设备。局部变更按实际影响选择相称命令。已有 vendor 资源可直接构建，
不需要先安装 npm 依赖。

阅读本地 API 文档使用 `cargo doc --workspace --no-deps --locked --open`；各 crate README 说明职责与使用流程，rustdoc 维护具体接口。

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

仅检查数值规则可执行 `python3 scripts/check.py --numeric-only`；采用方可加 `--numeric-root PATH`，对其工作区复用同一规则。业务字段和测试用 `f32`；整数计数／时基保持整数。JSON 依赖显式启用 `float_roundtrip`，有用户输入的入口保留原文直接解码，并使用 `eha-config` 数值解析检查非有限、溢出和非零下溢。第三方库与浏览器原生数值不纳入 Rust 源码扫描。
