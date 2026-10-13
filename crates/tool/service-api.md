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

#### P2 精确 DTO（实现后由 capability 启用）

P2 增量 capability：`managed_profiles`、`adapter_discovery`、`connections`、`can_scan`、
`events`、`recordings_read`、`odrive_read`、`read_jobs`、`usb_profiles`、`advanced_can`、`selected_node_connections`。记录开始/结束仍为 P4，P2 不发布该 capability。
所有下述对象均附加通用 envelope；ID 只在当前实例有效。读取接口也须认证。

- `GET /adapters`、`GET /recordings`、`GET /recordings/{id}?file=NAME&offset=DECIMAL`
  返回 HTTP 202 任务对象。`GET /jobs/{job_id}` 返回 HTTP 200 同一任务对象：
  `{job_id,state:"queued|running|completed|failed",data:null|object,error:null|string}`。
  队列忙返回 409 `busy`，任务不自动重试。结果最多保留 16 项，过期 ID 返回 404。
  adapters data 为 `{adapters:[{kind,serial,description,vid,pid}]}`，不公开设备路径；
  解释器不可用通过 failed/error 明示。
- recordings data 为 `{runs:[{id,metadata_json_text:null|string,files:[NAME]}],truncated:boolean}`。
  最多返回 128 项、检查 1024 目录项、每项 metadata 最多 65536 字节、总原文预算 262144 字节；超预算条目为 null 且 truncated=true。不可读 metadata_json_text 为 null，合法 JSON 原文不经过浮点回显。
  下载只能使用该服务列举返回的 ID，NAME 只能是 `metadata.json`、`events.jsonl`、
  `telemetry.csv`，不接受路径。下载 data 为
  `{run_id,file,offset:"0",next_offset:"65536",eof:boolean,content_base64:string}`，
  每批最多 65536 原始字节；offset 为非负 u64 十进制字符串，缺省为 0。客户端按 next_offset
  显式获取后续批次，不能把部分批次报告为完整导出。文件/目录符号链接拒绝。
- `GET /profiles` 返回 `{profiles:[PROFILE],load_error:null|string,busy:boolean}`。
  受管 CAN PROFILE 为 `{transport:"can",id,name,adapter:{kind:"canable2",serial},node:0..127,mode:"fd",expected_uid:null|string}`；
  不提供 autoconnect/default。POST 顶层为 `{api_version,instance_id,profile:PROFILE}`，
  DELETE `/profiles/{id}` body 为 `{api_version,instance_id}`。保存/删除返回任务对象；
  `GET /jobs/{id}` 中完成 data 为 `{profile:PROFILE}` 或 `{id,deleted:true}`。
  同 ID 保存沿用 ConnectionStore UID 绑定规则，不能更换已有绑定 UID。
- `POST /connections` body 仅允许 `{api_version,instance_id,transport:"usb",serial}` 或
  `{api_version,instance_id,profile_id}`。新连接仅允许空闲 owner 的明确候选/档案；
  已断开的会话须显式 reconnect，不能通过 connect 隐式恢复。
  成功返回 `{connection_id,target_id,identity,connection_generation,connected,snapshot_json_text,telemetry_received_age_ms:null|DECIMAL}`；
  connection_id 当前等于 target_id（`usb:SERIAL` 或 `can:N`），不能替代身份/代次。
  路径中的 `{id}` 按单个 URI 段进行 UTF-8 百分号编码，例如 `usb%3ASERIAL`；服务只在分离该段后解码一次，`+` 保持字面含义。非法 `%HH`、非法 UTF-8、解码后的 `/`、反斜线或控制字符返回 400，不解码整个路径或二次解码；响应 ID 仍为原始字符串。
  `GET /connections` 返回 `{connections:[同一对象]}`，仅投影缓存，包含断开会话事实。
  reconnect/disconnect body 精确为 `{api_version,instance_id,expected_identity,connection_generation}`；
  完成返回更新后的上述对象，旧代次立即失效。失败不重连/回落/重发。
  首次 Identity 握手失败可能保留 `identity:null` 的断开上下文；人工 reconnect 必须回显该 null
  和当前代次，服务仅在自己同样没有 Identity 时接受 null，再通过同一连接器完成首次身份核对。
  已知 Identity 绝不接受 null；调用者不能删除身份门禁以跨设备恢复。
  连接/重连仍在唯一 owner 串行执行 SDK 握手，P2 不承诺 P4 的高优先级 Stop 调度。
