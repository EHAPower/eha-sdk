// Copyright The eha-sdk Contributors

//! WebUI 的会话所有权、共享 CAN 网络和本地记录目录边界。
//!
//! 这里不保存浏览器“当前节点”。每个调用者都必须随请求给出 transport，CAN 请求还须
//! 给出 node（旧单节点入口可以省略 node，使用它连接时明确保存的默认节点）。

use crate::session::{
    Command, CommandResult, ConnectionRequest, SessionError, ToolSession, TrialRequest,
};
use eha_sdk::can::{
    CanChannelFactory, CanNetwork, CanNetworkStatus, CanNodeConnector, python::PythonCanOptions,
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    path::PathBuf,
    sync::Arc,
};

const TREND_CAPACITY: usize = 4_096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Transport {
    Usb,
    Can,
}

impl Transport {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Usb => "usb",
            Self::Can => "can",
        }
    }
}

pub(crate) struct CanNetworkState {
    network: CanNetwork,
    channel: String,
    mode: String,
    python: Option<String>,
}

/// 进程唯一的 WebUI 会话 owner。CAN 节点独立保存 SDK Client/身份/趋势/记录/试验，
/// 但由一个 `CanNetwork` 持有外部 CAN 通道。
pub(crate) struct Workbench {
    usb: ToolSession,
    can_nodes: BTreeMap<u8, ToolSession>,
    can_network: Option<CanNetworkState>,
    default_can_node: Option<u8>,
    /// 成功进入 group start 的原始范围。即使单项开始返回 unknown 也保留，直到每台
    /// 节点的 Stop 被新遥测确认，避免浏览器下一次请求缩窄 Stop 覆盖范围。
    group_nodes: BTreeSet<u8>,
    runs_dir: PathBuf,
}

