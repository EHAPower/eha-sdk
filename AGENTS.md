# SDK 协作约定

- 从 [README](README.md)按使用方式进入资料；公共行为归 README，精确编码归三份[通信合同](docs/通信协议/公共应用通信协议.md)，API 参数与生命周期归 rustdoc，开发检查归[贡献指南](CONTRIBUTING.md)。每项规则只在所属位置完整维护，其他位置概述并链接。
- 配置字段、schema 与纯换算归 [`eha-config`](crates/config/README.md)，完整静态检查归 SDK；固件算法、板级接入、存储实现和产品出厂参数留在 firmware。公开资料必须能在独立 SDK 检出中使用。
- [`eha-tool`](crates/tool/README.md) 的 CLI、Shell、WebUI 复用 SDK，不复制协议、配置检查或固件决策。公开 API 或结果语义变化须说明兼容影响；区分本地提交、收到回复和设备效果，未知副作用不得自动重放。
- 技能或脚本指引失效时，在已授权范围内修正并验证原失败步骤；按实际失败场景选择检查，不为简单编排新增测试框架。