- `POST /scans` body 为 `{api_version,instance_id,connection_id,expected_identity,connection_generation,nodes:[0..127]}`，
  nodes 非空、不重复、最多 16 项，connection_id 必须是已连接 CAN 会话。
  返回 HTTP 202 `{scan_id,state:"running",nodes:[],errors:[]}`；
  `GET /scans/{scan_id}` 返回 `{scan_id,state:"running|completed",nodes:[{node,identity}],errors:[{node,message,unknown,error_json_text}]}`。
  最多保留 16 项；节点无响应只进入 errors，不抹去成功节点。扫描复用既有共享 CAN 网络和门禁。
- `GET /odrive/devices` 和 `POST /odrive/read` 返回 HTTP 202 任务对象，后者 body
  精确为 `{api_version,instance_id,serial}`。结果通过 `/jobs/{id}` 读取；data 包含
  `{source:"odrive_usb",native_json_text:string}`。失败保留解释器/SDK 原生错误文本，
  不作为 H723 通路、身份或停止证据。独立 ODrive worker 同时最多一个任务。
- `GET /events?after=DECIMAL&limit=INTEGER` 默认 after=0、limit=100，limit 为 1..100。
  返回 `{events:[{cursor,kind,source_id,source_timestamp:{clock,value},received_at,payload,gap,source_gap}],
  cursor,gap,gap_reasons,lost_ranges,truncated}`。cursor/时间/掉样计数均为十进制字符串。
  `source_id` 为含 target_id、连接代次和 Identity 的不透明字符串；客户端以 payload 的
  target_id/connection_generation/identity 关联会话。kind 为 `connection`、`telemetry`、
  `source_gap`、`scan` 或 `job`；连接/scan/job clock 为 host_unix_ms，遥测为 device_monotonic_us。
  telemetry payload 固定为 `{target_id,identity,connection_generation,native_json_text}`；原文为趋势点
  `{cursor,time_us,sequence,position,velocity,force,target_mode,target_values,gap}`，仅供展示，不能重建业务请求。
  connection/source_gap payload 为 `{target_id,identity,connection_generation,connected}`；
  scan/job payload 为 `{scan_id|job_id,state}`。received_at 为 owner 将事实放入服务缓存的主机时间。source_gap 为 null 或
  `{reason,dropped:null|DECIMAL}`；lost_ranges 为 `{first_cursor,last_cursor,count}` 数组，
  只代表服务缓存遗漏，不能推断设备掉样。cursor_ahead/cache_loss/source_gap 均令 gap=true。
  消费不会删除其他消费者事件；limit 不跳过未返回项。遇 gap 必须重取 snapshot。
  缓存最多 4096 事件、估算 4 MiB、保留 30 秒；单事件最大 8192 字节。

`describe.limits` 明示上述 events/job/scan/recording 和有界 HTTP 发送线程上限。HTTP 单次往返使用 Connection: close，不支持连接流水线；发送最多 16 个并发线程，满载先 shutdown 实际 socket。
SDK opt-in bounded HTTP 在 accept 前预留、parser 分派前限制最多 64 个连接 permit，permit 跟随实际读写所有权；
Request 队列同样最多 64，满时关闭 socket。请求行与首部总计最多 16 KiB；chunk 原始编码 body 最多 8 MiB，应用实体限制不变；独立 monitor 从 accept 起
执行 10000 ms 绝对连接截止，涵盖首部、预读、排队、body 和响应。每连接只解析一个请求。
owner 的最多 64 个未完成控制引用与轮询截止仅为第二层保护；同步 SDK 等待不会阻止独立 monitor。
ready 写入 stdout 失败不会终止已监听并发布发现记录的服务。P4 前仍不承诺 SDK 请求可抢占。
客户端不得因关闭或超时自动重发设备动作。
P2 保持 `read_gate: existing_global_readonly_gate`、`control_supported:false`、
`mutation_ledger_supported:false`、`recording_write_supported:false`；无自动连接、心跳、目标或 Stop。

事件是服务采集事实，独立于浏览器绘制、binding 解除和前端轮询。Fieldkit 后端通过受认证的
批次接口投影事件；浏览器只接收插件 Remote 结果。隐藏面板最多停止前端轮询，不能停止服务采集
或记录。连接或重连不会启心跳、恢复目标或重放动作。