impl Workbench {
    pub(crate) fn new(runs_dir: PathBuf) -> Self {
        Self {
            usb: new_session(),
            can_nodes: BTreeMap::new(),
            can_network: None,
            default_can_node: None,
            group_nodes: BTreeSet::new(),
            runs_dir,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_test_sessions(
        usb: ToolSession,
        can_nodes: BTreeMap<u8, ToolSession>,
    ) -> Self {
        let default_can_node = can_nodes.keys().next().copied();
        Self {
            usb,
            can_nodes,
            can_network: None,
            default_can_node,
            group_nodes: BTreeSet::new(),
            runs_dir: std::env::temp_dir().join("eha-tool-webui-tests"),
        }
    }

    pub(crate) fn default_runs_dir() -> PathBuf {
        if let Some(path) = env::var_os("XDG_DATA_HOME") {
            return PathBuf::from(path).join("eha-tool").join("runs");
        }
        #[cfg(target_os = "macos")]
        if let Some(home) = env::var_os("HOME") {
            return PathBuf::from(home)
                .join("Library")
                .join("Application Support")
                .join("eha-tool")
                .join("runs");
        }
        #[cfg(windows)]
        if let Some(path) = env::var_os("LOCALAPPDATA") {
            return PathBuf::from(path).join("eha-tool").join("runs");
        }
        env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".local")
            .join("share")
            .join("eha-tool")
            .join("runs")
    }

    pub(crate) fn tick(&mut self) {
        self.usb.tick();
        for session in self.can_nodes.values_mut() {
            session.tick();
        }
        if !self.group_nodes.is_empty()
            && self
                .group_nodes
                .iter()
                .all(|node| self.can_nodes.get(node).is_some_and(group_member_terminal))
        {
            self.group_nodes.clear();
        }
    }

    pub(crate) fn has_active_trial(&self) -> bool {
        std::iter::once(&self.usb)
            .chain(self.can_nodes.values())
            .any(|session| {
                matches!(
                    session.trial_snapshot()["state"].as_str(),
                    Some("active" | "stopping")
                )
            })
    }

    /// 扫描会建立短生命周期的 Identity 查询。只要任一已持有会话仍在心跳或最后遥测
    /// 显示非 Idle 持续目标，就不能让这类查询与控制共享同一设备入口竞争。
    pub(crate) fn can_scan_guard(&mut self) -> Result<(), String> {
        let mut sessions = std::iter::once(&mut self.usb).chain(self.can_nodes.values_mut());
        if sessions.any(session_scan_blocker) {
            return Err(
                "已有会话正在心跳或保留非 Idle 持续目标；先显式 Stop 并核对无目标/Idle，再停用心跳后扫描 CAN。".into(),
            );
        }
        Ok(())
    }

    pub(crate) fn network_connector(&mut self, node: u8) -> Result<CanNodeConnector, String> {
        self.can_network
            .as_ref()
            .ok_or_else(|| "请先建立 CAN 网络".to_owned())?
            .network
            .node(node)
    }

    pub(crate) fn ensure_network(
        &mut self,
        channel: String,
        mode: String,
        python: Option<String>,
    ) -> Result<(), String> {
        let parsed =
            crate::session::parse_can_mode(&mode).map_err(|error| error.message.to_string())?;
        if let Some(network) = self.can_network.as_ref() {
            if network.channel == channel && network.mode == mode && network.python == python {
                return Ok(());
            }
            return Err(
                "CAN 网络已绑定到另一外部通道或帧格式；请在没有已持有 CAN 会话时重新启动 WebUI。"
                    .into(),
            );
        }
        let network = CanNetwork::new(python_factory(channel.clone(), python.clone()), parsed)?;
        self.can_network = Some(CanNetworkState {
            network,
            channel,
            mode,
            python,
        });
        Ok(())
    }

    pub(crate) fn connect(&mut self, request: ConnectionRequest) -> Result<Value, SessionError> {
        match request {
            ConnectionRequest::Usb { .. } => {
                self.usb.connect(request)?;
                Ok(json!({"connected": true}))
            }
            ConnectionRequest::Can {
                channel,
                node,
                mode,
                python,
            } => {
                let result = self
                    .connect_can(channel, mode, python, &[node], true)
                    .map_err(session_error)?;
                if result["ok"] == true {
                    Ok(result)
                } else {
                    Err(session_error(format!(
                        "CAN 节点连接未完成：{}",
                        result["errors"]
                    )))
                }
            }
        }
    }

    /// 返回每个节点的真实结果；失败节点绝不触发重试或重新编号。
    pub(crate) fn connect_can(
        &mut self,
        channel: String,
        mode: String,
        python: Option<String>,
        nodes: &[u8],
        set_default: bool,
    ) -> Result<Value, String> {
        validate_nodes(nodes)?;
        self.ensure_network(channel.clone(), mode.clone(), python.clone())?;
        let mut results = Vec::with_capacity(nodes.len());
        for &node in nodes {
            let request = ConnectionRequest::Can {
                channel: channel.clone(),
                node,
                mode: mode.clone(),
                python: python.clone(),
            };
            let result = if let Some(session) = self.can_nodes.get_mut(&node) {
                let snapshot = session.snapshot();
                if snapshot.connected && snapshot.transport["disconnected"].is_null() {
                    Ok(snapshot)
                } else {
                    session.reconnect()
                }
            } else {
                let connector = self.network_connector(node)?;
                let mut session = new_session();
                let result = session.connect_shared_can(request, connector);
                self.can_nodes.insert(node, session);
                result
            };
            match result {
                Ok(snapshot) => results.push(json!({"node":node,"ok":true,"identity":snapshot.identity,"snapshot":snapshot})),
                Err(error) => results.push(json!({"node":node,"ok":false,"message":error.message,"unknown":error.unknown,"error":error})),
            }
        }
        if set_default {
            self.default_can_node = nodes.first().copied();
        }
        let ok = results.iter().all(|value| value["ok"] == true);
        let errors = results
            .iter()
            .filter(|value| value["ok"] != true)
            .cloned()
            .collect::<Vec<_>>();
        let nodes = results.clone();
        Ok(json!({"ok":ok,"results":results,"nodes":nodes,"errors":errors}))
    }

    pub(crate) fn disconnect(
        &mut self,
        transport: Transport,
        node: Option<u8>,
    ) -> Result<(), SessionError> {
        self.session_mut(transport, node)?.disconnect()?;
        Ok(())
    }

    pub(crate) fn reconnect(
        &mut self,
        transport: Transport,
        node: Option<u8>,
    ) -> Result<(), SessionError> {
        if transport == Transport::Can {
            let selected = self.selected_node(node).map_err(session_error)?;
            if !self.can_nodes.contains_key(&selected) {
                return Err(session_error(format!("CAN 节点 {selected} 尚未连接")));
            }
            let status = self
                .can_network
                .as_ref()
                .map(|network| network.network.status());
            match status {
                Some(CanNetworkStatus::Terminated { .. }) => self
                    .can_network
                    .as_ref()
                    .expect("CAN status has an owner")
                    .network
                    .recover()
                    .map_err(session_error)?,
                Some(CanNetworkStatus::Quiescing { reason }) => {
                    return Err(session_error(format!(
                        "共享 CAN 网络正在隔离旧传输状态；请等待终止后再显式恢复：{reason}"
                    )));
                }
                Some(CanNetworkStatus::Active) | None => {}
            }
        }
        self.session_mut(transport, node)?.reconnect()?;
        Ok(())
    }

    pub(crate) fn refresh_identity(
        &mut self,
        transport: Transport,
        node: Option<u8>,
    ) -> Result<(), SessionError> {
        self.session_mut(transport, node)?.refresh_identity()?;
        Ok(())
    }

    pub(crate) fn execute(
        &mut self,
        transport: Transport,
        node: Option<u8>,
        command: Command,
    ) -> Result<CommandResult, SessionError> {
        if self.has_active_trial() {
            if matches!(command, Command::Stop) {
                return self.stop_control(transport, node);
            }
            return Err(session_error(
                "有进行中的试验或 Stop 确认；只允许被动快照、记录、显式 Stop 或合并位置目标",
            ));
        }
        self.session_mut(transport, node)?.execute(command)
    }

    /// 旧 `/api/action {action:"stop"}` 的兼容入口。若请求指向的同一运行实例正在另一
    /// 通路进行试验，Stop 必须送回该试验 owner，避免普通查询或第二次 Stop 覆盖确认状态。
    fn stop_control(
        &mut self,
        transport: Transport,
        node: Option<u8>,
    ) -> Result<CommandResult, SessionError> {
        let target = self.identity_for(transport, node)?;
        if self.usb_trial_matches(&target) {
            let value = self.usb.stop_trial()?;
            return Ok(CommandResult {
                action: "stop".into(),
                local_submission: None,
                operation_key: None,
                data: value,
                snapshot: self.usb.snapshot(),
            });
        }
        let owner = self.can_nodes.iter_mut().find_map(|(&number, session)| {
            (trial_active(session) && identity_of(session) == target).then_some(number)
        });
        if let Some(owner) = owner {
            let session = self.can_nodes.get_mut(&owner).expect("owner exists");
            let value = session.stop_trial()?;
            return Ok(CommandResult {
                action: "stop".into(),
                local_submission: None,
                operation_key: None,
                data: value,
                snapshot: session.snapshot(),
            });
        }
        self.session_mut(transport, node)?.execute(Command::Stop)
    }

    pub(crate) fn recording(
        &mut self,
        transport: Transport,
        node: Option<u8>,
        start: bool,
    ) -> Result<Value, SessionError> {
        if start && self.has_active_trial() {
            return Err(session_error(
                "有进行中的试验或 Stop 确认；不能从另一通路启动记录并读取 Startup。",
            ));
        }
        let root = self.runs_dir.clone();
        let session = self.session_mut(transport, node)?;
        if start {
            session.start_recording(&root)
        } else {
            session.stop_recording()
        }
    }

    pub(crate) fn trial(
        &mut self,
        transport: Transport,
        node: Option<u8>,
        action: TrialAction,
    ) -> Result<Value, SessionError> {
        match action {
            TrialAction::Start(request) => {
                if self.has_active_trial() {
                    return Err(session_error(
                        "已有进行中的试验或 Stop 确认；不能启动第二个试验",
                    ));
                }
                self.session_mut(transport, node)?.start_trial(request)
            }
            TrialAction::Stop => self.stop_trial_for(transport, node),
            TrialAction::Target(mm) => self.target_trial_for(transport, node, mm),
        }
    }

    fn stop_trial_for(
        &mut self,
        transport: Transport,
        node: Option<u8>,
    ) -> Result<Value, SessionError> {
        let target = self.identity_for(transport, node)?;
        if self.usb_trial_matches(&target) {
            return self.usb.stop_trial();
        }
        let owner = self.can_nodes.iter_mut().find_map(|(&number, session)| {
            (trial_active(session) && identity_of(session) == target).then_some(number)
        });
        if let Some(owner) = owner {
            return self
                .can_nodes
                .get_mut(&owner)
                .expect("owner exists")
                .stop_trial();
        }
        self.session_mut(transport, node)?.stop_trial()
    }

    fn target_trial_for(
        &mut self,
        transport: Transport,
        node: Option<u8>,
        mm: f32,
    ) -> Result<Value, SessionError> {
        let target = self.identity_for(transport, node)?;
        if self.usb_trial_matches(&target) {
            return self.usb.update_trial_position(mm);
        }
        let owner = self.can_nodes.iter_mut().find_map(|(&number, session)| {
            (trial_active(session) && identity_of(session) == target).then_some(number)
        });
        if let Some(owner) = owner {
            return self
                .can_nodes
                .get_mut(&owner)
                .expect("owner exists")
                .update_trial_position(mm);
        }
        self.session_mut(transport, node)?.update_trial_position(mm)
    }

    /// 全体先 prepare，任何一台失败时不向任何节点提交目标；启动后的单项失败如实保留，
    /// 不自动 Stop、回滚或重发。
    pub(crate) fn group_trial_start(
        &mut self,
        nodes: &[u8],
        request: TrialRequest,
    ) -> Result<Value, String> {
        validate_nodes(nodes)?;
        if self.has_active_trial() {
            return Err("上一组试验仍在运行或等待 Stop 确认；不能启动新试验".into());
        }
        let mut identities = BTreeSet::new();
        for &node in nodes {
            let identity = self
                .identity_for(Transport::Can, Some(node))
                .map_err(|error| error.message.to_string())?;
            let uid = identity
                .get("uid")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("CAN 节点 {node} 尚未核对 Identity"))?;
            if !identities.insert(uid.to_owned()) {
                return Err("组试验节点指向同一 UID，已拒绝重复提交".into());
            }
        }
        let mut prepared = Vec::with_capacity(nodes.len());
        let mut all_prepared = true;
        for &node in nodes {
            match self
                .session_mut(Transport::Can, Some(node))
                .map_err(|error| error.message.to_string())?
                .prepare_trial(&request)
            {
                Ok(()) => prepared.push(json!({"node":node,"ok":true,"stage":"prepared"})),
                Err(error) => {
                    all_prepared = false;
                    prepared.push(error_result(node, "prepare", error));
                }
            }
        }
        if !all_prepared {
            let unknown = results_unknown(&prepared);
            return Ok(json!({
                "ok":false,
                "unknown":unknown,
                "stage":"prepare",
                "results":prepared,
                "started":false,
                "message":"群组预检失败；未向任何节点提交目标。"
            }));
        }
        // 新一轮明确 group start 取代已经终止但未确认的旧范围，不能把旧未知成员
        // 悄悄并入新组的 Stop。旧范围在新的 group start 被用户明确选择前仍保留。
        self.group_nodes = nodes.iter().copied().collect();
        let mut results = Vec::with_capacity(nodes.len());
        for &node in nodes {
            match self
                .session_mut(Transport::Can, Some(node))
                .map_err(|error| error.message.to_string())?
                .start_prepared_trial(request.clone())
            {
                Ok(value) => {
                    results.push(json!({"node":node,"ok":true,"stage":"submitted","result":value}))
                }
                Err(error) => results.push(error_result(node, "start", error)),
            }
            self.tick();
        }
        let ok = results.iter().all(|value| value["ok"] == true);
        let unknown = results_unknown(&results);
        let message = if ok {
            "全部节点已提交试验目标；请等待各节点被动状态确认。"
        } else {
            "部分节点提交未确认；系统不会自动重发或回滚，请查看逐节点结果并按需显式 Stop。"
        };
        Ok(
            json!({"ok":ok,"unknown":unknown,"stage":"start","prepared":prepared,"results":results,"message":message}),
        )
    }

