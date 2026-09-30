// Copyright The eha_controller Contributors

//! 与传输无关的客户会话关联。
//!
//! 本模块只把已校验的 [`protocol::Response`] 与调用方建立的查询或维护会话关联。
//! 它不持有 I/O、时间、重试队列或控制状态，也不会在超时、断连或释放时发送停止、复位
//! 或任何维护请求。

use protocol::{Message, OperationKey, Response};

/// SDK 保存并恢复维护结果所使用的自有键。
pub type MaintenanceKey = protocol::OperationKeyFields;

/// 公共协议当前版本。
///
/// `protocol` 在解码时已经拒绝其他版本；此常量供宿主接入说明其会话所使用的版本。
pub const PROTOCOL_VERSION: u8 = 1;

/// 本地会话不能继续生成合同所需标识或尚未取得对象身份。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionError {
    /// 非身份查询或维护操作需要先核对一个可用运行实例。
    IdentityRequired,
    /// Identity 表达的运行实例不可用，不能用于关联后续查询或维护键。
    RunNonceUnavailable,
    /// 所有非零查询标识均已分配，不能回绕或复用。
    QueryIdExhausted,
    /// 已知身份的维护操作标识已耗尽，不能回绕或复用。
    OperationIdExhausted,
}

/// 已核对的控制器身份及会话关联所需事实。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BoundIdentity {
    /// 固定控制器 UID。
    pub uid: [u8; 12],
    /// 当前应用启动生成的非零运行实例。
    pub run_nonce: [u8; 16],
    /// 固件已经处理过的最大维护操作标识。
    pub high_water_operation_id: u64,
    /// 固件当前留存的维护结果标识；零表示没有留存结果。
    pub retained_operation_id: u64,
    /// 固件实际使用的主机心跳频率，单位 Hz。
    pub host_heartbeat_hz: u32,
    /// 固件实际发布的遥测频率，单位 Hz。
    pub telemetry_hz: u32,
    /// 固件允许的主机联系最大年龄，单位 ms。
    pub host_contact_max_age_ms: u32,
    /// 当前实际 CAN 节点号。
    pub active_can_node: u32,
    /// 当前配置格式版本。
    pub config_format_version: u32,
}

/// 接收 Identity 后的本地会话变化。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IdentityEvent {
    /// 首次建立身份绑定。
    Established(BoundIdentity),
    /// 同一 UID 和运行实例的身份事实刷新。
    Refreshed(BoundIdentity),
    /// UID 或运行实例改变；之前发出的匹配器不再匹配新会话。
    Changed {
        /// 先前绑定。
        previous: BoundIdentity,
        /// 新绑定。
        current: BoundIdentity,
    },
}

/// 一个由调用方选择的查询主题。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueryKind {
    /// 核对对象身份；这是唯一允许在尚未绑定时创建的查询。
    Identity,
    /// 读取当前状态。
    Status,
    /// 读取详细测量。
    Measurements,
    /// 读取详细诊断。
    Diagnostics,
    /// 读取指定配置视图；视图值由公共协议定义。
    Config(u8),
    /// 读取指定维护键的结果。
    Result(MaintenanceKey),
}

/// 一个已分配且不可复用的查询关联。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QueryMatch {
    /// 此请求的非零查询标识。
    pub query_id: u32,
    /// 本次查询选择的主题。
    pub kind: QueryKind,
    run_nonce: Option<[u8; 16]>,
    maintenance_key_bytes: Option<[u8; 36]>,
}