### P2 连接补全 DTO（同版本兼容扩展）

`PROFILE` 是带 `transport` 的联合：受管 CAN 在既有字段上增加 `transport:"can"`（保存时兼容省略），
USB 为 `{transport:"usb",id,name,serial,expected_uid:string|null}`。两个档案文件共享 id 命名空间；
加载损坏或跨类型重名汇入原 `load_error:string|null`，拒绝涉及档案的连接/写入，绝不静默覆盖。
GET/POST/DELETE profiles 和完成 job 的原封装不变；USB sidecar 为原档案目录的 `usb-connections.json`。
USB 档案首次连接和每次显式重连均核对 expected UID；未绑定档案首次 Identity 后把 UID 交有界档案
worker 持久化，连接回复额外含 `profile_job_id:string|null`。该 job 完成 data 为 `{profile:PROFILE}`；
持久化失败显示失败，不能解释为持久身份已绑定。当前 Workbench owner 仍保留首次 UID，所有 v1/v2 bindings 和连接入口统一核对以后重连。
档案 id 删除后重建为另一 serial 时，旧会话不能向新档案写入绑定 UID。

高级 CAN 输入 `ADVANCED={channel:string,mode:"classic"|"fd",python:string|null}`，channel 是既有
SDK python-can 配置 context，python 是显式解释器覆盖；复用原 SDK factory/配置和物理 owner lock。
`POST /connections` 新增 `{api_version,instance_id,transport:"can",advanced:ADVANCED,node:number}`，
高级/受管 CAN 连接统一 HTTP 202 标准 JOB，完成 data 为 `{connection:CONNECTION}`；节点 0..127。
网络首次构造交给有界 worker，仅移交唯一 CanNetwork，pending 时拒绝重复打开。owner 收回网络后
才执行 Identity 握手；工厂错误不回落、不自动重试。
无已持有节点且网络已终止时，下一次明确 connect/scan 可将旧网络交给同一 worker 释放后重建；
网络代次递增，旧 scan 失效。有节点的网络不得用此路径替换，须显式原会话重连。USB 连接仍返回 HTTP 200 原 CONNECTION。已存在的 target（包括断连）拒绝，须原上下文显式重连。

`POST /scans` 除原已连接 `connection_id/expected_identity/connection_generation` 外，允许空网络来源
`{api_version,instance_id,advanced:ADVANCED,nodes:number[]}` 或
`{api_version,instance_id,profile_id:string,nodes:number[]}`，三个来源互斥。profile 必须是 CAN。
创建或复用同一个 owner CAN 网络，202 scan 和部分结果封装不变；不自动连接扫描所得节点。

`POST /connections/batch {api_version,instance_id,scan_id:string,nodes:number[]}` 返回 HTTP 202 标准 JOB；
只能从该已完成且仍缓存的 scan 成功项明确选择 1..16 个互异节点。服务保留扫描身份并在首次连接后核对。
owner 每轮最多执行一个节点握手；不在线程中复制 Client，HTTP 请求不等待整批握手。
完成 JOB 的 data 为 `{connections:CONNECTION[],errors:[{node:number,target_id:string,code:string,
message:string,unknown:boolean,error_json_text:string|null}]}`。部分失败仍 `state:"completed"`，保留成功项；
已有 target 包括断连逐项 `binding_stale`，不调用旧批量入口的隐式 reconnect。扫描结果被淘汰、实例改变、
当前网络不匹配或扫描正忙则拒绝。作业、扫描、会话原上限 16 不变，同一时刻最多一个连接批次。
同步单次 SDK 握手仍不能承诺 Stop 抢占；P4 调度门未开放。客户端超时只显式刷新事实，不能自动重发。

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
| `POST /service/v2/control-reservations`、`DELETE /service/v2/control-reservations/{id}` | 明确申请或释放单台控制占用，群组通过 groups 创建。多个观察 binding 可以共存，每台设备至多一个写入者；释放不等于 Stop。|
| `POST /service/v2/bindings/{id}/controls` | 固定 `heartbeat_start`、`heartbeat_stop`、`heartbeat_once`。停止心跳与停止控制是不同动作。|
| `POST /service/v2/bindings/{id}/controls` | 固定 position、velocity、force、impedance 持续目标；到位、UI 超时、解绑、窗口关闭均不结束目标。|
| `POST /service/v2/bindings/{id}/stop` | 显式普通 Stop，走独立高优先级受理路径；其本地提交和后续停止观测分开报告。|
| `POST /service/v2/bindings/{id}/trials`、`POST /service/v2/bindings/{id}/trials/{trial_id}/stop`、`POST /service/v2/bindings/{id}/trials/{trial_id}/target` | 复用已有有界试验、位置更新和 Stop 确认语义。|
| `POST /service/v2/groups`、`DELETE /service/v2/groups/{id}` | 创建或释放固定成员引用；释放不 Stop、不断连。|
| `POST /service/v2/groups/{id}/trials`、`POST /service/v2/groups/{id}/stop`、`POST /service/v2/groups/{id}/controls` | 显式成员集合，逐设备报告准备、提交、停止与未知状态；不承诺设备侧原子性。|
| `POST /service/v2/bindings/{id}/recording/start`、`/recording/stop` | 显式开始/结束服务侧记录；不从绘图状态推断。|

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

