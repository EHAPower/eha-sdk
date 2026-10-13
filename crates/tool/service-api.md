# eha-tool 本机设备服务 API

本文件是 `eha-tool` 服务接口的唯一规范。客户端不得从旧 WebUI 路由、HTML 或
SDK 私有状态推断服务行为。

## 版本与安全边界

- 基础路径：`/service/v2`。
- 成功和错误响应均含 `protocol: "eha-tool.service.v2"`、`api_version: 2` 与
  `instance_id`。`instance_id` 仅在该服务进程存活期间有效。
- 服务仅监听回环地址，所有服务请求使用 `Authorization: Bearer <token>`，并拒绝**任何**
  `Origin` 请求。浏览器和模型不持有此 token，Fieldkit 后端代表它们请求服务。
- 每个 POST JSON 对象显式携带 `api_version: 2`。未知字段、重复顶层字段和不支持的
  capability 都是请求错误，服务不会猜测默认写入动作。
- `instance_id`、绑定身份和连接代次是防止旧上下文使用新设备会话的并列门禁；服务
  重启、重连或 UID/run nonce 变化后，旧 binding 和旧请求必须拒绝，不能自动重放。
- HTTP 受理、本地提交、收到协议回复、后续观测和设备效果是不同事实。`2xx` 从不
  表示设备已采用维护或控制动作。

`GET /describe` 的 `capabilities` 只列当前实现；本文后续阶段的端点仅为已冻结的规范，
未出现在该数组之前不得调用。客户端遇到未知 `api_version`、缺少必需 capability 或
不同 `protocol` 必须拒绝该功能，不能降级为直连设备。

## 通用对象

```json
{
  "protocol": "eha-tool.service.v2",
  "api_version": 2,
  "instance_id": "i-..."
}
```

`identity` 固定为 `{ "uid": "...", "run_nonce": "..." }`；
`connection_generation` 是十进制字符串。`target_id` 当前采用 `usb:<完整序列号>` 或
`can:<节点号>`，只可用作服务实例内的引用，不能替代实际 Identity。

错误响应在通用对象外增加 `code` 与 `message`；可能发生本地提交的失败还保留结构化
SDK 事实与 `unknown`。常用代码包括 `bad_request`、`not_found`、`busy`、
`instance_mismatch`、`binding_stale`、`identity_changed`、`read_not_ready`、
`owner_busy`、`sdk_timeout` 和 `sdk_error`。

## P1：当前实现范围

P1 的 capability 名称为 `targets`、`binding_attach`、`usb_connect`、`snapshot`、
`read_status`、`read_measurements`、`read_diagnostics`、`read_config_startup` 与
`read_config_user`，以及 `binding_lease`、`binding_renew`、`instance_status`、`service_shutdown`。只有这些 capability 可以在 P1 出现在 `describe.capabilities`。

| 请求 | P1 语义 |
| --- | --- |
| `GET /service/v2/describe` | 返回通用对象、`tool_version`、配置格式版本、上述精确 capability、请求/响应大小、SDK 读取超时和退出响应等待上限（`shutdown_response_grace_ms: 500`）。|
| `GET /service/v2/instance` | 返回当前服务实例、会话与活动/未决效果摘要，详见下文。|
| `POST /service/v2/shutdown` | 当前实例匹配且固定确认文本正确时受理服务退出；不请求设备 Stop，详见下文。|
| `GET /service/v2/targets` | 枚举可见 USB 候选和当前 owner 会话；返回发现受限原因，不执行隐式 CAN 扫描、心跳或连接恢复。|
| `POST /service/v2/bindings` | 以 `intent: "attach"` 绑定既有会话，或以 `intent: "connect"` 显式连接已枚举 USB。输入始终包含 `api_version`、`instance_id`、`target_id`；attach 还含 `expected_identity` 与 `connection_generation`。可选整数 `lease_ms` 申请有界观察租约；返回 binding、Identity、当前代次和 `lease_ms`（省略租约则为 null）。|
| `DELETE /service/v2/bindings/{id}` | 只解除该调用者观察；不关闭物理会话、不 Stop、不改变心跳或目标。|
| `POST /service/v2/bindings/{id}/renew` | 只刷新现存观察租约；请求含 `api_version`、`instance_id`、`expected_identity`、`connection_generation`。成功返回绑定通用对象及原 `lease_ms`；不发设备读取/动作。|
| `GET /service/v2/bindings/{id}/snapshot` | 返回已验证 binding 的服务缓存和数据年龄；不发主动查询。|
| `POST /service/v2/bindings/{id}/reads` | 执行一个固定的只读动作。输入除 `api_version` 外必须回显 `instance_id`、`connection_generation`、`expected_identity` 与 `action`。P1 动作为 `status`、`measurements`、`diagnostics`、`config_startup`、`config_user`。|