    pub(crate) fn group_stop(&mut self, nodes: &[u8]) -> Result<Value, String> {
        if nodes.is_empty() && self.group_nodes.is_empty() {
            return Err("没有可停止的群组节点".into());
        }
        if !nodes.is_empty() {
            validate_nodes(nodes)?;
        }
        let mut targets = self.group_nodes.clone();
        targets.extend(nodes.iter().copied());
        let mut results = Vec::with_capacity(targets.len());
        for node in targets {
            let session = self
                .session_mut(Transport::Can, Some(node))
                .map_err(|error| error.message.to_string())?;
            let state = session.trial_snapshot()["state"]
                .as_str()
                .unwrap_or("idle")
                .to_owned();
            let result = match state.as_str() {
                "active" | "stopping" => session.stop_trial(),
                // Transport interruption has no active Trial state to stop, but the user has
                // explicitly asked to cover the original group range once. Do not retry this
                // submission automatically.
                "interrupted_unknown" => session
                    .execute(Command::Stop)
                    .and_then(|result| serde_json::to_value(result).map_err(json_error)),
                "idle" | "completed" => Ok(json!({"state":state,"skipped":true})),
                _ => session.stop_trial(),
            };
            match result {
                Ok(value) => results.push(json!({"node":node,"ok":true,"result":value})),
                Err(error) => results.push(error_result(node, "stop", error)),
            }
            self.tick();
        }
        let ok = results.iter().all(|value| value["ok"] == true);
        let unknown = results_unknown(&results);
        Ok(json!({
            "ok":ok,
            "unknown":unknown,
            "nodes":self.group_nodes.iter().copied().collect::<Vec<_>>(),
            "results":results,
            "message":if ok {
                "已向群组原始范围逐节点提交 Stop 或报告终态。"
            } else {
                "部分节点 Stop 结果未知或失败；系统不会自动重发，请查看逐节点结果。"
            }
        }))
    }