Unix 的通路锁目录使用有效 UID 的 OS 账户数据库 home：macOS 为
`<OS-home>/Library/Application Support/eha-tool/owners-v1`，Linux 和其它 Unix 为
`<OS-home>/.local/share/eha-tool/owners-v1`。HOME、XDG_DATA_HOME、XDG_RUNTIME_DIR、
TMPDIR、service dataDir 或 profile 都不能改变该 namespace；配置与服务数据仍可使用 XDG。
账户查询失败、home 非绝对路径、路径 symlink 或 UID/权限不符合私有目录边界时直接拒绝，
不回退到环境变量。升级前必须停止旧服务及直连 owner；旧版自定义 XDG 锁目录与新版不支持
并发互斥，不扫描或兼容任意历史目录。该 advisory 锁不抵抗同 UID 主动删除/替换锁目录、
运行期间管理员改变 OS 账户 home 或外部非合作发送者。

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

### P4 控制 owner 接入合同

以下路由使用唯一 Workbench owner 的有界 continuation，发布前 `control_enabled=false`，不得凭本段启用真实设备控制。v1 的主动读取门禁不改变。所有变更请求继承 `api_version:2` 与 `instance_id`；以下 `R` 表示 `{expected_identity:{uid,run_nonce},connection_generation}`，所有身份和代次为字符串。默认权限为只读；控制授权后预分配 canonical Stop 编号，Stop 不需要调用者持有 writer reservation。

* 单成员 `POST /bindings/{id}/controls`：`R,reservation_id,claimant_id,request_id,command`。`command` 只允许 `heartbeat_start/heartbeat_stop/heartbeat_once`、`position{mm}`、`velocity{mm_s}`、`force{n}`、`impedance{equilibrium_mm,stiffness_n_per_mm,damping_ns_per_mm}`，每项带 `action`。未知字段拒绝。
* `POST /bindings/{id}/trials`：同上公共字段加 `trial`。trial 精确复用 SDK `TrialRequest`：`command`、`envelope`、`duration_s:number|null`、`reach:null|{tolerance_mm:number,settle_ms:u64}`。envelope 必含 `position_min_mm,position_max_mm,velocity_abs_max_mm_s,force_abs_max_n,stiffness_max_n_per_mm,damping_max_ns_per_mm,duration_max_s`。试验 ID 是开始操作的 request_id。开始先异步 Startup/Status 预检，再等待记录审计屏障，最后提交目标。
* `POST /bindings/{id}/trials/{trial_id}/target`：公共字段加 `mm`，原试验编号、成员和代次不可替换；回执须等待 SDK 20 Hz 合并、审计和实际本地提交结果，不能把仅合并目标称作提交。
* `POST /bindings/{id}/recording/start`、`/recording/stop`：仅公共字段；路径由服务选择，不接受用户文件路径。停止记录不请求设备 Stop。
* `POST /bindings/{id}/stop` 与 `/bindings/{id}/trials/{trial_id}/stop`：`R,claimant_id,request_id`，不带 reservation_id。两条路由使用同一 canonical Stop action/编号；trial 路径额外验证原试验。Stop 先在同 UID/run 的所有 alias 上 fence 旧生产者，再于当前有界步骤之后的下一调度步尝试唯一 SDK 提交；不等待普通读取完整超时。重复请求只返回原回执。未认证请求不能占用专用 Stop reader；大于 1024 字节的 Stop JSON 进入独立的容量 8 reader/completion 队列。

