// Copyright The eha-sdk Contributors

// A trial is live only while the service reports it as active or stopping.
// `unknown` is evidence about the terminal result, not permission to keep
// blocking recovery actions or to imply a retryable running command.
const LIVE = new Set(["active", "stopping"]);

function stateOf(entry) { return entry?.trial?.state ?? entry?.snapshot?.trial?.state; }

const reasonText = (reason) => ({
  explicit:"显式停止", duration_elapsed:"达到本次时长上限", telemetry_missing:"遥测缺失",
  runtime_facts_invalid:"运行事实失效", runtime_envelope_exceeded:"超出本次包络", position_reached:"位置到位",
}[reason] || reason || "未报告原因");

export function trialStatusText(trial = {}) {
  const submissionUnconfirmed = trial.stop_submission?.error || trial.stop_submission?.unknown || trial.stop_submission?.unknown_no_retry;
  if (trial.state === "stopping") return `停止中：${reasonText(trial.stop_reason)}${submissionUnconfirmed ? "；Stop 提交结果未确认，等待新遥测。" : trial.stop_submission ? "；Stop 已提交，等待新遥测。" : "。"}`;
  if (trial.state === "completed") {
    const observed = trial.stop_observed ? "；已观测到无目标且驱动 Idle。" : "。";
    return `试验已完成：${reasonText(trial.stop_reason)}${submissionUnconfirmed ? "；Stop 提交结果未确认" : ""}${observed}`;
  }
  if (trial.state === "interrupted_unknown") return `试验中断，结果未知：${reasonText(trial.reason)}。`;
  if (trial.state === "prepared") return "正在进行试验预检。";
  if (trial.state === "active") return "试验运行中；请以遥测和停止状态判断实际影响。";
  return "未运行。";
}

export function trialState(snapshot, canNodes = [], groupNodes = [], sessions = {}) {
  const selectedState = snapshot?.trial?.state;
  const selectedLive = LIVE.has(selectedState);
  const members = Array.isArray(groupNodes) ? groupNodes : [];
  const byNode = new Map(canNodes.filter((entry) => Number.isInteger(entry.node)).map((entry) => [entry.node, stateOf(entry)]));
  // A USB Identity can expose its configured CAN node.  It is not a CAN
  // session snapshot and must never replace that node's actual trial state.
  const selectedNode = snapshot?.connection?.transport === "can" ? snapshot.connection.node : undefined;
  if (Number.isInteger(selectedNode)) byNode.set(selectedNode, selectedState);
  const groupScope = members.length > 1;
  const groupStates = members.map((node) => byNode.get(node));
  const groupLive = groupScope && groupStates.some((state) => LIVE.has(state));
  const sessionLive = Object.values(sessions).some((entry) => LIVE.has(stateOf(entry)));
  const nodeLive = [...byNode.values()].some((state) => LIVE.has(state));
  const live = selectedLive || sessionLive || nodeLive;
  const stopping = selectedState === "stopping" || groupStates.includes("stopping");
  const terminalUnknown = snapshot?.trial?.unknown === true && !selectedLive;
  return {
    live,
    stopping,
    groupScope: groupScope && groupLive,
    // A single-node stop is only meaningful for the selected owner.  Group
    // Stop is owner-independent because the service has the locked members.
    stopAvailable: groupScope ? groupLive : selectedLive,
    terminalUnknown,
  };
}