    pub(crate) fn group_heartbeat(&mut self, nodes: &[u8], start: bool) -> Result<Value, String> {
        validate_nodes(nodes)?;
        let command = if start {
            Command::HeartbeatStart
        } else {
            Command::HeartbeatStop
        };
        let results = nodes
            .iter()
            .copied()
            .map(|node| {
                match self
                    .session_mut(Transport::Can, Some(node))?
                    .execute(command.clone())
                {
                    Ok(value) => Ok(json!({"node":node,"ok":true,"result":value})),
                    Err(error) => Ok(error_result(node, "heartbeat", error)),
                }
            })
            .collect::<Result<Vec<_>, SessionError>>()
            .map_err(|error| error.message.to_string())?;
        let ok = results.iter().all(|value| value["ok"] == true);
        Ok(json!({"ok":ok,"results":results}))
    }

    pub(crate) fn snapshot_json(
        &mut self,
        selected: Transport,
        node: Option<u8>,
        trend_after: Option<Option<u64>>,
        odrive: Option<&Value>,
    ) -> Result<Value, String> {
        // 错误响应也必须能投影其余会话。未建立旧单节点默认值时，CAN 选中快照只是
        // 未连接，而不能因构造错误响应再次失败。
        let selected_node = (selected == Transport::Can)
            .then(|| self.selected_node(node).ok())
            .flatten();
        let usb = snapshot_json(&mut self.usb)?;
        let mut can_nodes = Vec::with_capacity(self.can_nodes.len());
        for (&number, session) in &mut self.can_nodes {
            can_nodes.push(json!({"node":number,"snapshot":snapshot_json(session)?,"trial":session.trial_snapshot(),"recording":session.recording_snapshot()}));
        }
        let selected_snapshot = match selected {
            Transport::Usb => usb.clone(),
            Transport::Can => can_nodes
                .iter()
                .find(|entry| entry["node"] == selected_node.unwrap_or(u8::MAX))
                .map(|entry| entry["snapshot"].clone())
                .unwrap_or_else(disconnected_snapshot),
        };
        let can = self
            .default_can_node
            .and_then(|default| {
                can_nodes
                    .iter()
                    .find(|entry| entry["node"] == default)
                    .map(|entry| entry["snapshot"].clone())
            })
            .unwrap_or_else(disconnected_snapshot);
        let mut snapshot = selected_snapshot;
        // 趋势只放在当前快照一次；节点摘要不重复携带整个 30 秒窗口。
        if let Some(after) = trend_after {
            snapshot["trend"] = match selected {
                Transport::Usb => self.usb.telemetry_trend_json(after),
                Transport::Can => selected_node
                    .and_then(|node| self.can_nodes.get(&node))
                    .map_or(Value::Null, |session| session.telemetry_trend_json(after)),
            };
        }
        snapshot["trial"] = match selected {
            Transport::Usb => self.usb.trial_snapshot(),
            Transport::Can => can_nodes
                .iter()
                .find(|entry| entry["node"] == selected_node.unwrap_or(u8::MAX))
                .map(|entry| entry["trial"].clone())
                .unwrap_or_else(|| json!({"state":"idle"})),
        };
        snapshot["recording"] = match selected {
            Transport::Usb => self.usb.recording_snapshot(),
            Transport::Can => can_nodes
                .iter()
                .find(|entry| entry["node"] == selected_node.unwrap_or(u8::MAX))
                .map(|entry| entry["recording"].clone())
                .unwrap_or_else(|| json!({"active":false})),
        };
        snapshot["odrive_usb"] = odrive.cloned().unwrap_or(Value::Null);
        Ok(json!({
            "snapshot":snapshot,
            "sessions":{"usb":usb,"can":can},
            "transport":selected.as_str(),
            "can_nodes":can_nodes,
            "group_nodes":self.group_nodes.iter().copied().collect::<Vec<_>>(),
        }))
    }

