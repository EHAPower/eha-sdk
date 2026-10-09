<!-- Copyright The eha-sdk Contributors -->

# `eha-tool`

`eha-tool` 是 EHA 的官方命令行工具，提供单次命令、交互式 Shell 和本机 WebUI。它作为
[`eha-sdk`](../../README.md) 工作区中的 crate，访问明确选择的控制器；WebUI 可共享一个 CAN 适配器连接多个节点。协议、配置格式和客户接入说明由 SDK
维护。本 crate 维护工具的使用、输出和本机资源边界。

构建、安装和检查入口见[根贡献指南](../../CONTRIBUTING.md)。下文示例假设已将与当前主机系统和架构匹配的
`eha-tool` 放入 `PATH`；完整的选项、默认值和版本以 `eha-tool --help` 与对应子命令的 `--help`
为准。

## 使用方式

### 命令行

单次 CLI 为每个 `device` 动作新建本地会话、核对 Identity、执行一个动作，然后只关闭本地句柄。单次
调用返回后不保留本地会话或心跳；`heartbeat-test` 与 `velocity-test` 的显式编排见后文。无参数仅在标准
输入和标准输出都是终端时进入交互 Shell；在管道或重定向中无参数只显示帮助。先枚举候选，再以完整 USB
序列号，或 CAN 串口、节点和实际采用的配置组选择对象。枚举不打开设备或扫描总线，也不等于设备已连接
或 Identity 已核对。

```sh
# 查看完整命令帮助
eha-tool --help

# 显示本机工具版本和构建信息，不访问设备
eha-tool --version

# 检查完整本地配置，不访问设备
eha-tool config-check ./candidate.json

# 列出 USB 候选设备
eha-tool devices

# 为自动化输出 USB 候选的 JSON 数组
eha-tool devices --json

# 列出 CANable2 SLCAN 串口候选，不打开串口或探测总线
eha-tool can-devices

# 为自动化输出 CANable2 候选的 JSON 数组
eha-tool can-devices --json
```

以下变量必须在执行前替换为已经核对的实际值；它们避免把示例值误当成设备目标。

```sh
# 设置目标 USB 的完整序列号
export EHA_USB_SERIAL='替换为实际完整 USB 序列号'

# 设置目标 CANable2 串口路径
export EHA_CAN_PORT='替换为实际 CANable2 串口路径'

# 设置实际 Customer CAN 节点号
export EHA_CAN_NODE='替换为实际节点号'

# 设置目标实际采用的 CAN 配置组
export EHA_CAN_PROFILE='替换为实际配置组'
```

USB 与 CAN 的单次调用都需要二选一的通路参数；CAN 必须同时给出 `--can-port`、`--node` 和
`--profile`。所有 `device` 动作可在动作前增加 `--timeout-secs SECONDS`，默认 5 秒；`device --json`
输出结构化动作结果，`inspect --json` 则只输出身份、运行实例和更新路由。
CAN 配置组接受 `classical_500k`、`classical_1m`、`fd_500k_2m`、`fd_1m_2m`、`fd_1m_5m`，
须与目标实际通信配置一致。

```sh
# 通过 USB 核对 Identity
eha-tool device --usb "$EHA_USB_SERIAL" inspect

# 通过 CAN 核对 Identity
eha-tool device --can-port "$EHA_CAN_PORT" --node "$EHA_CAN_NODE" --profile "$EHA_CAN_PROFILE" inspect

# 读取 USB 对象的当前状态
eha-tool device --usb "$EHA_USB_SERIAL" status

# 读取 USB 对象的详细测量快照
eha-tool device --usb "$EHA_USB_SERIAL" measurements

# 读取 USB 对象的详细诊断快照
eha-tool device --usb "$EHA_USB_SERIAL" diagnostics

# 在指定时段观察异步遥测；不会启动心跳或提交控制
eha-tool device --usb "$EHA_USB_SERIAL" telemetry 5

# 在指定时段显式启用并停止 SDK 心跳；不会发送 Stop
eha-tool device --usb "$EHA_USB_SERIAL" heartbeat-test 5
```

#### 控制与停止