观察租约仅回收 binding 表中的调用者观察引用，绝不关闭 owner、停止心跳、释放控制占用或请求设备 Stop。
`describe.limits` 声明 `binding_lease_min_ms: 1000` 与 `binding_lease_max_ms: 121000`；
`lease_ms` 是此范围内的 JSON 整数。服务创建 binding 时按自身单调时钟建立截止时间；
每次 v2 路由访问前清除到期观察，再检查容量和绑定引用。租约不会因 snapshot/read 自动延长。
`renew` 需匹配仍存在的 binding、当前实例、owner 的 Identity/代次和请求回显，并从处理时刻
续接原长度；过期 binding 返回 `not_found`，不能复活或重新连接。没有租约的 binding
不无故到期，renew 返回 `unsupported`。旧 v1 线格式与持久观察生命周期保持兼容。
新的长观察客户端应申请租约并在后台显式续约（Fieldkit 使用120000 ms租约、最多30秒续约间隔）；
续约失败需结束该观察上下文，不能隐式重建 binding 或恢复设备动作。旧观察引用含本实例ID和
单调不复用序号，序号耗尽时在连接或附着前以 `busy` 拒绝；迟到/重复 DELETE 只能解除原观察，不能释放新实例或后来创建的观察。

snapshot 和 read 响应都回显 `binding_id`、`target_id`、`connection_generation`、
`identity`。read 还返回设备来源时间（若协议回复提供）、接收年龄与原生 JSON 文本；配置
读取同时可携带 UTF-8/十六进制的原始记录。业务数值保持原始十进制输入的一次 binary32
采用规则，显示文本不参与重建输入。

P1 在既有 `unresolved_target`、试验、心跳和 transport 门禁下维持只读限制。响应分别含：

```json
{
  "read_permit": { "ready": false, "reason_code": "unresolved_target" },
  "control_effect": { "state": "unknown", "reason_code": "unresolved_target" }
}
```

`read_permit` 只表示调度器此刻是否能安排查询；`control_effect` 表示此前动作效果是否
仍未知。查询成功不得清除 `control_effect`。没有 mutation 账本的 P1 可报告
`control_effect.state: "not_observed"` 且 `reason_code: null`，但不能伪造安全、停止或
设备执行结论。

## 独立服务发现与人工退出

`eha-tool serve --port PORT --token-file ABSOLUTE_PATH [--instance-file ABSOLUTE_PATH]`
只监听 `127.0.0.1`，不自动连接、启心跳、恢复或重放设备动作。发现记录默认位于：

- 设置 `XDG_DATA_HOME` 时：`$XDG_DATA_HOME/eha-tool/services-v2/port-PORT.json`（所有 Unix 平台）。
- 未设置时，macOS：`~/Library/Application Support/eha-tool/services-v2/port-PORT.json`。
- 其他 Unix：`~/.local/share/eha-tool/services-v2/port-PORT.json`。

本节的严格路径规则适用于独立 `serve` 与其客户端发现读取。旧 `webui
--integration-token-file PATH` 保留相对路径和普通父目录的兼容入口，仍要求 token 本身是
当前 UID 的 `0600`、非硬链接普通文件，打开最终文件时拒绝符号链接。该兼容入口不
发布独立服务发现记录，也不放宽 `serve` 或发现客户端的私有目录规则。

显式 `--instance-file` 与 `--token-file` 必须是无父目录跳转的绝对路径。各自父目录
必须属于当前 UID 且权限 `0700`；祖先目录必须是当前 UID 或 root 所有且不能由其他
用户修改（root 所有的 sticky 目录除外）。任何符号链接组件、不安全权限、非普通
文件、硬链接或不同 UID 的私有文件都拒绝；发现记录路径不能与 token 文件相同。新 token 文件为 `0600`、64 字节小写
十六进制随机值加一个换行；已有损坏或空文件拒绝，不能自动覆盖。凭据内容及
`token_file` 定位只供本机 CLI/Fieldkit 后端使用，不能传入浏览器、模型或报告。

