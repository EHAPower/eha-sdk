<!-- Copyright The eha-sdk Contributors -->

# `eha-config`

## 职责与依赖

`eha-config` 是硬件无关的 `no_std` 配置记录库。它唯一拥有配置字段类型、记录版本、JSON
编解码、JSON Schema 文本和液压速度到泵转速的纯换算。它只依赖 `serde` 与
`serde-json-core`，不依赖固件、存储、同步原语、平台或产品出厂配置。

## 公开接口与使用

crate 根导出 `Config` 及各字段类型、`FORMAT_VERSION`、`MAX_JSON_LEN`、`JsonError` 和
`hydraulic_velocity_to_rpm`。`Config::from_json` 与 `Config::write_json` 处理完整的版本化
JSON 记录；使用方负责该记录来自何处、何时生效及是否适用于设备。

启用 `schema` feature 时，`SCHEMA_JSON` 提供同目录 [`schema.json`](schema.json) 的原始文本，
供主机校验使用。固件不启用此 feature，因此不链接 Schema。

`hydraulic_velocity_to_rpm` 将线速度（mm/s）、有效面积（mm²）和泵每转排量（cm³/rev）换算为
不含安装方向的 rpm。它保留速度符号，只拒绝 NaN、无穷输入或中间结果；静态正值约束、保护范围
和输出授权由调用方处理。

## 实现约定

记录有固定的最大长度，不分配内存，也不保存原始 JSON。解析拒绝未知或缺失字段、非有限数值和
不支持的版本；编码只写入调用方提供的缓冲。

Schema 使用标准关键字，通过 `integer`、`number` 等类型唯一维护单字段的固定范围；参数说明和
单位写在 `description`。Schema 不包含 `default`、项目私有关键字或产品数据。完整静态检查由
[SDK 客户端](../../README.md#配置检查)维护。

该 crate 不读取或写入存储，不填充默认值，不选择用户或出厂记录，不发布启动快照，也不判断
设备匹配或维护许可。

## 错误与结果边界

`JsonError` 只说明长度、版本或 JSON 表示错误，不代表业务参数适用性。`Config` 编解码失败不会
访问存储或产生控制副作用。液压换算的 `HydraulicConversionError::NonFinite` 只说明计算不能得到
有限的 `f64` 结果。

## 代码与验证入口

| 文件 | 职责 |
|---|---|
| [`src/lib.rs`](src/lib.rs) | 记录类型、版本、JSON 编解码及公开导出 |
| [`src/control.rs`](src/control.rs) | 控制字段与液压速度换算 |
| [`src/measurement.rs`](src/measurement.rs) | 测量字段 |
| [`src/policy.rs`](src/policy.rs) | 保护与运行时效字段 |
| [`schema.json`](schema.json) | 配置 Schema 文本 |

`src/lib.rs` 的测试覆盖不完整、非有限或版本不兼容的记录拒绝；`src/control.rs` 的测试覆盖换算符号和
非有限结果。