`position MM`、`velocity MM_S`、`force N` 和 `impedance EQUILIBRIUM_MM STIFFNESS_N_PER_MM
DAMPING_NS_PER_MM` 都提交持续目标。单次 CLI 返回后不再持有心跳或会话，进程退出、超时、断连和停止
心跳都不表达停止意图；普通停止只能显式执行 `stop`。先按实际产品流程选择参数、持续意图和停止条件，
并读取本次目标的后续遥测判断采用、输出允许和设备影响。

```sh
# 将数值替换为已选定的实际位置目标，单位 mm
eha-tool device --usb "$EHA_USB_SERIAL" position 12.5

# 将数值替换为已选定的实际速度目标，单位 mm/s；零值仍是速度目标
eha-tool device --usb "$EHA_USB_SERIAL" velocity 1.0

# 将数值替换为已选定的实际力目标，单位 N
eha-tool device --usb "$EHA_USB_SERIAL" force 100.0

# 将数值替换为已选定的实际阻抗目标：平衡位置、刚度、阻尼
eha-tool device --usb "$EHA_USB_SERIAL" impedance 12.5 5.0 0.2

# 向同一通路显式发送普通停止意图
eha-tool device --usb "$EHA_USB_SERIAL" stop
```

`velocity-test MM_S SECONDS` 是单次 CLI 内建的短时速度编排：目标必须是非零有限值，时长为 1 至 10 秒，
并且绝对值不得超过从实际 Startup 配置读取的速度软幅值。它核对 Identity、Startup、初始 Idle 条件和
所选通路的联系，显式启停心跳，随后显式发送一次 Stop 并等待停止条件；不能用 `--json`。它成功只说明
取得了该流程要求的通信与停止结果，不能替代机构行程、负载或安装条件的核对。

```sh
# 将速度替换为已选定的低速目标，执行 2 秒受限速度试验
eha-tool device --usb "$EHA_USB_SERIAL" velocity-test 1.0 2
```

#### ODrive USB 只读

ODrive USB 是独立的只读入口：它不清错、重启、保存、标定、喂狗或提交控制，也不能证明 H723 与 ODrive
之间的 CAN 状态。`--python PATH` 可指定含 `odrive==0.5.1.post0` 的解释器；未给出时依次使用
`EHA_ODRIVE_PYTHON` 和 `python3`。`--timeout-secs` 的范围为 1 至 60，`--json` 在失败时也输出 JSON。

```sh
# 列出 ODrive USB 候选，不自动选择
eha-tool odrive devices

# 读取实际十六进制序列号的 ODrive USB 只读快照
eha-tool odrive status --serial 0x0123456789AB

# 使用指定 Python 并输出紧凑 JSON
eha-tool odrive --python /path/to/python --timeout-secs 10 --json status --serial 0x0123456789AB
```

#### 持久 Shell

在终端中运行无参数命令或 `shell` 会进入持久 Shell。Shell 在 `device connect` 或
`device connect-can` 后保留同一个 SDK 会话，因而可在其内启动心跳、持续观察缓存遥测和连续提交动作。
`disconnect`、`reconnect`、`quit` 和关闭 Shell 都只回收或恢复本地连接，不会发送 Stop、Reset 或重放
动作。交互式 Shell 可用 `help`、`help device` 和 `help config` 查看帮助。

```sh
# 进入交互式持久 Shell
eha-tool shell
```

在 Shell 内先用 `device list` 刷新 USB 候选；`device select` 可接受完整序列号，也可接受刚刚列出的
短数字序号。`device connect` 才会核对 Identity。CAN 不需要本地选择，直接由
`device connect-can PORT NODE PROFILE` 建立并核对。`device show`、`device clear`、`device snapshot`、
`device disconnect` 与 `device reconnect` 分别显示或管理本地选择与会话；`device list-can` 只刷新候选。
交互 Shell 只按 shell 语法分词，不展开环境变量；以下 `#` 行只说明下一行，输入时不要键入注释，并把带引号
的说明值及示例节点号替换为实际对象与参数。

```sh
# 在 Shell 中列出 USB 候选
device list

# 在 Shell 中选择已列出候选的完整 USB 序列号
device select '实际完整 USB 序列号'

# 在 Shell 中连接 USB 并核对 Identity
device connect

# 在 Shell 中通过 CAN 连接并核对 Identity
device connect-can '实际 CANable2 串口路径' 0 '实际配置组'

# 在 Shell 中读取当前会话快照
device snapshot
```

