// Copyright The eha-sdk Contributors

// A transport interruption holds the original trial scope until reconnect
// identifies the same running instance or proves that it has changed.
const LIVE = new Set(["active", "stopping", "interrupted_unknown"]);

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
  if (trial.state === "interrupted_unknown") return trial.stop_available
    ? `试验中断，结果未知：${reasonText(trial.reason)}；已重连同一运行实例，请显式 Stop 并等待新遥测。`
    : `试验中断，结果未知：${reasonText(trial.reason)}；等待重连后核对同一运行实例。`;
  if (trial.state === "interrupted_unrecoverable") return `试验中断，结果未知：${reasonText(trial.reason)}；设备或运行实例已变化，不能确认原试验 Stop。`;
  if (trial.state === "prepared") return "正在进行试验预检。";
  if (trial.state === "active") return "试验运行中；请以遥测和停止状态判断实际影响。";
  return "未运行。";
}

export function trialState(snapshot, canNodes = [], groupNodes = [], sessions = {}) {
  const selectedTrial = snapshot?.trial ?? {};
  const selectedState = selectedTrial.state;
  const selectedLive = LIVE.has(selectedState);
  const members = Array.isArray(groupNodes) ? groupNodes : [];
  const byNode = new Map(canNodes.filter((entry) => Number.isInteger(entry.node)).map((entry) => [entry.node, entry.trial ?? entry.snapshot?.trial ?? {}]));
  // A USB Identity can expose its configured CAN node.  It is not a CAN
  // session snapshot and must never replace that node's actual trial state.
  const selectedNode = snapshot?.connection?.transport === "can" ? snapshot.connection.node : undefined;
  if (Number.isInteger(selectedNode)) byNode.set(selectedNode, selectedTrial);
  const groupScope = members.length > 1;
  const groupTrials = members.map((node) => byNode.get(node) ?? {});
  const groupStates = groupTrials.map((trial) => trial.state);
  const groupLive = groupScope && groupStates.some((state) => LIVE.has(state));
  const sessionLive = Object.values(sessions).some((entry) => LIVE.has(stateOf(entry)));
  const nodeLive = [...byNode.values()].some((trial) => LIVE.has(trial.state));
  const live = selectedLive || sessionLive || nodeLive;
  const stopping = selectedState === "stopping" || groupStates.includes("stopping");
  const terminalUnknown = selectedTrial.unknown === true;
  const stopAvailableFor = (trial) => LIVE.has(trial?.state) && (trial.state !== "interrupted_unknown" || trial.stop_available === true);
  const stopAvailable = groupScope ? groupTrials.some(stopAvailableFor) : stopAvailableFor(selectedTrial);
  const label = selectedState === "interrupted_unknown" ? stopAvailable ? "结果未知（需显式停止）" : "结果未知（等待重连）"
    : selectedState === "interrupted_unrecoverable" ? "结果未知（运行实例已变化）"
    : stopping ? "停止中" : live ? "运行中" : selectedState === "completed" ? "已完成" : selectedState === "prepared" ? "预检中" : "未运行";
  return {
    label,
    live,
    stopping,
    groupScope: groupScope && groupLive,
    // A single-node stop is only meaningful for the selected owner.  Group
    // Stop is owner-independent because the service has the locked members.
    stopAvailable,
    terminalUnknown,
  };
}
