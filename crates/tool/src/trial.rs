// Copyright The eha-sdk Contributors

//! 单次显式试验的客户端范围和生命周期状态。

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{Command, SessionError, invalid};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TrialEnvelope {
    #[serde(deserialize_with = "crate::deserialize_f32")]
    pub position_min_mm: f32,
    #[serde(deserialize_with = "crate::deserialize_f32")]
    pub position_max_mm: f32,
    #[serde(deserialize_with = "crate::deserialize_f32")]
    pub velocity_abs_max_mm_s: f32,
    #[serde(deserialize_with = "crate::deserialize_f32")]
    pub force_abs_max_n: f32,
    #[serde(deserialize_with = "crate::deserialize_f32")]
    pub stiffness_max_n_per_mm: f32,
    #[serde(deserialize_with = "crate::deserialize_f32")]
    pub damping_max_ns_per_mm: f32,
    #[serde(deserialize_with = "crate::deserialize_f32")]
    pub duration_max_s: f32,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReachCondition {
    #[serde(deserialize_with = "crate::deserialize_f32")]
    pub tolerance_mm: f32,
    pub settle_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TrialRequest {
    pub command: Command,
    pub envelope: TrialEnvelope,
    #[serde(default, deserialize_with = "crate::deserialize_optional_f32")]
    pub duration_s: Option<f32>,
    pub reach: Option<ReachCondition>,
}

pub(crate) struct PreparedTrial {
    pub(crate) request: TrialRequest,
    pub(crate) status: Value,
    pub(crate) startup: Value,
    pub(crate) identity: Value,
    pub(crate) connection: Option<super::ConnectionRequest>,
}

pub(crate) struct ActiveTrial {
    pub(crate) request: TrialRequest,
    pub(crate) started_at: Instant,
    pub(crate) duration: Duration,
    pub(crate) pending_position_mm: Option<f32>,
    pub(crate) last_position_submit: Option<Instant>,
    pub(crate) settled_since: Option<Instant>,
    pub(crate) stop_submission: Option<Value>,
    pub(crate) stop_reason: Option<String>,
    pub(crate) stop_observed: bool,
    pub(crate) started_sample_time_us: u64,
    pub(crate) stop_after_sample_time_us: Option<u64>,
    pub(crate) stop_attempt_finished_at: Option<Instant>,
    pub(crate) settled_sample_time_us: Option<u64>,
}

#[derive(Default)]
pub(crate) struct TrialState {
    pub(crate) prepared: Option<PreparedTrial>,
    pub(crate) active: Option<ActiveTrial>,
    pub(crate) completed: Option<Value>,
}

impl TrialState {
    pub(crate) fn snapshot(&self) -> Value {
        if let Some(active) = &self.active {
            return json!({
                "state": if active.stop_submission.is_some() { "stopping" } else { "active" },
                "request": active.request,
                "elapsed_ms": active.started_at.elapsed().as_millis(),
                "pending_position_mm": active.pending_position_mm,
                "stop_reason": active.stop_reason,
                "stop_submission": active.stop_submission,
                "stop_observed": active.stop_observed,
            });
        }
        if let Some(prepared) = &self.prepared {
            return json!({"state":"prepared", "request": prepared.request, "status": prepared.status, "startup": prepared.startup});
        }
        if let Some(completed) = &self.completed {
            return completed.clone();
        }
        json!({"state":"idle"})
    }
}

impl TrialRequest {
    pub(crate) fn duration_limit(&self) -> Result<Duration, SessionError> {
        Duration::try_from_secs_f32(self.duration_s.unwrap_or(self.envelope.duration_max_s))
            .ok()
            .filter(|duration| !duration.is_zero())
            .ok_or_else(|| invalid("试验时长必须为主机时钟可表示的正数"))
    }
}