Shell 的 `status`、`measurements`、`diagnostics`、`position`、`velocity`、`force`、`impedance`、`stop`、
`heartbeat-start`、`heartbeat-stop`、`heartbeat-once`、`config-read`、`config-save`、`restore-factory`、
`result`、`release`、`reset-application` 和 `enter-update` 与持久会话关联。`telemetry` 只读取 SDK 已缓存
的最新遥测，不发查询；`wait SECONDS` 只在本地等待，既不发送查询、心跳也不控制。`config validate FILE`
在本地校验完整 JSON，但要求先连接；`config check FILE` 和 `config-check FILE` 不要求连接。`odrive` 与
`version` 也可在 Shell 内使用。

```sh
# 在 Shell 中启动心跳调度
heartbeat-start

# 将数值替换为已选定的持续速度目标
velocity 1.0

# 在 Shell 中读取已缓存的最新遥测
telemetry

# 在 Shell 中显式停止控制
stop

# 在 Shell 中停止心跳；这不是停止控制
heartbeat-stop
```

`shell --script FILE` 从文件逐行执行，`shell --batch` 从标准输入逐行执行；两者共用一个 Shell 会话，
遇到首个错误立即停止，不会重发。批处理输入中的空行和以 `#` 开头的整行会被忽略；交互 Shell 不把
`#` 当作注释。批处理中的 `help` 会以非零状态结束，因此应预先在普通 CLI 或交互 Shell 查看帮助。每个
批处理文件仍需自行先完成选择与连接。

```sh
# 执行文件中的持久 Shell 命令
eha-tool shell --script ./session.eha

# 通过标准输入按序执行持久 Shell 命令
eha-tool shell --batch <<EOF
# 刷新 USB 候选
device list
# 选择实际 USB 对象
device select "$EHA_USB_SERIAL"
# 连接并核对 Identity
device connect
# 读取状态
status
EOF
```

#### 配置与维护

