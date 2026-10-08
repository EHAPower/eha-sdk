<!-- Copyright The eha-tool Contributors -->

# 贡献指南

`eha-tool` 是独立公开仓，依赖同级目录中的 `../eha-sdk` 工作区，不依赖固件仓、产品配置或设备输入。
将两个仓库克隆到同一父目录后，在工具仓执行检查。命令、选项和默认值由 `eha-tool --help`
及子命令帮助维护。

## 依赖

按用途自行安装所需依赖：

| 用途 | 依赖 |
|---|---|
| Cargo 构建 | Git、Rust stable、Cargo、原生链接器、相邻 SDK；Rust 包由 `Cargo.toml`／`Cargo.lock` 管理 |
| 完整软件检查 | Node.js 24、Clippy、rustfmt |
| 更新前端 vendor 资源 | Node.js 24、npm；前端包由 `webui/package.json`／`package-lock.json` 管理 |
| 已构建工具运行 | 匹配主机系统和架构的可执行文件；页面资源已内嵌，EHA 操作不需要 Python 或 Node.js |
| ODrive USB 独立只读（可选） | [SDK 的 ODrive 依赖](https://github.com/EHAPower/eha-sdk/blob/main/CONTRIBUTING.md#依赖)；用 `--python`、`--odrive-python` 或 `EHA_ODRIVE_PYTHON` 指定解释器 |

## 构建与检查

```sh
# 将公共 SDK 克隆到相邻目录
git clone https://github.com/EHAPower/eha-sdk.git eha-sdk
# 克隆工具仓
git clone https://github.com/EHAPower/eha-tool.git eha-tool
# 进入工具仓；后续命令均在此执行
cd eha-tool
# 构建当前主机的 release 工具
cargo build --release --locked
```

产物位于 `target/release/eha-tool`（Windows 为 `eha-tool.exe`）。可直接用产物路径运行，
或按下列命令安装到 Cargo 的可执行文件目录，并将该目录加入 `PATH`，以使用 README 中的
`eha-tool` 命令。主机产物须匹配实际运行系统与架构。

```sh
# 将当前工具安装到 Cargo 的可执行文件目录
cargo install --path . --locked
```

完整软件检查直接执行以下命令；CI 使用同一组命令，不访问设备：

```sh
# 安装 Rust 格式与 lint 工具
rustup component add clippy rustfmt
# 检查 Rust 格式
cargo fmt --all -- --check
# 运行 Rust 测试
cargo test --all-targets --locked
# 运行 Web UI 遥测测试
node --test webui/tests/*.test.js
# 检查 Rust lint，并将警告视为错误
cargo clippy --all-targets --locked -- -D warnings
# 构建 release 工具及内嵌页面
cargo build --release --locked
```

已有 vendor 资源可直接构建，不需要先安装 npm 依赖。设备测试需要另行明确对象、参数和结果证据。

## 前端资源与版本

修改 Web UI 图表依赖后，安装锁定依赖并更新内嵌资源：

```sh
# 进入前端目录
cd webui
# 安装锁文件中的依赖
npm ci
# 更新图表资源及其第三方许可证
npm run vendor
# 返回工具仓
cd ..
# 将更新后的资源编入工具
cargo build --release --locked
```

一并维护前端清单、锁文件、静态资源及许可证；第三方资源保留原始版权声明。
工具版本独立维护在 `workspace.package.version`；修改后刷新自己的锁文件。
相邻 SDK 的版本变化时也执行下列命令，不要求同步提高工具版本：

```sh
# 刷新工作区包及相邻 SDK 的锁定记录
cargo update --workspace --offline
```