控制端点首次接纳返回 202 `{operation,new_submission:true}`；相同原始参数重放返回 200 `new_submission:false`。`GET /operations/{id}` 复用 P3 的唯一账本，动作包括 `heartbeat_start/heartbeat_stop/heartbeat_once/position/velocity/force/impedance/trial_start/trial_target/recording_start/recording_stop/stop`。submission 为 `not_attempted/not_submitted/locally_submitted/unknown`，control_effect 为 `not_attempted/not_applicable/known/unresolved`；known 仅表示该操作本地结果已知，不表示停止或机械安全。未知原操作不可被成功读取、Idle 遥测或新操作覆盖。额外白名单事实为 `phase`、`stop_attempt_delay_ms:string|null`、`trial_id:string|null`、`recording_id:string|null`。

业务数字在 JSON 中是原始 number token；消费者编辑器可保留十进制文本，但发送时必须经严格 JSON 数字词法验证后直接拼接原 token，不经 JS Number 或 binary64。SDK 对 f32 仅一次量化，参数全文参与同 request_id 的一致性判断。

`GET /bindings/{id}/trials/{trial_id}` 返回 `{trial_id,operation,trial:{native_json_text,state,stop_observed,stop_confirmation_overdue,elapsed_ms}}`，elapsed_ms 为字符串或 null；其他白名单字段只投影 SDK 原事实。`GET /bindings/{id}/recording` 返回 `{native_json_text,active,failed,recording_id}`。UI 不重建试验或配置计算。 当前 owner 每个 target 最多保留一个成功成为当前的试验开始回执，活动、完成或人工释放占用后仍保留原账本 projection，超过普通 30 秒保留期也不改写其提交/效果事实。仅同 target 的新试验成功成为当前或该目标会话移出 owner 后解除旧引用；同 UID 的其他 target alias 不互相替换，多成员引用同一回执时须全部解除才可回收。保留回执仍计入 32 个普通操作容量，容量满时拒绝新普通准入；未知原回执无论引用是否解除均按既有规则永久保留。

群组创建 `POST /groups {claimant_id,members:[{binding_id,...R}]}` 必须包含 1..16 个不同物理 UID。一次原子 Book reserve 全部成员；设备提交仍逐台，非原子。响应 `{group_id,reservation_id,claimant_id,stop_request_id,members:[{binding_id,target_id,...R,control_epoch,stop_request_id}]}`，每成员有独立 epoch/canonical Stop；群组 Stop ID 只聚合这些预留范围。

`POST /groups/{id}/operation-ids {claimant_id}` 返回 `{group_id,expires_in_ms:30000,requests:[{binding_id,request_id,control_epoch}]}`。`POST /groups/{id}/controls|trials|recording/start|recording/stop` 请求含 `claimant_id,requests:[{binding_id,request_id,command|trial}]`，记录动作不带 command/trial。`POST /groups/{id}/trials/target` 的每项为 `{binding_id,request_id,trial_id,mm}`。每次必须精确覆盖原始所有成员一次；部分准入或失败逐台记录。群组 trial 全部原成员预检及记录审计屏障成功之前不提交任何目标；实际首目标前再次核对原 writer 状态。

`POST /groups/{id}/stop {claimant_id,request_id}` 使用 group.stop_request_id，始终覆盖原始全部成员，包括当前断连者；响应 202 `{group_id,members:[{binding_id,operation:null|OPERATION,error:null|{code,message}}]}`，无新子提交时 200。普通群组动作采用同一逐台响应。`GET /groups/{id}` 返回完整固定范围、released 与每台最新 operation/error。DELETE 仅释放占用、废止未用票据，不 Stop、不 disconnect，不删除未知原回执或固定 Stop 范围。

存在控制授权、控制等待或未知 target 效果期间，尚未转为有界 continuation 的连接、扫描与旧 /api 入口（含慢 GET）返回明确 busy，targets 只投影已持有目标，不同步枚举；不会把同步 Identity 等待插入 Stop 调度。v2 固定主动 reads 使用同一 Client 的 begin/poll，HTTP 结果仍保持原 DTO；read_permit 与 control_effect 分开。`async_reads` 已随异步读取实现公布；`control,control_groups,trials,recording_write` 仅在控制发布门开放时公布。