监听成功、SDK owner 核心准备完成后才原子发布 `0600` 发现 JSON，再向 stdout 输出
一行机器可读 ready JSON。端口占用或凭据校验失败不改动已有发现记录。发布期间
持有同目录 `<发现文件名>.lock` 的内核文件锁；其 `0600` 文件保留以防 inode 锁分裂。
正常退出仅删除仍属于同一 `instance_id` 的发现记录，绝不删除已被另一实例替换的记录。
异常终止可能留下记录；文件存在、PID 存在或 PID 复用都不是存活证明。

发现 JSON 的完整字段如下；时间和 PID 均为十进制字符串，endpoint 固定为回环 HTTP
origin，没有查询、片段或任意路径：

```json
{
  "protocol": "eha-tool.service.v2",
  "api_version": 2,
  "tool_version": "4.0.0-dev.3",
  "instance_id": "i-...",
  "pid": "12345",
  "started_at_unix_ms": "1791820800000",
  "endpoint": "http://127.0.0.1:PORT",
  "token_file": "/absolute/private/token"
}
```

ready JSON 包含通用对象以及 `event: "ready"`、`host: "127.0.0.1"`、整数 `port`、
`endpoint` 和 `instance_file`；不包含 token 或 token 文件定位。客户端附着时先安全
读取私有文件，再用 token 请求 `GET /service/v2/describe`；必须验证 protocol、
API 版本、所需 capability 以及响应 `instance_id` 与发现记录完全相同。认证失败、
版本不符、实例不符或连接失败均拒绝附着，不能依据 PID 接受残留记录，不能降级为
另一条设备直连或自动重放未知副作用。

`GET /service/v2/instance` 返回通用对象及 `tool_version`、十进制字符串 `pid`/
`started_at_unix_ms`、`endpoint`、`service_state`（`running` 或 `shutting_down`）。
`sessions` 为缓存投影数组，每项只有 `session_id`（`usb` 或 `can:N`）、`connected`、
`heartbeat_active`、`trial_state`、`recording_active`；不枚举设备、不主动读取协议，
不返回凭据、token 文件定位、记录目录、连接档案或驱动私有路径。
`activity` 含 `trial_active`、`trial_unresolved` 与 `read_blocker`；`control_effect`
含 `state`、`reason_code` 和 `mutation_ledger_supported: false`。P1 仅能投影已存在的
未决目标 latch：有 latch 时 `unknown/unresolved_target`，否则 `not_observed/null`。
这些字段不能证明历史动作完整可追溯、设备 Idle 或机械停止。响应始终包含
`device_stop_requested: false` 和明确的退出影响提示。

`POST /service/v2/shutdown` 只接受以下完整 JSON；缺少字段、未知字段、重复字段、
字段类型不符、错误 API 版本或错误确认文本返回 `400 bad_request`，实例不符返回
`409 instance_mismatch`，认证规则与所有服务请求相同：

```json
{
  "api_version": 2,
  "instance_id": "i-...",
  "confirmation": "shutdown_service_without_stopping_devices"
}
```

受理后返回 `202`，结构为上述 instance 摘要，加 `accepted: true` 与
`service_state: "shutting_down"`。它只表示服务受理退出，不能表示设备收到 Stop、
目标撤销或机械停止。活动心跳、试验、记录和未决效果保留在响应中供人工判断。
服务在 response 线程安排成功后停止安排新的设备工作，响应发送完成即退出；最多
等待 `500 ms`，慢客户端不能让退出无限等待，也不阻塞 owner 调度线程。响应交付
为有界尽力而为，超时/断连后客户端可能不知道受理结果，不得自动重发设备动作。
清理和断开只释放主机资源，不隐式发送 Stop；不 kill 其他进程、不持久队列、不以
同一个 token 自动重连、恢复控制或重放动作。客户端关闭不自动调用此端点，退出
必须由人工显式操作。关闭服务与设备机械停止是两个独立事实。

`describe.limitations` 真实声明 `shutdown_stops_devices: false`、
`shutdown_response_delivery: "bounded_best_effort"`、
`instance_liveness: "authenticated_describe_instance_match"` 和
`mutation_ledger_supported: false`。`instance_status` 与 `service_shutdown` 属于本机
服务生命周期能力，不改变 `mutations_supported: false` 或 `control_supported: false`。