`config-check` 完成 SDK 的静态配置检查；字段、schema 和换算规则见
[SDK 配置检查](https://github.com/EHAPower/eha-sdk#配置检查)。检查通过不替代设备身份、通信、
写后读回或实际运行条件的核对。

`config-read --view` 接受 `factory`、`user`、`startup` 或 `communication`；只有完整且为 UTF-8 的非
`communication` 视图可用 `--output` 新建文件，工具不覆盖已有文件。
以下为单次 CLI 示例；持久 Shell 用 `config-save FILE` 保存，用 `config-read --view VIEW` 读取，
不支持 `--output` 导出选项，其余维护命令及操作键参数相同。

```sh
# 读取用户配置并新建导出文件
eha-tool device --usb "$EHA_USB_SERIAL" config-read --view user --output ./before.json

# 保存完整候选配置并等待读回
eha-tool device --usb "$EHA_USB_SERIAL" save ./candidate.json

# 用保存返回的 72 位十六进制操作键读取原维护结果
eha-tool device --usb "$EHA_USB_SERIAL" result --operation-key '替换为保存返回的操作键'

# 释放已结清维护结果，并再次读取确认释放
eha-tool device --usb "$EHA_USB_SERIAL" release --operation-key '替换为已结清的操作键'

# 请求恢复出厂记录并读回实际用户记录
eha-tool device --usb "$EHA_USB_SERIAL" restore-factory

# 请求应用复位
eha-tool device --usb "$EHA_USB_SERIAL" reset-application

# 请求进入已交付的更新入口，不传输镜像
eha-tool device --usb "$EHA_USB_SERIAL" enter-update
```

`save` 的完成结果包含同一设备用户记录的读回；保存不自动复位、采用新启动配置或释放维护结果。
`restore-factory`、`reset-application` 与 `enter-update` 也会返回操作键。超时、断连或结果未知时，
保留原操作键并用 `result` 查询原结果，绝不自动重发原维护操作；`release` 仅释放已结清的结果。

`enter-update` 只请求应用进入更新入口，不传输镜像。镜像传输、DFU 对象核对和新应用启动核对由
外部更新流程负责。

### Web UI

Web UI 持有独立的 USB 和 CAN 持久 SDK 会话，只监听 `127.0.0.1`；浏览器打开、刷新、关闭页面、切换
当前通路或关闭本地连接都不会发送目标、Stop 或 Reset。默认入口为 `http://127.0.0.1:8080/`。跨主机访问时，
应以符合部署访问控制要求的端口转发访问同一回环端口。

```sh
# 在默认回环端口启动 Web UI
eha-tool webui

# 在指定回环端口启动 Web UI
eha-tool webui --port 8081

# 为 Web UI 的 ODrive USB 读取指定 Python
eha-tool webui --odrive-python /path/to/python

# 将运行记录写入指定目录
eha-tool webui --runs-dir /path/to/eha-runs
```

打开页面后，先在“总览”刷新 USB 或 CAN 候选。USB 从列表选择完整序列号后点击“连接 USB”；CAN 选择
适配器路径，填写实际节点号和实际采用的配置组后点击“连接 CAN”。连接会核对 Identity；“重连”再次核对，
“关闭”只关闭该通路的本地连接。页面顶部的“当前操作通路”决定后续状态读取、遥测、控制、配置、维护结果
与 Stop 的目的通路；USB 与 CAN 的会话、心跳、草稿和维护操作键分别保存。

在“诊断与维护”读取状态、测量或诊断。在“遥测与控制”查看位置、速度、力、接收年龄、输出条件、采用
阻塞项和控制来源；图表只显示当前会话收到的遥测，旧数据、缺失值和未知影响按页面标识，不能当成当前输出
或实际执行。持续控制时，先在当前通路启用“心跳调度”，再选择位置、速度、力或阻抗模式并填写已经选定的
实际参数后提交。提交仅表示本地发送边界；随后结合遥测观察目标采用与输出条件。停止心跳不会停止控制，
普通停止必须点击当前通路的“停止控制”。

在“配置”读取用户、出厂或启动记录，编辑完整 JSON 后先“校验 JSON”，再“保存并读回”；“恢复出厂配置”
也会产生维护结果。保存、恢复、复位或进入更新入口出现未知结果时，保留显示的操作键，在“维护结果”中查询
原结果，不重发；只对已结清结果点击“释放已结清结果”。需要应用维护时，在“诊断与维护”的“应用维护”区
点击“请求应用复位”或“请求进入更新入口”；断连不能证明新应用或更新入口已启动。
“诊断与维护”也可按需发现并读取 ODrive USB；它不
依赖 H723 会话，不自动轮询，也不能证明 H723 与 ODrive 的内部 CAN 正常。H723 所见 ODrive 摘要与直接
ODrive USB 快照是独立证据，分别查看其时间与错误状态。
ODrive USB 读取由独立工作者串行执行，已有读取未完成时再次请求返回忙碌；它不阻塞 H723 会话或快照读取。

#### 配置表单、原文与差异

“配置”页的 Schema 字段表单只根据当前工具取得的 Schema 帮助编辑和比较字段；完整 JSON 原文仍是唯一
完整记录。读取某个设备视图会更新当前草稿；“导入候选”只载入本地文件，“导出原文”只下载当前草稿，二者
都不访问或写入设备。表单和原文会彼此同步，页面会标记尚未保存的差异。保存前仍须校验完整 JSON，保存后
以设备读回的结果为准。

字段表单、差异、导入成功或浏览器下载都不说明配置已采用。只有“保存并读回”的维护结果才说明本次设备
操作已经取得的事实；它仍不自动复位、采用新的启动配置或释放维护结果。

#### 曲线、目标与反馈

“遥测与控制”中的曲线仅来自当前通路的被动遥测窗口。位置、速度和力曲线同时显示固件报告的对应目标；
压力 A/B 保留在遥测和运行记录中。时间缺口和缺失值会保留标识，不能把折线连续、图表刷新或最后一次本地提交转速当作目标已采用、
ODrive 已执行或机械机构已运动的证明。暂停绘图只影响浏览器展示，不会停止控制、心跳或后端记录。

#### 试验与普通持续目标

普通“遥测与控制”页的位置、速度、力和阻抗命令仍是持续目标：它们不附带 PC 侧试验包络或自动停止；关闭
页面、断开连接、停止心跳也不会发送 Stop。操作者须按产品流程保持联系、观察遥测，并在需要时显式点击
“停止控制”。

“试验”页是单独的受限流程。操作者先确认机械空间、外部断能和本次范围，填写仅约束本次主机请求的包络，
再由工具读取实际 Startup 配置与 Status 作只读预检。预检通过才提交一次试验目标；包络不会写入设备配置。
每次试验均受本次最大时长限制，也可设置更短的运行时间；位置试验还可设置到位容差和稳定时间。位置滑块以至多 20 Hz 合并最新的试验位置请求，不能绕过
包络或把连续拖动解释为连续成功的设备执行。

试验停止始终显式发送一次 Stop，并只以 Stop 后的新遥测确认目标清除和停止状态。时长结束、位置到位、
显式停止、提交失败或结果未知均须查看试验状态和后续遥测；工具不会重放目标或 Stop。试验运行期间，普通
控制与阻塞查询不应与其并发；先读取快照，或通过试验入口更新位置、停止试验。

#### 共享 CAN 与多节点试验

一个 CAN 适配器可以承载多个已明确选择的 Customer CAN 节点。WebUI 在同一物理串口上为每个节点保存各自的
身份、会话、心跳、遥测和结果，不能以某一节点的连接、反馈或停止结果替代另一节点的事实。
在“总览”明确指定 0 至 127 范围内的扫描区间，再选择发现的节点连接。扫描只查询身份，不开启心跳；
已有会话正在心跳或保留持续目标时，应先显式停止并核对，再关闭心跳后扫描。共享串口故障会结束所有节点的
该次连接，不自动重开串口或重放动作。
串口重新可用后，可显式点击所选节点的“重连”；工具保留传输保护状态，只重新连接并核对该节点身份，
其他节点仍保持断开，心跳与目标均需另行明确操作。

“试验”页可选择已连接的 CAN 群组。开始前每个成员都必须独立完成只读预检；预检不全通过时不提交群组需求。
提交或停止后的每个节点结果仍分别显示。部分提交、部分停止、超时或未知结果时，不自动重试、补发或把群组
结果概括为成功；停止进一步运动并按各节点实际结果处置。
群组开始后，“停止本次试验”使用服务保留的原成员范围，不随当前节点选择或勾选变化缩小。

#### 运行记录

`--runs-dir` 选择记录根目录。省略时使用系统用户数据目录下的 `eha-tool/runs`；设置了
`XDG_DATA_HOME` 时优先使用该目录，macOS 默认使用 `~/Library/Application Support/eha-tool/runs`，
Windows 使用 `%LOCALAPPDATA%/eha-tool/runs`，其他系统使用 `~/.local/share/eha-tool/runs`。

“运行记录”由本机 WebUI 服务写入。开始记录要求当前通路已有已核对 Identity 的活跃会话，并只读取得 Startup
配置；应在试验开始前启动记录。开始和停止记录不会发送控制、心跳、Stop、Reset 或维护动作。每次运行使用独立目录，包含：

- `metadata.json`：工具版本、连接、设备身份、UID、运行 nonce、本次启动配置原文及来源；
- `events.jsonl`：命令、试验、记录及错误的顺序事件；
- `telemetry.csv`：带时间、序号、目标、反馈、`gap` 与完整遥测 JSON 的样本。

`gap` 或 dropped 计数说明记录期间存在遥测丢失，不能补为连续测量。关闭或刷新浏览器、暂停图表都不会删除已经
写入的样本；停止记录只完成并刷新文件，不改变当前连接或控制状态。记录文件保留通信与工具事件证据，不能单独
证明设备采用、执行、机械位移或安全条件。
关闭本地连接、传输断开或设备/运行实例改变时结束该次记录，不混入新身份的数据，也不因此发送 Stop。
写入失败会显示错误并保留部分文件，不能将这些文件当作完整运行记录。

## 结果边界

工具不模拟设备，也不隐式停止、复位或重放目标。关闭进程只回收本地资源。配置导出仅在固件报告
完整记录时写入文件，且不会覆盖已有文件；本地写入失败可能留下不完整文件。设备适用性、安装条件、
保护结果和机构效果须由相应产品流程核对。