    pub(crate) fn list_runs(&self) -> Result<Value, String> {
        let mut runs = Vec::new();
        let entries = match fs::read_dir(&self.runs_dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(json!({"runs":runs}));
            }
            Err(error) => return Err(error.to_string()),
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(id) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if !valid_run_id(&id) || !path.is_dir() {
                continue;
            }
            let metadata = fs::read_to_string(path.join("metadata.json"))
                .ok()
                .and_then(|text| serde_json::from_str::<Value>(&text).ok())
                .unwrap_or(Value::Null);
            let files = ["metadata.json", "events.jsonl", "telemetry.csv"]
                .into_iter()
                .filter(|name| path.join(name).is_file())
                .collect::<Vec<_>>();
            runs.push(json!({"id":id,"metadata":metadata,"files":files}));
        }
        runs.sort_by(|left, right| right["id"].as_str().cmp(&left["id"].as_str()));
        Ok(json!({"runs":runs}))
    }

    pub(crate) fn recording_file(&self, id: &str, file: &str) -> Result<PathBuf, String> {
        if !valid_run_id(id) || !matches!(file, "metadata.json" | "events.jsonl" | "telemetry.csv")
        {
            return Err("记录下载路径无效".into());
        }
        let root = self
            .runs_dir
            .canonicalize()
            .map_err(|_| "记录目录不存在".to_owned())?;
        let path = root.join(id).join(file);
        let resolved = path
            .canonicalize()
            .map_err(|_| "记录文件不存在".to_owned())?;
        if !resolved.starts_with(&root) || !resolved.is_file() {
            return Err("记录下载路径无效".into());
        }
        Ok(resolved)
    }

    fn selected_node(&self, requested: Option<u8>) -> Result<u8, String> {
        requested
            .or(self.default_can_node)
            .ok_or_else(|| "CAN 请求必须明确指定 node，或先通过旧单节点连接建立默认节点".into())
    }

    fn identity_for(
        &mut self,
        transport: Transport,
        node: Option<u8>,
    ) -> Result<Value, SessionError> {
        let snapshot = self.session_mut(transport, node)?.snapshot();
        let value =
            serde_json::to_value(snapshot).map_err(|error| session_error(error.to_string()))?;
        Ok(value["identity"].clone())
    }

    fn usb_trial_matches(&mut self, identity: &Value) -> bool {
        trial_active(&self.usb) && identity_of(&mut self.usb) == *identity
    }

    fn session_mut(
        &mut self,
        transport: Transport,
        node: Option<u8>,
    ) -> Result<&mut ToolSession, SessionError> {
        match transport {
            Transport::Usb => Ok(&mut self.usb),
            Transport::Can => {
                let node = self.selected_node(node).map_err(session_error)?;
                self.can_nodes
                    .get_mut(&node)
                    .ok_or_else(|| session_error(format!("CAN 节点 {node} 尚未连接")))
            }
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) enum TrialAction {
    Start(TrialRequest),
    Stop,
    Target(f32),
}

fn new_session() -> ToolSession {
    ToolSession::new().with_telemetry_trend(TREND_CAPACITY)
}

fn snapshot_json(session: &mut ToolSession) -> Result<Value, String> {
    let mut value = serde_json::to_value(session.snapshot()).map_err(|error| error.to_string())?;
    value["trial"] = session.trial_snapshot();
    value["recording"] = session.recording_snapshot();
    Ok(value)
}

fn disconnected_snapshot() -> Value {
    json!({"connected":false,"transport":{"disconnected":null}})
}

fn identity_of(session: &mut ToolSession) -> Value {
    serde_json::to_value(session.snapshot())
        .ok()
        .and_then(|value| value.get("identity").cloned())
        .unwrap_or(Value::Null)
}

fn trial_active(session: &ToolSession) -> bool {
    matches!(
        session.trial_snapshot()["state"].as_str(),
        Some("active" | "stopping")
    )
}

fn session_scan_blocker(session: &mut ToolSession) -> bool {
    let snapshot = session.snapshot();
    scan_snapshot_blocker(&snapshot.heartbeat, snapshot.telemetry.as_ref())
}

fn scan_snapshot_blocker(heartbeat: &Value, telemetry: Option<&Value>) -> bool {
    heartbeat["enabled"] == true
        || telemetry
            .and_then(|telemetry| telemetry.get("target_mode"))
            .and_then(Value::as_u64)
            .is_some_and(|mode| mode != 0)
}

/// 只有成员已报告终态才释放群组范围。`stopping` 和任何未知/中断投影都保留范围，
/// 让用户仍可覆盖原始成员发出显式 Stop；未来 TrialSession 若加入 `completed`，也
/// 不会让已结束组永久锁住重连或下一轮试验。
fn group_member_terminal(session: &ToolSession) -> bool {
    matches!(
        session.trial_snapshot()["state"].as_str(),
        Some("idle" | "completed")
    )
}

fn python_factory(channel: String, python: Option<String>) -> Arc<dyn CanChannelFactory> {
    Arc::new(match python {
        Some(python) => PythonCanOptions::with_python(channel, python),
        None => PythonCanOptions::new(channel),
    })
}

fn validate_nodes(nodes: &[u8]) -> Result<(), String> {
    if nodes.is_empty() {
        return Err("至少选择一个 CAN 节点".into());
    }
    let mut seen = BTreeMap::new();
    for &node in nodes {
        if node > 127 {
            return Err("CAN 节点必须在 0..=127".into());
        }
        if seen.insert(node, ()).is_some() {
            return Err("CAN 节点不得重复".into());
        }
    }
    Ok(())
}

fn error_result(node: u8, stage: &str, error: SessionError) -> Value {
    json!({"node":node,"ok":false,"stage":stage,"message":error.message,"unknown":error.unknown,"error":error})
}

fn results_unknown(results: &[Value]) -> bool {
    results.iter().any(|result| {
        result["unknown"] == true
            || result["error"]["unknown"] == true
            || result["result"]["unknown"] == true
    })
}

fn session_error(message: impl Into<String>) -> SessionError {
    SessionError {
        kind: "invalid_input".into(),
        message: message.into().into_boxed_str(),
        local_stage: Some("not_submitted".into()),
        operation_key: None,
        unknown: false,
        failure: None,
        last_result: None,
        observed_transport: None,
        transport_raw_hex: None,
    }
}

fn json_error(error: serde_json::Error) -> SessionError {
    session_error(error.to_string())
}

fn valid_run_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id.starts_with("run-")
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

#[cfg(test)]
mod tests {
    use super::{Transport, TrialAction, Workbench, scan_snapshot_blocker, valid_run_id};
    use crate::{
        session::trial_tests::{FixtureBehavior, FixtureProbe, session_fixture},
        session::{Command, TrialEnvelope, TrialRequest},
    };
    use serde_json::json;
    use std::{
        collections::BTreeMap,
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    #[test]
    fn snapshot_sends_trend_only_for_selected_view() {
        let (usb, _) = session_fixture(0, [42; 12], FixtureBehavior::Ready);
        let (can, _) = session_fixture(1, [1; 12], FixtureBehavior::Ready);
        let mut workbench = Workbench::with_test_sessions(
            usb.with_telemetry_trend(4096),
            BTreeMap::from([(1, can.with_telemetry_trend(4096))]),
        );
        for transport in [Transport::Usb, Transport::Can] {
            let value = workbench
                .snapshot_json(transport, Some(1), Some(None), None)
                .expect("snapshot");
            assert!(value["snapshot"]["trend"].is_object());
            assert!(value["sessions"]["usb"].get("trend").is_none());
            assert!(value["sessions"]["can"].get("trend").is_none());
            for node in value["can_nodes"].as_array().expect("CAN nodes") {
                assert!(node["snapshot"].get("trend").is_none());
            }
        }
    }

    fn request() -> TrialRequest {
        TrialRequest {
            command: Command::Position { mm: 1.0 },
            envelope: TrialEnvelope {
                position_min_mm: -10.0,
                position_max_mm: 10.0,
                velocity_abs_max_mm_s: 2.0,
                force_abs_max_n: 10.0,
                stiffness_max_n_per_mm: 10.0,
                damping_max_ns_per_mm: 10.0,
                duration_max_s: 5.0,
            },
            duration_s: Some(5.0),
            reach: None,
        }
    }

    fn group_workbench(
        first: FixtureBehavior,
        second: FixtureBehavior,
    ) -> (Workbench, FixtureProbe, FixtureProbe) {
        let (usb, _) = session_fixture(0, [42; 12], FixtureBehavior::Ready);
        let (one, one_probe) = session_fixture(1, [1; 12], first);
        let (two, two_probe) = session_fixture(2, [2; 12], second);
        let workbench = Workbench::with_test_sessions(usb, BTreeMap::from([(1, one), (2, two)]));
        (workbench, one_probe, two_probe)
    }

    #[test]
    fn recording_download_only_accepts_a_run_id_and_fixed_files() {
        let root = std::env::temp_dir().join(format!(
            "eha-tool-webui-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        fs::create_dir_all(root.join("run-safe")).expect("run directory");
        fs::write(root.join("run-safe/metadata.json"), "{}").expect("metadata");
        let workbench = Workbench::new(root.clone());
        assert!(
            workbench
                .recording_file("run-safe", "metadata.json")
                .is_ok()
        );
        assert!(
            workbench
                .recording_file("../run-safe", "metadata.json")
                .is_err()
        );
        assert!(workbench.recording_file("run-safe", "other.txt").is_err());
        assert!(valid_run_id("run-1_2"));
        assert!(!valid_run_id("run-../x"));
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn scan_rejects_heartbeat_or_known_non_idle_target() {
        assert!(scan_snapshot_blocker(&json!({"enabled":true}), None));
        assert!(scan_snapshot_blocker(
            &json!({"enabled":false}),
            Some(&json!({"target_mode":1}))
        ));
        assert!(!scan_snapshot_blocker(
            &json!({"enabled":false}),
            Some(&json!({"target_mode":0}))
        ));
    }

    #[test]
    fn group_preflight_failure_sends_no_targets() {
        let (mut workbench, ready, rejected) =
            group_workbench(FixtureBehavior::Ready, FixtureBehavior::PreflightFails);

        let result = workbench
            .group_trial_start(&[1, 2], request())
            .expect("preflight returns a structured result");

        assert_eq!(result["ok"], false);
        assert_eq!(result["stage"], "prepare");
        assert_eq!(
            ready.observed.lock().expect("ready observations").targets,
            0
        );
        assert_eq!(
            rejected
                .observed
                .lock()
                .expect("rejected observations")
                .targets,
            0
        );
        assert!(workbench.group_nodes.is_empty());
    }

    #[test]
    fn group_unknown_start_keeps_original_stop_scope_without_target_retry() {
        let (mut workbench, ready, unknown) =
            group_workbench(FixtureBehavior::Ready, FixtureBehavior::StartUnknown);

        let started = workbench
            .group_trial_start(&[1, 2], request())
            .expect("group start returns results");
        assert_eq!(started["ok"], false);
        assert_eq!(
            ready.observed.lock().expect("ready observations").targets,
            1
        );
        assert_eq!(
            unknown
                .observed
                .lock()
                .expect("unknown observations")
                .targets,
            1
        );

        let stopped = workbench
            .group_stop(&[])
            .expect("original range is retained");
        assert_eq!(stopped["nodes"], json!([1, 2]));
        assert!(
            stopped["results"]
                .as_array()
                .expect("group results")
                .iter()
                .any(|result| result["node"] == 2)
        );
        assert_eq!(ready.observed.lock().expect("ready observations").stops, 1);
        // The unknown initial target already submitted one Stop. Group Stop observes that
        // outstanding confirmation and does not resend a target or Stop automatically.
        assert_eq!(
            unknown
                .observed
                .lock()
                .expect("unknown observations")
                .targets,
            1
        );
        assert_eq!(
            unknown.observed.lock().expect("unknown observations").stops,
            1
        );
        assert_eq!(
            workbench.group_nodes.iter().copied().collect::<Vec<_>>(),
            [1, 2]
        );
    }

    #[test]
    fn active_usb_trial_blocks_can_start_and_recording_start() {
        let (usb, _) = session_fixture(0, [7; 12], FixtureBehavior::Ready);
        let (can, can_probe) = session_fixture(1, [8; 12], FixtureBehavior::Ready);
        let mut workbench = Workbench::with_test_sessions(usb, BTreeMap::from([(1, can)]));

        workbench
            .trial(Transport::Usb, None, TrialAction::Start(request()))
            .expect("USB trial starts");
        assert!(
            workbench
                .trial(Transport::Can, Some(1), TrialAction::Start(request()))
                .is_err()
        );
        assert!(workbench.recording(Transport::Can, Some(1), true).is_err());
        assert_eq!(
            can_probe.observed.lock().expect("CAN observations").targets,
            0
        );
    }

    #[test]
    fn completed_group_releases_scope_for_next_group_start() {
        let (mut workbench, probe, _) =
            group_workbench(FixtureBehavior::Ready, FixtureBehavior::Ready);

        workbench
            .group_trial_start(&[1], request())
            .expect("first group starts");
        workbench.group_stop(&[]).expect("group stop submits once");
        probe.complete_stop();
        for _ in 0..20 {
            workbench.tick();
            if workbench.group_nodes.is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(workbench.group_nodes.is_empty());
        assert!(
            workbench
                .group_trial_start(&[1], request())
                .expect("second group result")["ok"]
                == true
        );
    }
}