## 已冻结、尚未实现的阶段接口

以下路径和语义供后续阶段使用；它们不是 P1 capability，也不得用已有 v1 或页面路由
冒充实现。除列为纯 GET 的接口外，所有服务请求使用上述版本、实例与认证规则；涉及已连接
设备的请求还必须带 binding、连接代次和 Identity。

### P2：连接、观察、事件、记录和 ODrive 只读

| 请求 | 冻结语义 |
| --- | --- |
| `GET /service/v2/adapters`、`GET /service/v2/profiles` | 返回受支持适配器和当前用户连接档案；不打开另一条设备会话。|
| `POST /service/v2/profiles`、`DELETE /service/v2/profiles/{id}` | 保存或删除档案，沿用 UID 绑定和删除规则。档案锁不是设备 owner lock。|
| `POST /service/v2/connections` | 显式选择 USB 候选或受管 CAN 档案，并只执行连接与 Identity 核对。|
| `POST /service/v2/connections/{id}/reconnect`、`POST /service/v2/connections/{id}/disconnect` | 显式物理会话管理；不等于观察解绑、Stop、心跳停止或目标恢复。|
| `POST /service/v2/scans`、`GET /service/v2/scans/{id}` | 对明确 CAN 通路和节点范围执行受限扫描；不隐式全网扫描，且复用试验/控制门禁。|
| `GET /service/v2/events?after=CURSOR&limit=LIMIT` | 有界游标批次，返回递增序号、来源时间和 `gap`/丢失标记。上限、缓存水位和保留期由 `describe` 声明；游标越界必须报告缺口，客户端重新取 snapshot，不能伪造完整记录。|
| `GET /service/v2/recordings`、`GET /service/v2/recordings/{id}` | 只按服务返回的记录 ID 列举或导出，不能接受任意文件路径。慢下载不阻塞采集、记录或 Stop。|
| `GET /service/v2/odrive/devices`、`POST /service/v2/odrive/read` | 仅 ODrive USB 枚举/读取；不能证明 H723 CAN 通路或扩大为控制能力。|

事件是服务采集事实，独立于浏览器绘制、binding 解除和前端轮询。Fieldkit 后端通过受认证的
批次接口投影事件；浏览器只接收插件 Remote 结果。隐藏面板最多停止前端轮询，不能停止服务采集
或记录。连接或重连不会启心跳、恢复目标或重放动作。

### P3：配置和维护

| 请求 | 冻结语义 |
| --- | --- |
| `GET /service/v2/config/schema` | 返回 SDK 发布的 schema；浏览器不维护第二套校验规则。|
| `POST /service/v2/config/validate` | 对完整原始配置文本做离线校验，不要求设备连接。|
| `POST /service/v2/bindings/{id}/maintenance` | 固定 `action` 仅为 `save_and_restart`、`restore_factory_and_restart`、`reset_application`、`enter_update`。`enter_update` 不等于镜像传输或新应用验证。|

配置和维护请求保留用户输入的原始十进制文本或完整原文，由 Rust 按 SDK 数值规则一次采用；
浏览器显示值、JSON 浮点回显和格式化文本不得重建请求。维护响应分别记录受理、本地提交、
后续读取/观测与未知结果；没有固件操作键或历史设备结果查询时，服务不得制造“保存成功”回执。

### P4：控制、占用、试验和服务侧记录

| 请求 | 冻结语义 |
| --- | --- |
| `POST /service/v2/control-reservations`、`DELETE /service/v2/control-reservations/{id}` | 明确申请或释放单台/群组控制占用。多个观察 binding 可以共存，每台设备至多一个写入者；释放不等于 Stop。|
| `POST /service/v2/bindings/{id}/heartbeat` | 固定 `start`、`stop`、`once`。停止心跳与停止控制是不同动作。|
| `POST /service/v2/bindings/{id}/target` | 固定 position、velocity、force、impedance 持续目标；到位、UI 超时、解绑、窗口关闭均不结束目标。|
| `POST /service/v2/bindings/{id}/stop` | 显式普通 Stop，走独立高优先级受理路径；其本地提交和后续停止观测分开报告。|
| `POST /service/v2/bindings/{id}/trials`、`POST /service/v2/bindings/{id}/trials/{trial_id}/stop`、`POST /service/v2/bindings/{id}/trials/{trial_id}/target` | 复用已有有界试验、位置更新和 Stop 确认语义。|
| `POST /service/v2/groups`、`DELETE /service/v2/groups/{id}` | 创建或释放固定成员引用；释放不 Stop、不断连。|
| `POST /service/v2/groups/{id}/trials`、`POST /service/v2/groups/{id}/stop`、`POST /service/v2/groups/{id}/heartbeat` | 显式成员集合，逐设备报告准备、提交、停止与未知状态；不承诺设备侧原子性。|
| `POST /service/v2/bindings/{id}/recording` | 显式开始/结束服务侧记录；不从绘图状态推断。|