原固定范围 Stop 不依赖观察 binding：`GET /control-scopes` 返回 `{scopes:[{stop_request_id,scope:{target_id,expected_identity:{uid,run_nonce},connection_generation,control_epoch},current,control_granted}]}`。`POST /control-scopes/{stop_request_id}/stop` 请求 `{instance_id,claimant_id,expected_scope}`，expected_scope 必须完整匹配上述原对象（另带 api_version）。它使用原明确 target 的唯一 Client；不会从同 UID 的其他观察 alias 选路，不会取列表第一项。跨观察页显示原范围，由人明确选择；当前 scope 不可用则明确拒绝并保留回执，不能换通路重试。capability `control_scopes` 随 control 发布门。


##### P3 维护 owner 精确 DTO（P4 复用共同账本）

本段定义维护与共享权限的 SDK route 和插件线格式。维护只允许固定四项；控制是否开放以 describe 的 control_supported 及对应 capabilities 为准，发布门关闭时 trial/HB/target 写入不开放。所有在线请求均携带 api_version:2、instance_id；绑定路由还须 expected_identity:{uid,run_nonce}、connection_generation（均字符串，与 v2 binding 同形）。普通 v2 envelope/error 复用现合同；未知字段拒绝，不接受任意 method/action。鉴权仍为实例私密凭据；claimant_id 是插件从可信 exec.agent.id 或本地人工会话签发的身份，不接受模型自报身份。

#### Capabilities and limits

新增 capabilities: config_schema, config_validate, maintenance_permissions, maintenance_reservations, operation_ids, maintenance, operation_read, read_config_factory。limitations: mutations_supported=true, maintenance_supported=true, mutation_ledger_supported=true；control_supported 由控制发布门决定，device_execution_confirmation=false。limits: max_maintenance_reservations=16, max_mutation_members=1, max_operation_tickets=32, operation_ticket_ttl_ms=30000, max_normal_operations=32, max_stop_scopes=16, operation_receipt_retention_ms=30000, max_maintenance_readbacks=16。未知记录永久保留并计容量，满时诚实拒绝普通准入。

#### Routes

- GET /service/v2/config/schema：envelope + native_json_text:string（官方 SDK SCHEMA_JSON 原文）、config_format_version:number。离线，不需要 binding。
- POST /service/v2/config/validate：{api_version,instance_id,config_text:string}。原字符串完整 UTF-8 送 SDK validate_json；不 parse/reserialize。成功 envelope + valid:true；失败普通 error code invalid_config + message（SDK 原错）。不写设备。
- POST /service/v2/bindings/{binding_id}/permissions：{api_version,instance_id,expected_identity,connection_generation,expected_grant_id:string|null,permissions:{maintenance:boolean,control:boolean}}。只允许显式人工授权/撤销，模型工具不能调用。返回 envelope + binding_id + target_id + expected_identity + connection_generation + grant_id:string|null + control_epoch:string|null + stop_request_id:string|null + permissions:{maintenance:boolean,control:boolean}。授权是固定 MemberKey。grant_id 是服务签发 instance-grant-N；同权限重复授权保持编号；撤销/权限变更后重新授权必须新编号。reservation 捕获所有 grant_id，票据捕获该 reservation 生命周期；旧 reservation/ticket 不得因重新启用复活。撤销取消尚未提交工作，retire旧reservation/未消费ticket；在途/unknown仍保留原归属，不 Stop、不清未知占用；control_supported=false 时不能授予 control:true。
- POST /service/v2/control-reservations：{api_version,instance_id,claimant_id:string,permission:"maintenance",members:[{binding_id,expected_identity,connection_generation}]}。P3 恰好一台；owner 一次核对所有成员/权限/身份/当前门禁/容量，按 UID 互斥。返回 envelope + reservation_id + claimant_id + permission + control_epoch:string + members:[{binding_id,target_id,expected_identity,connection_generation,grant_id}]。control_epoch 服务签发，不接受 caller 指定。
- DELETE /service/v2/control-reservations/{reservation_id}：{api_version,instance_id,claimant_id}。返回 envelope + reservation_id + released:[target_id] + blocked:[target_id]。release 不 Stop；pending/unknown claim 保持阻挡，不能自动移交。
- POST /service/v2/operation-ids：{api_version,instance_id,reservation_id,claimant_id}。返回 envelope + request_id + reservation_id + control_epoch + expires_in_ms:30000。票据固定 reservation 成员/epoch；计数单调、实例内永不复用；未消费过期号永拒绝，不再执行；签发不提交设备动作。
- POST /service/v2/bindings/{binding_id}/maintenance：{api_version,instance_id,expected_identity,connection_generation,reservation_id,claimant_id,request_id,action,config_text?}。action 仅 save_and_restart、restore_factory_and_restart、reset_application、enter_update；config_text 仅 save 必需，其余禁止。save 原文只经 SDK validate_json，原始全文进入去重同一性。既有 request_id 全参数相同返回原 operation，不重发；异参 conflict；旧/过期号 expired。成功 HTTP202 envelope + operation（以下 projection）+ new_submission:boolean（仅新本地工作，非实际 TX 证明）。重复 HTTP200 且 new_submission:false。开始前禁止 HB/trial/target pending/unknown；factory 读取与 mutation submission 均同唯一 Client begin/poll，不 blocking owner。
- GET /service/v2/operations/{request_id}：envelope + operation。只读取原记录，不重发。unknown/pending 永不被普通 retention 淘汰；已知本地终态 receipt 可在 retention 后返回 expired，旧号仍永不执行。