impl QueryMatch {
    /// 生成与本关联对应的公共请求。
    ///
    /// 调用方仍负责将该完整业务消息交给所选 CAN 或 USB 承载，并分别记录本地提交、
    /// 链路与固件结果。
    pub fn request(&self) -> Result<Message<'_>, protocol::Error> {
        Ok(match &self.kind {
            QueryKind::Identity
            | QueryKind::Status
            | QueryKind::Measurements
            | QueryKind::Diagnostics => Message::Query {
                query_id: self.query_id,
                category: self.category(),
            },
            QueryKind::Config(view) => Message::ReadConfig {
                query_id: self.query_id,
                view: *view,
            },
            QueryKind::Result(_) => Message::ReadResult {
                query_id: self.query_id,
                key: OperationKey::new(
                    self.maintenance_key_bytes
                        .as_ref()
                        .ok_or(protocol::Error::InvalidField)?,
                )?,
            },
        })
    }

    /// 判断一个已校验回复是否精确属于本查询。
    ///
    /// 除首次 Identity 查询以外，普通查询匹配当前运行实例。维护键的正向结果还必须
    /// 匹配该键自己的原运行实例；当前运行实例可以对旧键明确回复不可取得。
    #[must_use]
    pub fn matches(&self, response: &Response<'_>) -> bool {
        match (self.kind, response) {
            (QueryKind::Identity, Response::Identity(value)) => {
                value.sample().query_id() == self.query_id
            }
            (QueryKind::Status, Response::Status(value)) => self.matches_sample(value.sample()),
            (QueryKind::Measurements, Response::Measurements(value)) => {
                self.matches_sample(value.sample())
            }
            (QueryKind::Diagnostics, Response::Diagnostics(value)) => {
                self.matches_sample(value.sample())
            }
            (QueryKind::Config(view), Response::ConfigData(value)) => {
                self.matches_sample(value.sample()) && value.fields().view as u8 == view
            }
            (QueryKind::Result(key), Response::OperationResult(value)) => {
                value.sample().query_id() == self.query_id
                    && value.sample().run_nonce() == key.run_nonce
                    && value.fields().operation_id == key.operation_id
            }
            (kind, Response::DataUnavailable(value)) => {
                let fields = value.fields();
                self.matches_sample_or_initial_identity(value.sample(), kind)
                    && unavailable_subject_matches(kind, fields.subject)
                    && match kind {
                        QueryKind::Result(key) => fields.operation_id == key.operation_id,
                        _ => fields.operation_id == 0,
                    }
            }
            _ => false,
        }
    }

    const fn category(self) -> u8 {
        match self.kind {
            QueryKind::Identity => 0,
            QueryKind::Status => 1,
            QueryKind::Measurements => 2,
            QueryKind::Diagnostics => 3,
            QueryKind::Config(_) | QueryKind::Result(_) => 0,
        }
    }

    fn matches_sample(&self, sample: protocol::Sample<'_>) -> bool {
        sample.query_id() == self.query_id
            && self
                .run_nonce
                .is_some_and(|expected| sample.run_nonce() == expected)
    }

    fn matches_sample_or_initial_identity(
        &self,
        sample: protocol::Sample<'_>,
        kind: QueryKind,
    ) -> bool {
        match kind {
            QueryKind::Identity => sample.query_id() == self.query_id,
            _ => self.matches_sample(sample),
        }
    }
}

fn unavailable_subject_matches(
    kind: QueryKind,
    subject: protocol::responses::UnavailableSubject,
) -> bool {
    matches!(
        (kind, subject),
        (
            QueryKind::Identity,
            protocol::responses::UnavailableSubject::Identity
        ) | (
            QueryKind::Status,
            protocol::responses::UnavailableSubject::Status
        ) | (
            QueryKind::Measurements,
            protocol::responses::UnavailableSubject::Measurements
        ) | (
            QueryKind::Diagnostics,
            protocol::responses::UnavailableSubject::Diagnostics
        ) | (
            QueryKind::Config(_),
            protocol::responses::UnavailableSubject::Config
        ) | (
            QueryKind::Result(_),
            protocol::responses::UnavailableSubject::OperationResult
        )
    )
}

/// 传输无关的、无分配会话核心。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Session {
    identity: Option<BoundIdentity>,
    next_query_id: Option<u32>,
    next_operation_id: Option<u64>,
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}