权限在服务后端检查，默认只读。用户只在明确对指定目标启用维护或控制能力后，固定表单和对话
才可调用对应接口；对话的可信归属来自 Fieldkit `exec.agent.id`，不能由模型参数替换。binding、
token 和 reservation 不组成多用户认证系统，且不声称约束外部 CAN 发送者。目标、维护和试验还须
匹配有效 control reservation；人工 Stop 不被另一对话占用阻挡，但仍核对身份、代次和原试验
Stop 路径。

所有可能造成设备副作用的 P3/P4 请求共同携带 `request_id`、`instance_id`、binding、Identity、
连接代次、固定 action 及原始参数。请求同一性包含原始数值文本：同实例同编号同参数返回既有
操作记录，同编号不同参数拒绝，实例变化后旧请求拒绝且绝不跨进程重放。操作记录的容量、水位与
保留期由 `describe` 声明；未决记录不得淘汰，容量不足时必须拒绝普通提交，不能因遗忘去重事实
再次执行旧动作。人工 Stop 的受理不得被普通队列或账本容量挤占。取消只对尚未提交的请求或后续
读取有效；已提交设备动作没有通用撤销含义。

### P5：交付验收

P1 已定义的服务发现、启动与附着继续是服务设置的实现要求；P5 只验证其与官方 DSH、插件安装、
升级和完整工作流的交付组合。P5 不新增第二套设备协议、宿主补丁或静默回落直连。

## 所有权与兼容性

每个物理通路只有一个服务 owner：USB 使用完整稳定序列号；受管 CAN 使用适配器稳定
USB 序列号；高级 CAN 使用规范化 channel/resource。节点、模式、profile 文件或 Python
解释器不能绕过同一物理通路占用；无法识别的外部别名是明确限制。CLI 默认保持一次性直连，
取得 owner lock 后才打开 Backend；只有显式 `--via-service` 能复用服务会话，失败时不回落
直连。旧 `webui` 是同一服务核心的兼容入口，不能再创建第二个持有者。

CLI 精确形式为 `device --via-service <absolute-instance-file> --target-id <target-id> ACTION`。
`--via-service` 和 `--target-id` 必须同时出现，且与 `--usb`、`--connection`、`--can-channel`、
`--node`、显式 `--can-mode`、`--can-python` 互斥。默认没有 `--via-service` 时保持单次直连。
P1 只映射 `status`、`measurements`、`diagnostics`、`config-read --view startup|user`，
Factory、`config-read --output` 和其它动作均为 `unsupported`，不会尝试旧页面路由。
共享 CLI 输出成功读取的 `native_json_text` 原文，不通过 JSON Value 重建设备数值。
它先认证 describe 并比对描述符实例，再按精确 target_id 取既有会话的 Identity/代次，
以 attach 绑定并在读取请求回显版本、实例、Identity 和代次；每份回复再次核对这些绑定事实。
HTTP 限定 literal loopback IP、固定路径、无代理/Origin/redirect、16 KiB 响应头和 1 MiB 响应体，
`--timeout-secs` 范围 1 至 60，默认 5，每次请求独立限时且不重试。CLI attach 必须申请 `2 × timeout-secs × 1000 + 1000` ms观察租约，
并先确认服务公布 `binding_lease`。取得可靠ID后，成功或任何读取失败都只对该当次实例ID进行
一次有界 best-effort DELETE；从不重试原读取。attach 回复未知而没有ID时，服务在租约到期后
有界回收观察，不依赖服务重启。解除观察不改变 owner、心跳、目标或设备 Stop，且共享通路
任何失败都不能转向直连。

`/integration/v1` 在其公布的兼容期内保持原协议。它不自动升级为 v2，也不因删除 HTML
而提前破坏。P6 删除旧页面前，所有仍受支持的客户端都必须已迁移或有明确发布处置。