#### Operation projection

operation:{request_id,reservation_id,claimant_id,action,control_epoch,members:[{target_id,expected_identity,connection_generation}],admission:"queued"|"attempting"|"finished",submission:"not_attempted"|"not_submitted"|"locally_submitted"|"unknown",control_effect:"not_attempted"|"not_applicable"|"unresolved",observation:"unconfirmed",phase:"factory_read"|"submission"|null,native_json_text:string|null,follow_up:{reconnect_required:boolean,readback_views:["user","startup"]},error:{code,message}|null}。

native_json_text 保留 SDK ToolSession CommandResult 或 SessionError 的完整 JSON 原文（含 local_stage、raw SDK failure 与 transport），不得把 bigint 转 JS number；插件只投影白名单字段。queued 无 native。Factory query 的 MayHaveBeenSubmitted 只是读请求：FactoryRead 失败映射维护 submission:not_submitted/control_effect:not_attempted，原错误保留。真正 mutation 的 MayHaveBeenSubmitted/ResultUnknown => submission:unknown/control_effect:unresolved，原 operation+blockedclaim 永久 pin；取消本地等待不撤回 bytes、不重发。身份变化撤销旧 grant/未消费票据，但不删未知审计。后续 Idle/readback 不自动清未知。

成功 local submission => control_effect:not_applicable，仅表示无持续控制需求；observation:unconfirmed，绝不宣称设备保存/复位完成。四维护已知提交都要求操作者明确重连，并读取同一明确选定设备的 User/Startup 作为后续证据；旧 nonce/generation 的授权、reservation、未消费票据不能迁入新连接。EnterUpdate 后不可读时诚实说明不可用，不自动找新设备。有限 retention 只回收已结束且非真实 SDK unknown 的 receipt；未决记录不改名淘汰。

#### Errors

延续 v2 {protocol,api_version,instance_id,code,message}。精确新增 codes: invalid_config、unsupported_permission、permission_denied、permission_conflict、claimant_mismatch、reservation_busy、reservation_unknown、operation_capacity、request_id_expired、request_id_conflict、operation_not_found、maintenance_busy；原 not_connected/binding_stale/identity_changed/invalid_request/instance_mismatch 复用。提交后错误成为 operation 投影，不把已准入 operation 改成无记录 HTTP 错误。HTTP202/200 仅说明服务受理/回执读取，未知不自动重试。

固定 Factory 读取通过现 v2 bindings/{id}/reads action:config_factory；User/Startup保持现有固定actions。v1能力不扩大，读取原门禁不放松。