impl Session {
    /// 从首个非零查询标识开始建立一个空会话。
    #[must_use]
    pub const fn new() -> Self {
        Self {
            identity: None,
            next_query_id: Some(1),
            next_operation_id: None,
        }
    }

    /// 返回当前已核对的对象身份。
    #[must_use]
    pub const fn identity(&self) -> Option<BoundIdentity> {
        self.identity
    }

    /// 采用一份已校验的 Identity 回复。
    ///
    /// Identity 的公共版本已经由 `protocol` 校验。本方法额外拒绝不可用于会话关联的
    /// 全零运行实例，并且永远不会改写已经交给调用方的 [`MaintenanceKey`]。
    pub fn observe_identity(
        &mut self,
        identity: &protocol::Identity<'_>,
    ) -> Result<IdentityEvent, SessionError> {
        let fields = identity.fields();
        if fields.sample.run_nonce == [0; 16] {
            return Err(SessionError::RunNonceUnavailable);
        }
        let current = BoundIdentity {
            uid: fields.uid,
            run_nonce: fields.sample.run_nonce,
            high_water_operation_id: fields.high_water_operation_id,
            retained_operation_id: fields.retained_operation_id,
            host_heartbeat_hz: fields.host_heartbeat_hz,
            telemetry_hz: fields.telemetry_hz,
            host_contact_max_age_ms: fields.host_contact_max_age_ms,
            active_can_node: fields.active_can_node,
            config_format_version: fields.config_format_version,
        };
        let event = match self.identity {
            None => IdentityEvent::Established(current),
            Some(previous)
                if previous.uid == current.uid && previous.run_nonce == current.run_nonce =>
            {
                IdentityEvent::Refreshed(current)
            }
            Some(previous) => IdentityEvent::Changed { previous, current },
        };
        self.identity = Some(current);
        let floor = current.high_water_operation_id.checked_add(1);
        self.next_operation_id = match event {
            IdentityEvent::Refreshed(_) => self
                .next_operation_id
                .and_then(|next| floor.map(|floor| next.max(floor))),
            IdentityEvent::Established(_) | IdentityEvent::Changed { .. } => floor,
        };
        Ok(event)
    }

    /// 分配一个新的查询关联。
    ///
    /// 除 Identity 外的所有查询都会冻结当前运行实例。若固件在回复前重启，匹配器返回
    /// `false`，调用方应重新核对身份后用新标识重查。
    pub fn next_query(&mut self, kind: QueryKind) -> Result<QueryMatch, SessionError> {
        let run_nonce = match kind {
            QueryKind::Identity => None,
            _ => Some(
                self.identity
                    .ok_or(SessionError::IdentityRequired)?
                    .run_nonce,
            ),
        };
        let query_id = self.next_query_id.ok_or(SessionError::QueryIdExhausted)?;
        self.next_query_id = query_id.checked_add(1);
        Ok(QueryMatch {
            query_id,
            kind,
            run_nonce,
            maintenance_key_bytes: match kind {
                QueryKind::Result(key) => Some(key.to_bytes()),
                _ => None,
            },
        })
    }

    /// 分配一个属于当前 UID 和运行实例的维护键。
    ///
    /// 该调用只建立一个本地请求身份；它不表示消息已提交、设备已接收或维护操作已经
    /// 开始。调用方在任何不确定结果后保存返回键并读取实际结果。
    pub fn next_operation(&mut self) -> Result<MaintenanceKey, SessionError> {
        let identity = self.identity.ok_or(SessionError::IdentityRequired)?;
        let operation_id = self
            .next_operation_id
            .ok_or(SessionError::OperationIdExhausted)?;
        self.next_operation_id = operation_id.checked_add(1);
        Ok(MaintenanceKey::new(
            identity.uid,
            identity.run_nonce,
            operation_id,
        ))
    }
}