维护已知提交的后续读回是 SDK 准入门槛：固定原 target_id、同 UID 的新 connection_generation 与重新核对的 run_nonce，成功 User 和 Startup 两固定读取必须属于同一完整新 MemberKey，才解除后续维护授权/占用阻挡。跨 alias 或旧代次读不解除；读取不修改原 operation observation/unresolved，不判断配置等于保存原文，差异完整保留供操作员核对。义务按 UID 有界（max_stop_scopes），不增加无限审计。未发送队列工作被显式释放/撤销后，精确 operation_id 的 NotSubmitted 完成可安全回收 released claim；真正 submitted/unknown 与 prior unresolved 保持原保留规则。

P4共享权限响应：control开启且门槛完成时由服务提前预留canonical Stopslot，permissions响应control_epoch/stop_request_id可在无reservation情况下供显式人工Stop；P3control=false时两者null。双权限属于同一个服务grant，任何权限变更更换编号并使旧占用/票据失效，不能合并成两个独立授权。

权限读取与CAS：GET /service/v2/bindings/{id}/permissions 返回当前完整权限、grant_id、control_epoch、stop_request_id及固定binding身份，无device动作。POST 必需 expected_grant_id:string|null 与当前grant一致；第一次null、已存在则真实编号。CAS冲突HTTP409 permission_conflict且不改权限；相同flags幂等保留grantID。插件先GET核对固定绑定，仅修改人类明确请求的位，再POST完整flags+CAS；冲突不自动覆盖，不把另一位重置为false。P3cap maintenance_permissions涵盖GET/POST。

Readback quota在维护动作admission前预留：已保留义务+pending维护预留总数不超过16，旧scope退休不删除义务；满则普通拒绝、不提交SDK、旧票据作废不会在容量恢复后执行。已知完成转为原UID义务；NotSubmitted/真实Unknown仅释放尚未成为义务的预留容量，未知operation/claim另保留永久pin，绝不丢未知审计。


### 原范围本地收尾（P4，随 control_scopes 发布门）

Stop fence 后仍可由操作者明确停止原会话的本地心跳或记录，保持原 MemberKey、原控制授权和服务签发请求 ID。此操作不提交设备 Stop，不重连、不切换 Client、不取得或转移 writer claim，不修改任何旧 unknown 回执或 claim。失联或身份/代次变化返回 `binding_stale`。

- `POST /service/v2/control-scopes/{stop_request_id}/cleanup-ids`：请求 `{api_version:2,instance_id,claimant_id,expected_scope}`，其中 expected_scope 与原范围 Stop 相同。HTTP 200 data 为 `{request_id:string,expires_in_ms:30000,grant_id:string}`。票据绑定原 MemberKey、claimant、grant ID，使用共享 normal 32 容量，30 秒后过期；授权撤销或轮换不可复活旧票据。
- `POST /service/v2/control-scopes/{stop_request_id}/cleanup`：以上请求另加 `request_id:string,action:"heartbeat_stop"|"recording_stop"`。只能停止本地心跳或记录；其他动作及把此票据用于普通控制入口均拒绝。无 reservation_id。首次 HTTP 202、同一会话精确重放 HTTP 200，data 均为 `{operation,new_submission}`。本地成功回执 `submission:"not_submitted",control_effect:"not_applicable"` 表示未提交设备命令，原文保留本地 SDK 结果。

所有 normal 票据均由共同准入层捕获用途、原完整 MemberKey/control_epoch、claimant、服务 grant ID 与期限。`operation-ids` 及群组子票据还固定原 reservation ID 和 permission；只能用于该 permission 对应的动作，不能改占用或请求会话。cleanup 票据仅用于上述两个无 reservation 的本地收尾动作，不可进入维护、普通控制或群组设备动作。共同票据校验对当前票据跨用途或归属不一致返回 `request_id_conflict`；授权撤销/轮换、原占用释放或期限到期后返回 `request_id_expired`，旧 ID 永不重新执行；入口更早的身份、授权或占用门禁仍保留其固定拒绝码。所有维护、控制、群组及本地收尾入口执行同一校验，再消费编号并建立回执；已有回执的精确重放只读取原记录，不重新验证为一次新提交。票据拒绝不改变任何在途或 unknown operation/claim，不 Stop、不提交设备字节。

排队与执行每一步核对原 grant ID，撤权取消未开始的收尾；release 本身不执行收尾。当前同 UID 有控制/维护等待动作时返回 `control_busy`，操作者可等该原动作明确结束后重新签票操作。Stop 保持独立优先准入。群组可使用各原成员明确显示的原范围执行同样收尾，不缩减群组 Stop 范围。
