// Copyright The eha-sdk Contributors

import { createTelemetryChart, selectedContactGuidance, telemetryReception } from "./telemetry.js";
import { createConfigEditor } from "./config-editor.js";
import { createTrialUi } from "./trial-ui.js";
import { createRecordingUi } from "./recording-ui.js";
import { trialState } from "./trial-state.js";

const $ = (selector) => document.querySelector(selector);
const $$ = (selector) => [...document.querySelectorAll(selector)];
const notice = $("#notice");
let noticeTimer;
let noticeRemaining = 0;
let noticeStartedAt = 0;
let noticeReturnFocus;
const result = $("#result");
let busy = false;
let odriveBusy = false;
let odriveRequestEpoch = 0;
let noticeRevision = 0;
let snapshot = null;
let snapshotReady = false;
let unavailableSince = false;
let sessionRevision = 0;
let activeTransport = "usb";
let activeCanNode = null;
let sessions = { usb:null, can:null };
let canNodes = [];
let canScanNodes = [];
let groupNodes = [];
const viewStates = { usb:null, can:null };
const connectionFormSources = { usb:null, can:null };
const discovering = new Set();
let localControlSubmission = null;
let configDraft = { dirty:false, source:null, baseline:"" };
let operationKeySource = null;
let operationKeyManual = false;
let renderedDiagnostics;
let renderedOdrive;
const chartController = createTelemetryChart({
  container: $("#telemetry-chart"), summary: $("#chart-summary"), empty: $("#chart-empty"),
  pauseButton: $("#chart-pause"), clearButton: $("#chart-clear"), windowSelect: $("#chart-window"),
});
const configEditor = createConfigEditor({
  get,
  sourceKey: () => sourceKey(),
  onChange: (raw, source, baseline) => { configDraft = { ...configDraft, dirty:true, source:source === undefined ? configDraft.source : source, warning:null, raw, ...(baseline !== undefined ? { baseline } : {}) }; updateDraftState(); },
});
const trialUi = createTrialUi({ run, activeTransport: () => activeTransport });
const recordingUi = createRecordingUi({ get, run, activeTransport: () => activeTransport });

const text = (value, fallback = "—") => value === undefined || value === null || value === "" ? fallback : String(value);
const json = (value) => JSON.stringify(value, null, 2);
const transportLabel = (transport) => transport === "can" ? "CAN" : "USB";
const viewKey = (transport = activeTransport, node = activeCanNode) => transport === "can" ? `can:${node ?? "none"}` : "usb";
const UNKNOWN_AGE_US = 0xffff_ffff;
const SATURATED_AGE_US = 0xffff_fffe;
const NO_CURRENT_TARGET_BLOCKER = 1 << 25;
function formattedRecord(record) {
  try { JSON.parse(record); }
  catch (_) { return { text:record, warning:"原始记录不是有效 JSON，未格式化；校验、保存前需修正。" }; }
  // Only rewrite whitespace: preserve numeric precision and every original string token.
  const tokens = record.match(/"(?:\\.|[^"\\])*"|[{}\[\],:]|[^\s{}\[\],:]+/g) || [];
  let depth = 0;
  let formatted = "";
  const newline = () => "\n" + "  ".repeat(depth);
  tokens.forEach((token, index) => {
    if (token === "{" || token === "[") {
      formatted += token;
      depth++;
      if (tokens[index + 1] !== (token === "{" ? "}" : "]")) formatted += newline();
    } else if (token === "}" || token === "]") {
      depth--;
      if (tokens[index - 1] !== (token === "}" ? "{" : "[")) formatted += newline();
      formatted += token;
    } else formatted += token === "," ? "," + newline() : token === ":" ? ": " : token;
  });
  return { text:formatted + "\n", warning:null };
}
const finiteValue = (value) => typeof value === "number" && Number.isFinite(value) ? value : null;
const formatAge = (age) => age === undefined || age === null ? "未取得" : `${age} ms`;
function ageUsText(ageUs) {
  if (ageUs === undefined || ageUs === null) return undefined;
  if (ageUs === UNKNOWN_AGE_US) return "未收到";
  if (ageUs === SATURATED_AGE_US) return "至少 4294967 ms";
  return Number.isFinite(ageUs) && ageUs >= 0 ? formatAge(Math.round(ageUs / 1000)) : "年龄无效";
}
function roundedNumber(value, maximumFractionDigits = 2) {
  const rounded = Number(value.toFixed(maximumFractionDigits));
  return (Object.is(rounded, -0) ? 0 : rounded).toLocaleString("zh-CN", { maximumFractionDigits });
}
const resultText = (value) => ["从未取得", "可用", "计算失败", "历史不足", "本次未计算", "输出被禁止", "不适用"][value] || "未知结果";
const qualityText = (value) => ["质量未知", "来源合格", "来源故障", "来源中断"][value] || "质量未知";

function observed(value, maximumFractionDigits) {
  const result = value?.result;
  const quality = value?.quality;
  const stale = Boolean(value?.stale);
  const detail = `结果：${resultText(result)}；${qualityText(quality)}；${stale ? "旧值，已过期" : "未过期"}`;
  if (result !== 1) return { number: null, label: `不可用（${detail}）`, detail };
  const number = finiteValue(value.value);
  return number === null ? { number: null, label: `不可用（${detail}）`, detail } : { number, label: `${maximumFractionDigits === undefined ? number : roundedNumber(number, maximumFractionDigits)}（${detail}）`, detail };
}
function targetValues(mode, values) {
  if (!Array.isArray(values)) return undefined;
  if (mode === 1) return `位置 ${values[0]} mm`;
  if (mode === 2) return `速度 ${values[0]} mm/s`;
  if (mode === 3) return `力 ${values[0]} N`;
  if (mode === 4) return `平衡位置 ${values[0]} mm；刚度 ${values[1]} N/mm；阻尼 ${values[2]} N·s/mm`;
  return mode === 0 ? "无当前目标" : undefined;
}

function dismissNotice() {
  window.clearTimeout(noticeTimer);
  const hadFocus = notice.contains(document.activeElement);
  notice.hidden = true;
  if (hadFocus) (noticeReturnFocus?.isConnected && !noticeReturnFocus.disabled && noticeReturnFocus.getClientRects().length ? noticeReturnFocus : $("#main-content")).focus({ preventScroll:true });
}
function pauseNotice() {
  window.clearTimeout(noticeTimer);
  if (noticeStartedAt) {
    noticeRemaining = Math.max(0, noticeRemaining - (performance.now() - noticeStartedAt));
    if (!noticeRemaining) dismissNotice();
  }
  noticeStartedAt = 0;
}
function resumeNotice() {
  if (notice.hidden || !noticeRemaining || noticeStartedAt || notice.matches(":hover, :focus-within")) return;
  noticeStartedAt = performance.now();
  noticeTimer = window.setTimeout(dismissNotice, noticeRemaining);
}
function setNotice(message, state = "") {
  noticeRevision++;
  window.clearTimeout(noticeTimer);
  if (!notice.contains(document.activeElement)) noticeReturnFocus = document.activeElement;
  $("#notice-message").textContent = message;
  notice.className = `notice ${state}`;
  notice.hidden = false;
  noticeRemaining = state === "is-working" ? 0 : state ? 8000 : 4000;
  noticeStartedAt = 0;
  resumeNotice();
  return noticeRevision;
}
$("#notice-close").addEventListener("click", dismissNotice);
notice.addEventListener("mouseenter", pauseNotice);
notice.addEventListener("mouseleave", resumeNotice);
notice.addEventListener("focusin", pauseNotice);
notice.addEventListener("focusout", () => window.setTimeout(resumeNotice, 0));
notice.addEventListener("keydown", (event) => { if (event.key === "Escape") { event.preventDefault(); dismissNotice(); } });
// Keep a newly focused control above the notification without moving the page when it appears.
function revealNoticeFocus() {
  const focused = document.activeElement;
  if (notice.hidden || !focused?.matches("input, select, textarea, button, a[href], [tabindex='0']") || notice.contains(focused)) return;
  const bounds = focused.getBoundingClientRect();
  const overlay = notice.getBoundingClientRect();
  if (bounds.bottom > overlay.top - 12 && bounds.top < overlay.bottom && bounds.right > overlay.left && bounds.left < overlay.right) {
    focused.scrollIntoView({ block:"center", behavior:"instant" });
  }
}
document.addEventListener("focusin", revealNoticeFocus);
document.addEventListener("keydown", (event) => {
  if (event.key === "Escape" && !event.defaultPrevented && !notice.hidden && !$("#sidebar").classList.contains("open")) {
    event.preventDefault();
    dismissNotice();
  }
});
function setBusy(next) {
  busy = next;
  document.body.setAttribute("aria-busy", String(next));
  updateActionAvailability();
}
function setOdriveBusy(next) {
  odriveBusy = next;
  updateActionAvailability();
}
function phase(value = snapshot) {
  if (!snapshotReady) return "loading";
  if (!value?.connected) return "disconnected";
  return value.transport?.disconnected ? "recoverable" : "connected";
}
function deviceReady() { return snapshotReady && phase() === "connected"; }
function setDisabled(selector, disabled) { $$(selector).forEach((element) => { element.disabled = disabled; }); }
function selectSavedOption(select, value) {
  if (!value) return;
  if (![...select.options].some((option) => option.value === value)) select.add(new Option(`${value}（已保存）`, value));
  select.value = value;
}
function syncConnectionForm(transport, connection) {
  if (!connection) return;
  const source = JSON.stringify(connection);
  if (connectionFormSources[transport] === source) return;
  if (transport === "usb") selectSavedOption($("#usb-serial"), connection.serial);
  if (transport === "can") {
    selectSavedOption($("#can-port"), connection.port);
    $("#can-connection-form input[name=node]").value = connection.node ?? "";
    $("#can-connection-form select[name=profile]").value = connection.profile || "";
  }
  connectionFormSources[transport] = source;
}
function sessionForTransport(transport) {
  return transport === "can" ? canNodes.find((entry) => entry.node === activeCanNode)?.snapshot || sessions.can : sessions[transport];
}
function updateConnections() {
  for (const transport of ["usb", "can"]) {
    const session = sessionForTransport(transport);
    const disconnected = session?.transport?.disconnected;
    const connected = session?.connected && !disconnected;
    setPill($(`#${transport}-connection-state`), unavailableSince ? "服务不可达" : disconnected ? "连接已断开" : connected ? "已连接" : "未连接", unavailableSince || disconnected ? "error" : connected ? "success" : "idle");
    syncConnectionForm(transport, session?.connection);
    const identity = session?.identity;
    facts($(`#${transport}-identity`), [["UID", identity?.uid], ["运行实例", identity?.sample?.run_nonce], ["实际 CAN 节点", identity?.active_can_node], ["实际 CAN profile", identity?.active_can_profile_label], ["断连原因", disconnected], ["心跳", session ? session.heartbeat?.enabled ? "已启用" : "未启用" : undefined], ["遥测频率", identity?.telemetry_hz === undefined ? undefined : `${identity.telemetry_hz} Hz`]], "连接后显示设备身份；另一通路可独立连接。");
  }
}
function applyResponse(body, odriveEpoch = odriveRequestEpoch) {
  if (!body) return;
  if (body.sessions) sessions = body.sessions;
  if (Array.isArray(body.can_nodes)) canNodes = body.can_nodes;
  if (Array.isArray(body.group_nodes)) groupNodes = body.group_nodes;
  if (Array.isArray(body.nodes) && body.nodes.some((entry) => entry.snapshot)) canNodes = body.nodes.filter((entry) => Number.isInteger(entry.node)).map((entry) => ({ node:entry.node, snapshot:entry.snapshot, trial:entry.trial, recording:entry.recording, identity:entry.identity }));
  const responseNode = body.snapshot?.connection?.node ?? body.snapshot?.identity?.active_can_node;
  if (body.transport === "can" && Number.isInteger(responseNode)) activeCanNode = responseNode;
  updateCanNodeOptions();
  const selected = body.transport === activeTransport ? body.snapshot || sessionForTransport(activeTransport) : sessionForTransport(activeTransport);
  update(selected, false, !odriveBusy && odriveEpoch === odriveRequestEpoch);
  updateConnections();
}
function updateJourney() {
  const current = phase();
  const title = $("#journey-title"); const detail = $("#journey-detail"); const link = $("#journey-link");
  const otherTransport = activeTransport === "usb" ? "can" : "usb";
  const otherReady = sessionReady(sessions[otherTransport]);
  const otherHint = otherReady ? `${transportLabel(otherTransport)} 已连接，可在页首切换后观察、操作或停止控制。` : null;
  const guidance = selectedContactGuidance(activeTransport, snapshot?.identity, snapshot?.telemetry, snapshot?.heartbeat);
  if (unavailableSince) [title.textContent, detail.textContent, link.hidden] = ["本机 WebUI 服务不可达", "请恢复服务或端口映射；页面会自动重新读取状态，不会重发设备请求。", true];
  else if (busy) [title.textContent, detail.textContent, link.hidden] = ["正在等待前一请求", "当前会话按顺序处理操作；完成前不会发送另一条设备指令。", true];
  else if (current === "loading") [title.textContent, detail.textContent, link.hidden] = ["正在取得会话", "本地服务返回当前状态前，设备操作不会开放。", true];
  else if (current === "disconnected") [title.textContent, detail.textContent, link.href, link.textContent, link.hidden] = ["连接设备", otherHint || (snapshot?.connection ? "可重新连接已保存通路，或重新选择通路后连接并核对身份。" : "先选择通路并核对设备身份，再观察或提交目标。"), "#overview", "连接设备", false];
  else if (current === "recoverable") [title.textContent, detail.textContent, link.href, link.textContent, link.hidden] = ["连接已断开", otherHint || "可用保存的通路重新连接；旧遥测只作历史参考。", "#overview", "重新连接", false];
  else if (guidance) {
    [title.textContent, detail.textContent, link.href, link.textContent, link.hidden] = [guidance.title, guidance.detail, "#telemetry", guidance.link, false];
  }
  else [title.textContent, detail.textContent, link.href, link.textContent, link.hidden] = ["已核对设备身份", "查看当前遥测与控制条件；持续控制时保持本入口心跳，提交目标后核对采用与执行反馈。", "#telemetry", "查看遥测与控制", false];
  title.textContent = `${transportLabel(activeTransport)} · ${title.textContent}`;
}
function updateActionAvailability() {
  const active = deviceReady(); const locked = busy || !snapshotReady;
  const trialLocked = trialState(snapshot, canNodes, groupNodes, sessions).live;
  $("#active-transport").disabled = busy;
  $("#active-can-node").disabled = busy;
  for (const transport of ["usb", "can"]) {
    const session = sessionForTransport(transport);
    const held = Boolean(session?.connected);
    const pending = discovering.has(transport);
    setDisabled(`#${transport}-connection-form input, #${transport}-connection-form select`, busy || held || pending || trialLocked);
    setDisabled(`#discover-${transport}`, busy || held || pending || trialLocked);
    setDisabled(`#connect-${transport}`, locked || held || pending || trialLocked);
    setDisabled(`#reconnect-${transport}`, locked || !session?.connection || (held && !session.transport?.disconnected) || trialLocked);
    setDisabled(`#disconnect-${transport}`, locked || !held || trialLocked);
  }
  setDisabled("button[data-action], .command-form button, [data-stop-control], #save-config, #restore-factory, #reset-application, #enter-update", locked || !active);
  const heartbeat = snapshot?.heartbeat || {};
  setDisabled('[data-action="heartbeat_start"]', locked || !active || heartbeat.enabled);
  setDisabled('[data-action="heartbeat_stop"]', locked || !active || !heartbeat.enabled);
  setDisabled("#validate-config", busy || trialLocked);
  setDisabled("#discover-odrive, #read-odrive, #odrive-serial", busy || odriveBusy || discovering.has("odrive"));
  $("#operation-key").disabled = busy || !active;
  const hasOperationKey = Boolean($("#operation-key").value.trim());
  setDisabled("#query-result, #release-result", locked || !active || !hasOperationKey || trialLocked);
  $("#config-record").disabled = busy || trialLocked;
  setDisabled("#config-import, #config-export", busy || trialLocked);
  setDisabled("#config-fields input, #config-fields select", busy || trialLocked);
  setDisabled("#save-config, #restore-factory, #reset-application, #enter-update, [data-action=\"config_read\"]", busy || trialLocked || !active);
  setDisabled("#can-scan-start, #can-scan-end, #can-scan-nodes, #can-connect-nodes, #can-scan-results input", busy || trialLocked);
  const stopLabel = `停止 ${transportLabel(activeTransport)} 控制`;
  $$('[data-stop-label]').forEach((target) => { target.textContent = stopLabel; });
  $$('[data-stop-control]').forEach((target) => { target.setAttribute("aria-label", stopLabel); });
  updateJourney();
}
function facts(target, entries, empty) {
  const rows = entries.filter(([, value]) => value !== undefined && value !== null && value !== "");
  target.classList.toggle("empty", rows.length === 0);
  target.replaceChildren();
  if (!rows.length) {
    const row = document.createElement("div");
    const term = document.createElement("dt");
    const definition = document.createElement("dd");
    term.textContent = "等待";
    definition.textContent = empty;
    row.append(term, definition);
    target.append(row);
    return;
  }
  for (const [name, value] of rows) {
    const row = document.createElement("div");
    const term = document.createElement("dt");
    const definition = document.createElement("dd");
    term.textContent = name;
    definition.textContent = typeof value === "object" ? json(value) : text(value);
    row.append(term, definition);
    target.append(row);
  }
}
function statusClass(state) {
  return state === "error" ? "is-error" : state === "warning" ? "is-warning" : state === "success" ? "is-success" : "is-idle";
}
function setPill(target, label, state = "idle") {
  target.textContent = label;
  target.className = `status-label ${statusClass(state)}`;
}
function setStatusFact(id, label, state = "idle") {
  const target = $(`#${id}`);
  target.className = `status-fact ${statusClass(state)}`;
  target.querySelector("strong").textContent = label;
  target.querySelector(".status-dot").className = `status-dot ${state === "success" ? "success" : state === "warning" ? "warning" : state === "error" ? "danger" : "neutral"}`;
}

async function responseJson(response) {
  try { return await response.json(); }
  catch (_) { throw new Error("服务返回了非 JSON 响应。"); }
}
async function post(path, payload = {}) {
  const response = await fetch(path, { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(payload), credentials: "same-origin" });
  const body = await responseJson(response);
  if (!response.ok || body.ok === false) {
    const error = new Error(body.message || `HTTP ${response.status}`);
    error.detail = body;
    throw error;
  }
  return body;
}
async function get(path, { timeout = 0 } = {}) {
  const controller = timeout ? new AbortController() : null;
  const timer = controller && window.setTimeout(() => controller.abort(), timeout);
  try {
    const response = await fetch(path, { cache: "no-store", credentials: "same-origin", signal:controller?.signal });
    const body = await responseJson(response);
    if (!response.ok || body.ok === false) {
      const error = new Error(body.message || `HTTP ${response.status}`);
      error.detail = body;
      throw error;
    }
    return body;
  } finally { if (timer) window.clearTimeout(timer); }
}
function actionLabel(action) {
  return ({status:"读取状态", measurements:"读取测量", diagnostics:"读取诊断", heartbeat_start:"启用心跳", heartbeat_stop:"停止心跳", heartbeat_once:"发送一次心跳", position:"提交位置目标", velocity:"提交速度目标", force:"提交力目标", impedance:"提交阻抗目标", stop:"停止控制", config_read:"读取配置", config_validate:"校验配置", config_save:"保存配置", restore_factory:"恢复出厂配置", maintenance_result:"查询维护结果", maintenance_release:"释放维护结果", reset_application:"请求应用复位", enter_update:"请求进入更新入口"})[action] || action || "操作";
}
function renderOperationSummary(body, fallback, state = "success") {
  const data = body.error || body.result || body.data;
  if (!data && !body.message && body.ok === undefined) return;
  const resultBody = body.result || body;
  const action = resultBody.action || fallback.replace(/^(USB|CAN) · /, "");
  const unknown = Boolean(body.unknown || body.error?.unknown);
  const local = resultBody.local_submission;
  $("#operation-summary").hidden = false;
  $("#operation-title").textContent = `${transportLabel(body.transport || activeTransport)} · ${actionLabel(action)}${state === "working" ? "进行中" : unknown ? "结果未知" : state === "error" ? "失败" : ""}`;
  const control = ["position", "velocity", "force", "impedance", "stop"].includes(resultBody.action);
  const readback = resultBody.data?.readback;
  $("#operation-detail").textContent = unknown ? "未得到可确认的结果；恢复通信后核对当前事实，有维护操作键时可只读查询。" : state === "error" ? (body.message || fallback) : readback ? action === "restore_factory" ? "已恢复出厂记录并读回，编辑器已更新为读回内容。" : "已保存并核对读回，编辑器已更新为读回内容。" : local ? control ? `已本地提交（${local.boundary}）；请对照固件当前目标与输出状态。` : `已本地提交（${local.boundary}）；请查看维护结果与实际读回。` : (body.message || fallback);
  $("#operation-summary").className = `operation-summary is-${unknown ? "warning" : state}`;
  const link = $("#operation-link");
  link.hidden = !body.operation_key && !body.result?.operation_key;
}
function setOperationKey(key, source = snapshot, transport = activeTransport) {
  if (!key) return;
  if (transport !== activeTransport) {
    const view = viewStates[transport] ||= {};
    Object.assign(view, { operationKey:key, operationKeySource:sourceKey(source), operationKeyManual:false, keyState:"本次会话的维护操作键；切换对象后会保留但标为历史。" });
    return;
  }
  $("#operation-key").value = key;
  operationKeySource = sourceKey(source);
  operationKeyManual = false;
  $("#operation-key-state").textContent = `本次会话的维护操作键；切换对象后会保留但标为历史。`;
}
function sourceKey(value = snapshot) {
  const identity = value?.identity;
  return identity?.uid && identity?.sample?.run_nonce ? `${identity.uid}:${identity.sample.run_nonce}` : null;
}
function updateDraftState() {
  const state = $("#config-draft-state"); const current = sourceKey();
  if (!configDraft.source) state.textContent = configDraft.dirty ? "本地草稿尚未从设备读取。" : "草稿尚未从设备读取。";
  else if (configDraft.source !== current) state.textContent = "此草稿来自另一设备或运行实例；保存到当前设备前会再次确认。";
  else state.textContent = `${configDraft.viewLabel || "已读取配置"}，来自当前已核对设备${configDraft.dirty ? "；已修改" : ""}。`;
  if (configDraft.warning) state.textContent += " " + configDraft.warning;
  if (operationKeySource && operationKeySource !== current) $("#operation-key-state").textContent = "此操作键来自另一设备或运行实例；仍可明确查询，不会自动重发。";
}
function showResult(body, label, state = "success") {
  if (body.error) result.textContent = json(body.error);
  else if (body.result) result.textContent = json(body.result);
  else if (body.data) result.textContent = json(body.data);
  else if (body.results || body.nodes || body.trial || body.recording) result.textContent = json(body.results || body.nodes || body.trial || body.recording);
  const key = body.operation_key || body.result?.operation_key;
  if (key) setOperationKey(key, body.snapshot || snapshot, body.transport || activeTransport);
  const action = body.result?.action;
  const config = action === "config_read" ? body.result.data : ["config_save", "restore_factory"].includes(action) ? body.result.data?.readback : null;
  if (typeof config?.data_utf8 === "string") {
    const record = $("#config-record");
    const overwrite = action !== "config_read" || !configDraft.dirty || !record.value || window.confirm("当前草稿已有未保存修改。读取的配置将覆盖它，是否继续？");
    if (overwrite) {
      record.value = config.data_utf8;
      configDraft = { dirty:false, source:sourceKey(body.snapshot || snapshot), view:config.view, viewLabel:action === "restore_factory" ? "已恢复出厂并读回用户配置" : config.view_label, warning:null, baseline:config.data_utf8 };
      configEditor.setDocument(config.data_utf8, configDraft.source, configDraft.baseline);
      updateDraftState();
    }
  }
  renderOperationSummary(body, label, state);
}
function sessionReady(session) {
  return Boolean(session?.connected && !session.transport?.disconnected);
}
function selectConnectedTransport(path, transport, body) {
  if (!body?.ok || transport === activeTransport || !["/api/connect", "/api/reconnect"].includes(path) || !sessionReady(sessionForTransport(transport))) return null;
  if (sessionReady(sessionForTransport(activeTransport))) {
    return `${transportLabel(transport)} 已连接并核对身份；当前操作通路仍为 ${transportLabel(activeTransport)}，如需经 ${transportLabel(transport)} 操作，请在页首选择该通路。`;
  }
  selectTransport(transport, { force:true, announce:false });
  return `${transportLabel(transport)} 已连接并核对身份，已切换为当前操作通路。`;
}
async function run(path, payload, label) {
  if (busy) return;
  const transport = payload.transport || activeTransport;
  const odriveEpoch = odriveRequestEpoch;
  payload = { ...payload, transport };
  if (transport === "can" && payload.node === undefined && !["/api/group", "/api/can/scan", "/api/can/connect"].includes(path) && Number.isInteger(activeCanNode)) payload.node = activeCanNode;
  label = `${transportLabel(transport)} · ${label}`;
  sessionRevision++;
  setBusy(true);
  setNotice(`${label}…`, "is-working");
  renderOperationSummary({ transport, data:{}, result:{ action:path === "/api/action" ? payload.action : label.split(" · ")[1] } }, label, "working");
  try {
    const body = await post(path, payload);
    body.transport ??= transport;
    if (transport === "can" && Number.isInteger(payload.node)) activeCanNode = payload.node;
    applyResponse(body, odriveEpoch);
    const connectionNotice = selectConnectedTransport(path, transport, body);
    if (["position", "velocity", "force", "impedance", "stop"].includes(payload.action) && body.result?.local_submission) {
      localControlSubmission = { action:payload.action, parameters:Object.fromEntries(Object.entries(payload).filter(([key]) => !["action", "transport"].includes(key))), boundary:body.result.local_submission.boundary, source:sourceKey(snapshot) };
      update(snapshot, false, !odriveBusy && odriveEpoch === odriveRequestEpoch);
    }
    if (payload.action === "maintenance_release") {
      $("#operation-key").value = ""; operationKeySource = null; operationKeyManual = false;
      $("#operation-key-state").textContent = "已提交释放并完成确认；操作键已从输入框清除。";
    }
    showResult(body, label);
    setNotice(connectionNotice || `${label}：${body.message || "已完成。"}`, body.unknown ? "is-warning" : "");
    return body;
  } catch (error) {
    const detail = error.detail;
    if (["position", "velocity", "force", "impedance", "stop"].includes(payload.action)) localControlSubmission = null;
    applyResponse(detail, odriveEpoch);
    if (detail) showResult(detail, label, "error");
    else {
      unavailable();
      renderOperationSummary({ transport, action:payload.action, error:{message:error.message}, unknown:true }, label, "warning");
    }
    if (detail?.operation_key) setOperationKey(detail.operation_key, detail.snapshot || snapshot, transport);
    const network = !detail;
    setNotice(network ? `${label}未得到服务响应；未判断是否提交或执行，请先恢复服务后查看事实。` : detail.unknown ? `${label}结果未知：${error.message}；保留操作键后只读查询。` : `${label}失败：${error.message}`, network || detail?.unknown ? "is-warning" : "is-error");
    return null;
  } finally {
    setBusy(false);
  }
}

function renderDiagnostics(diagnostics) {
  const content = json(diagnostics?.entries ?? null);
  if (content === renderedDiagnostics) return;
  renderedDiagnostics = content;
  const target = $("#diagnostic-summary");
  target.replaceChildren();
  const entries = diagnostics?.entries;
  if (!Array.isArray(entries) || !entries.length) {
    target.className = "diagnostic-list empty-state";
    target.textContent = diagnostics ? "此次诊断没有报告条目。" : "尚未读取诊断。";
    return;
  }
  target.className = "diagnostic-list";
  for (const entry of entries) {
    const item = document.createElement("details");
    const impact = entry.impact || {};
    const severity = impact.current && (impact.limits_output || impact.ends_target || impact.unknown_side_effect) ? "error" : impact.current ? "warning" : "";
    item.className = `diagnostic-item ${severity}`;
    const summary = document.createElement("summary");
    const title = document.createElement("strong");
    const source = entry.domain_label || entry.domain || entry.native_domain_label || entry.native_domain || "固件诊断";
    const reason = entry.reason_label || entry.reason || entry.code_label || entry.code || "已报告条目";
    const lifetime = impact.current ? "当前影响" : impact.historical ? "历史记录" : "已记录";
    title.textContent = `${source} · ${lifetime}：${reason}`;
    const detail = document.createElement("small");
    const nextActions = Array.isArray(entry.next_action_labels) ? entry.next_action_labels.join("；") : entry.next_action_labels;
    detail.textContent = [entry.native_code_description, entry.phase_label || entry.phase, nextActions].filter(Boolean).join(" · ") || "展开查看原始字段与缺失证据。";
    summary.append(title, detail);
    const raw = document.createElement("pre");
    raw.textContent = json(entry);
    item.append(summary, raw);
    target.append(item);
  }
}

function renderH723Driver(telemetry, disconnected) {
  const summary = telemetry?.driver_summary;
  const raw = telemetry?.driver_state;
  const age = telemetry?.driver_age_us === undefined
    ? summary?.age_ms === undefined ? undefined : formatAge(summary.age_ms)
    : ageUsText(telemetry.driver_age_us);
  const state = summary?.state || (raw?.has_status ? raw.stale ? "stale" : raw.faulted ? "faulted" : raw.qualified ? "available" : "unqualified" : "unavailable");
  const hostCacheExpired = Boolean(telemetry) && (Boolean(disconnected) || (telemetry.received_age_ms ?? 0) > 1000);
  const observedLabel = summary?.label || ({available:"ODrive 状态合格", stale:"ODrive 状态已过期", faulted:"ODrive 报告故障", unqualified:"ODrive 状态未获合格", unavailable:"未取得 ODrive 状态"})[state] || "未知";
  const label = hostCacheExpired ? "主机缓存已过期" : observedLabel;
  const visual = !telemetry ? "idle" : hostCacheExpired ? (disconnected ? "error" : "warning") : state === "available" ? "success" : state === "faulted" ? "error" : "warning";
  setPill($("#driver-age"), age === undefined ? label : `${label} · ${age}`, visual);
  setStatusFact("odrive-state", label, visual);
  if (!telemetry) return facts($("#h723-odrive"), [], "尚未取得 H723 状态。");
  facts($("#h723-odrive"), [
    ["当前展示", label], ["最后 H723 接收年龄", formatAge(telemetry.received_age_ms)], ["驱动反馈年龄", age],
    ["有状态来源", raw?.has_status], ["来源合格", raw?.qualified], ["来源过期", raw?.stale], ["驱动故障", raw?.faulted],
    ["轴状态", !hostCacheExpired ? summary?.axis_state_label ?? (state === "available" ? telemetry.axis_state_raw : undefined) : undefined],
    ["轴错误", !hostCacheExpired ? summary?.axis_error_description ?? (state === "available" ? telemetry.axis_error_raw : undefined) : undefined],
    ["目标轴状态", telemetry.desired_axis_label ?? telemetry.desired_axis],
  ], "H723 未提供 ODrive 状态来源。");
}

function renderOdriveResponse(response) {
  if (!response) return;
  const content = json(response);
  if (content === renderedOdrive) return;
  renderedOdrive = content;
  const failed = response.ok === false;
  const snapshot = response.snapshot;
  if (failed || !snapshot) {
    setPill($("#odrive-usb-state"), failed ? "读取失败" : "尚未读取", failed ? "error" : "idle");
    const message = response.message || response.error?.message || "ODrive USB 没有返回可用快照。";
    facts($("#odrive-identity"), [], `ODrive USB 读取失败：${message}`);
    facts($("#odrive-status"), [], `ODrive USB 读取失败：${message}`);
    facts($("#odrive-metrics"), [], `ODrive USB 读取失败：${message}`);
    $("#odrive-raw").textContent = json(response);
    if (response.serial_number && !$("#odrive-serial").value) selectSavedOption($("#odrive-serial"), response.serial_number);
    return;
  }
  const hasErrors = snapshot.can_error || [snapshot.axis0, snapshot.axis1].some((axis) => axis && (axis.error || axis.motor_error || axis.encoder_error || axis.controller_error));
  setPill($("#odrive-usb-state"), `已读取${hasErrors ? " · 存在错误" : " · 无报告错误位"} · ${text(snapshot.serial_number)}`, hasErrors ? "error" : "idle");
  if (snapshot.serial_number && !$("#odrive-serial").value) selectSavedOption($("#odrive-serial"), snapshot.serial_number);
  const firmware = snapshot.firmware_version;
  facts($("#odrive-identity"), [
    ["来源", snapshot.source], ["USB 序列号", snapshot.serial_number],
    ["固件版本", firmware ? `${firmware.major}.${firmware.minor}.${firmware.revision}${firmware.unreleased ? " (unreleased)" : ""}` : undefined],
    ["开始时间", new Date(snapshot.read_started_unix_ms).toLocaleString()], ["完成时间", new Date(snapshot.read_finished_unix_ms).toLocaleString()],
    ["CAN 错误", snapshot.can_error_description ?? snapshot.can_error],
  ], "ODrive 未返回身份信息。");
  const axes = ["axis0", "axis1"];
  const axisEntries = axes.flatMap((name) => {
    const axis = snapshot[name];
    if (!axis) return [[`${name} 状态`, "未返回"]];
    return [[`${name} 状态`, axis.current_state_description ?? axis.current_state], [`${name} 错误`, axis.error_description ?? axis.error], [`${name} 电机错误`, axis.motor_error_description ?? axis.motor_error], [`${name} 编码器错误`, axis.encoder_error_description ?? axis.encoder_error], [`${name} 控制器错误`, axis.controller_error_description ?? axis.controller_error], [`${name} 电机已标定`, axis.motor_calibrated], [`${name} 编码器就绪`, axis.encoder_ready], [`${name} 位置估计（转）`, axis.encoder_position_estimate], [`${name} 速度估计（转/s）`, axis.encoder_velocity_estimate]];
  });
  facts($("#odrive-status"), axisEntries, "ODrive 未返回轴状态。");
  facts($("#odrive-metrics"), [["母线电压（V）", snapshot.metrics?.vbus_voltage], ["母线电流（A）", snapshot.metrics?.ibus], ["FET 温度（°C）", snapshot.metrics?.fet_temperature], ["电机温度（°C）", snapshot.metrics?.motor_temperature], ["q 轴电流（A）", snapshot.metrics?.iq_measured], ["不可取得字段", snapshot.unavailable]], "ODrive 未返回量测。");
  $("#odrive-raw").textContent = json(response);
}

function update(nextSnapshot, cached = false, renderOdrive = true) {
  if (!nextSnapshot) return;
  snapshot = nextSnapshot;
  if (snapshot.connection?.transport === "usb" && snapshot.trial) sessions.usb = { ...sessions.usb, trial:snapshot.trial };
  if (!cached) {
    snapshotReady = true;
    const recovered = unavailableSince;
    unavailableSince = false;
    if (recovered) setNotice("本机 WebUI 服务已恢复；已重新取得当前会话状态。");
  }
  const disconnected = snapshot.transport?.disconnected;
  const connectionInactive = !snapshot.connected || Boolean(disconnected);
  if (localControlSubmission?.source && localControlSubmission.source !== sourceKey(snapshot)) localControlSubmission = null;
  setStatusFact("connection-state", `${transportLabel(activeTransport)} · ${disconnected ? "本地连接已断开" : snapshot.connected ? "已连接" : "未连接"}`, disconnected ? "error" : snapshot.connected ? "success" : "idle");
  const telemetry = snapshot.telemetry;
  const age = telemetry?.received_age_ms;
  const reception = telemetryReception(snapshot);
  setStatusFact("telemetry-age", reception.label, reception.state);
  const values = telemetry?.main_values || [];
  const position = observed(values[0]); const velocity = observed(values[1]); const force = observed(values[4]);
  const submittedRpm = observed(telemetry?.last_submitted_rpm, 2);
  const submittedRpmAge = ageUsText(telemetry?.submitted_age_us);
  const outputAllowed = telemetry?.facts?.output_allowed;
  const targetIdle = telemetry?.target_mode === 0;
  const outputBlockers = telemetry?.output_blockers;
  const hasAdditionalOutputBlocker = Number.isInteger(outputBlockers) && (outputBlockers & ~NO_CURRENT_TARGET_BLOCKER) !== 0;
  const cacheExpired = Boolean(telemetry) && (connectionInactive || (age ?? 0) > 1000);
  const firmwareState = cacheExpired ? ["主机缓存已过期", "warning"]
    : !telemetry ? ["未读取", "idle"]
      : telemetry.facts?.unknown_effect ? ["影响未知", "warning"]
        : targetIdle && !hasAdditionalOutputBlocker ? ["无当前目标", "idle"]
          : targetIdle ? ["无当前目标；另有输出阻塞", "warning"]
            : outputAllowed ? ["允许输出", "success"] : ["输出被禁止", "warning"];
  setStatusFact("firmware-state", firmwareState[0], firmwareState[1]);
  facts($("#telemetry-values"), [["数据状态", cacheExpired ? "最后缓存，已过期" : undefined], [connectionInactive ? "缓存距今" : "距最后一帧", age === undefined ? undefined : formatAge(age)], ["位置（mm）", telemetry ? position.label : undefined], ["速度（mm/s）", telemetry ? velocity.label : undefined], ["主要力（N）", telemetry ? force.label : undefined], ["输出允许", outputAllowed], ["输出阻塞项", telemetry?.output_blocker_labels ?? telemetry?.output_blockers], ["最后结束原因", telemetry?.last_end_reason_label ?? telemetry?.last_end_reason], ["未知影响", telemetry?.facts?.unknown_effect], ["Customer CAN 联系", telemetry?.can_contact_label], ["Customer CAN 心跳年龄", ageUsText(telemetry?.can_heartbeat_age_us)], ["USB 联系", telemetry?.usb_contact_label], ["USB 心跳年龄", ageUsText(telemetry?.usb_heartbeat_age_us)]], snapshot.connected && !disconnected ? "尚未收到当前运行实例的新遥测。" : "未连接设备。");
  for (const [name, observation, value] of [["position", position, values[0]], ["velocity", velocity, values[1]], ["force", force, values[4]]]) {
    for (const prefix of ["metric", "live"]) {
      $(`#${prefix}-${name}`).textContent = observation.number === null ? "—" : roundedNumber(observation.number);
      const detail = $(`#${prefix}-${name}-detail`);
      detail.textContent = telemetry ? `${cacheExpired ? "主机缓存已过期；采样时：" : ""}${observation.detail}` : "等待当前设备的遥测";
      detail.classList.toggle("is-warning", Boolean(telemetry) && (cacheExpired || observation.number === null || value?.stale || value?.quality !== 1));
    }
  }
  renderH723Driver(telemetry, connectionInactive);
  chartController.update(snapshot);
  configEditor.sourceChanged();
  trialUi.update(snapshot, canNodes, groupNodes, sessions);
  recordingUi.update(snapshot, trialState(snapshot, canNodes, groupNodes, sessions).live);
  facts($("#control-submission"), localControlSubmission ? [["模式", actionLabel(localControlSubmission.action)], ["参数", localControlSubmission.parameters], ["本地提交", localControlSubmission.boundary]] : [], "尚未对当前运行实例提交控制目标。");
  facts($("#control-adoption"), [["数据状态", cacheExpired ? "最后缓存，已过期" : undefined], ["目标模式", telemetry?.target_mode_label ?? telemetry?.target_mode], ["目标参数", targetValues(telemetry?.target_mode, telemetry?.target_values)], ["控制来源", telemetry?.target_ingress_label ?? telemetry?.target_ingress], ["采用阻塞项", telemetry?.[`${snapshot.connection?.transport}_adoption_blocker_labels`] ?? (snapshot.connection?.transport === "usb" ? telemetry?.usb_adoption_blocker_labels : telemetry?.can_adoption_blocker_labels)], ["输出允许", outputAllowed], ["输出阻塞项", telemetry?.output_blocker_labels ?? telemetry?.output_blockers], ["驱动状态", telemetry?.driver_summary?.axis_state_label], ["最后本地提交转速（rpm）", telemetry?.last_submitted_rpm ? [submittedRpm.label, submittedRpmAge && `距今 ${submittedRpmAge}`, "不是实际测得转速"].filter(Boolean).join("；") : undefined]], "等待当前运行实例的遥测。");
  const heartbeat = snapshot.heartbeat || {};
  const heartbeatHadDeliveryIssue = Boolean(heartbeat.missed || heartbeat.error);
  setPill($("#heartbeat-state"), heartbeat.enabled ? heartbeatHadDeliveryIssue ? "心跳已启用；曾有调度遗漏" : "心跳已启用" : "心跳未启用", heartbeat.enabled ? heartbeatHadDeliveryIssue ? "warning" : "success" : "idle");
  facts($("#heartbeat"), [["已启用", heartbeat.enabled], ["本地提交数", heartbeat.submitted], ["调度遗漏数", heartbeat.missed], ["最后错误", heartbeat.error]], "尚未取得心跳状态。");
  $("#observations").textContent = json({ status:snapshot.last_status, measurements:snapshot.last_measurements, diagnostics:snapshot.last_diagnostics, transport:snapshot.transport });
  renderDiagnostics(snapshot.last_diagnostics);
  if (snapshot.pending_operation_key && !operationKeyManual && document.activeElement !== $("#operation-key")) setOperationKey(snapshot.pending_operation_key, snapshot);
  if (renderOdrive && snapshot.odrive_usb) renderOdriveResponse(snapshot.odrive_usb);
  updateDraftState();
  updateActionAvailability();
}

function unavailable() {
  const first = !unavailableSince;
  unavailableSince = true;
  snapshotReady = false;
  chartController.unavailable();
  setStatusFact("connection-state", "服务不可达", "error");
  setStatusFact("telemetry-age", "服务不可达；缓存已过期", "error");
  setStatusFact("firmware-state", "主机缓存已过期", "warning");
  setStatusFact("odrive-state", "主机缓存已过期", "warning");
  for (const prefix of ["metric", "live"]) {
    for (const name of ["position", "velocity", "force"]) {
      const detail = $(`#${prefix}-${name}-detail`);
      detail.textContent = "服务不可达；最后缓存不代表当前测量";
      detail.classList.add("is-warning");
    }
  }
  if (first) setNotice("本机 WebUI 服务不可达；未重发任何设备请求。", "is-error");
  updateConnections();
  updateActionAvailability();
}
function command(action, fields = {}) { return run("/api/action", { action, ...fields }, actionLabel(action)); }
function number(form, name) { const raw = String(new FormData(form).get(name) ?? "").trim(); if (!raw) throw new Error(`${name} 不能为空。`); const value = Number(raw); if (!Number.isFinite(value)) throw new Error(`${name} 必须是有限数值。`); return value; }
function populateDevices(select, devices, valueKey, placeholder) {
  const previous = select.value;
  const candidates = devices.filter((device) => device[valueKey]);
  select.replaceChildren(new Option(candidates.length ? placeholder : "未发现设备，请刷新列表", ""));
  for (const device of candidates) {
    const value = device[valueKey];
    const detail = [device.serial && device.serial !== value ? device.serial : "", device.description].filter(Boolean).join(" · ");
    select.add(new Option(value + (detail ? " · " + detail : ""), value));
  }
  if (previous) {
    if (!candidates.some((device) => device[valueKey] === previous)) select.add(new Option(previous + "（本次未枚举到）", previous));
    select.value = previous;
  } else if (candidates.length === 1) select.value = candidates[0][valueKey];
}
async function discoverDevices(transport, announce = true) {
  if (busy || discovering.has(transport) || (transport === "odrive" && odriveBusy)) return;
  const noticeEpoch = noticeRevision;
  discovering.add(transport);
  if (transport === "odrive") setOdriveBusy(true);
  updateActionAvailability();
  const source = transport === "odrive" ? { path:"/api/odrive/devices", select:"#odrive-serial", key:"serial_number", name:"ODrive USB" }
    : transport === "can" ? { path:"/api/can/devices", select:"#can-port", key:"port", name:"CAN 适配器" }
    : { path:"/api/devices", select:"#usb-serial", key:"serial", name:"USB" };
  try {
    const body = await get(source.path, { timeout:transport === "odrive" ? 8000 : 5000 });
    populateDevices($(source.select), body.devices || [], source.key, "请选择" + source.name);
    if (announce && noticeEpoch === noticeRevision) setNotice(source.name + " 列表已刷新；选择候选后明确连接或读取。");
  } catch (error) {
    if (!$(source.select).value) $(source.select).replaceChildren(new Option("枚举失败，请刷新重试", ""));
    if (announce && noticeEpoch === noticeRevision) setNotice(source.name + " 枚举失败：" + error.message, "is-error");
  } finally {
    discovering.delete(transport);
    if (transport === "odrive") setOdriveBusy(false);
    updateActionAvailability();
  }
}
function updateCanNodeOptions() {
  const select = $("#active-can-node"); const control = $("#active-can-node-control"); control.hidden = activeTransport !== "can";
  const nodes = new Map();
  for (const entry of canNodes) if (Number.isInteger(entry.node)) nodes.set(entry.node, entry);
  const connectedNode = sessions.can?.connection?.node ?? snapshot?.connection?.node;
  if (Number.isInteger(connectedNode) && !nodes.has(connectedNode)) nodes.set(connectedNode, { node:connectedNode, snapshot:sessions.can || snapshot });
  if (activeCanNode === null && nodes.size) activeCanNode = [...nodes.keys()][0];
  const options = [...nodes].map(([node, entry]) => [String(node), `节点 ${node}${entry.snapshot?.connected ? "" : "（未就绪）"}`]);
  if (!options.length) options.push(["", "等待已连接节点"]);
  const signature = JSON.stringify(options);
  if (select.dataset.options !== signature) {
    select.replaceChildren(...options.map(([value, label]) => new Option(label, value)));
    select.dataset.options = signature;
  }
  const selected = activeCanNode === null ? "" : String(activeCanNode);
  if (select.value !== selected) select.value = selected;
}
function selectTransport(transport, { force = false, announce = true, node = activeCanNode } = {}) {
  if ((busy && !force) || (transport === activeTransport && !force)) return false;
  const priorKey = viewKey();
  viewStates[priorKey] = {
    record:$("#config-record").value, configDraft, operationKey:$("#operation-key").value,
    operationKeySource, operationKeyManual, keyState:$("#operation-key-state").textContent, localControlSubmission,
    controlMode:$("input[name=control-mode]:checked").value,
    controlValues:$$(".command-form input").map((input) => input.value),
  };
  activeTransport = transport;
  if (transport === "can") activeCanNode = Number.isInteger(node) ? node : null;
  $("#active-transport").value = transport;
  sessionRevision++;
  updateCanNodeOptions();
  const view = viewStates[viewKey()];
  $("#config-record").value = view?.record || "";
  configDraft = view?.configDraft || { dirty:false, source:null, baseline:"" };
  configEditor.setDocument($("#config-record").value, configDraft.source, configDraft.baseline || "");
  $("#operation-key").value = view?.operationKey || "";
  operationKeySource = view?.operationKeySource || null;
  operationKeyManual = view?.operationKeyManual || false;
  $("#operation-key-state").textContent = view?.keyState || "操作键尚未关联本页会话。";
  localControlSubmission = view?.localControlSubmission || null;
  const mode = view?.controlMode || "position";
  $('input[name=control-mode][value="' + mode + '"]').checked = true;
  $$(".command-form input").forEach((input, index) => { input.value = view?.controlValues?.[index] || ""; });
  selectControlMode();
  chartController.selectTransport(viewKey());
  const selectedSession = sessionForTransport(transport);
  update(selectedSession || { connected:false }, true, false);
  if (!snapshotReady) chartController.unavailable();
  updateActionAvailability();
  if (announce) setNotice(`当前操作通路：${transportLabel(transport)}${transport === "can" && activeCanNode !== null ? ` 节点 ${activeCanNode}` : ""}。连接与心跳分别保留；后续请求发送到此通路。`);
  return true;
}
function selectedCanScanNodes() { return $$("#can-scan-results input:checked").map((input) => Number(input.value)); }
function renderCanScanResults(errors = []) {
  const root = $("#can-scan-results"); root.replaceChildren();
  if (!canScanNodes.length && !errors.length) { root.textContent = "未发现可识别的节点。"; return; }
  for (const entry of canScanNodes) {
    const label = document.createElement("label"); const input = document.createElement("input"); input.type = "checkbox"; input.value = entry.node; input.checked = true;
    label.append(input, document.createTextNode(`节点 ${entry.node} · ${entry.identity?.uid || entry.identity?.serial || "已读取 Identity"}`)); root.append(label);
  }
  for (const entry of errors) { const line = document.createElement("p"); line.className = "helper is-warning"; line.textContent = `节点 ${entry.node}：${entry.message}`; root.append(line); }
}
async function scanCanNodes() {
  const port = $("#can-port").value; const profile = $("#can-connection-form select[name=profile]").value;
  const start_node = Number($("#can-scan-start").value); const end_node = Number($("#can-scan-end").value);
  if (!port) return setNotice("请先选择 CAN 适配器路径。", "is-error");
  if (!Number.isInteger(start_node) || !Number.isInteger(end_node) || start_node < 0 || end_node > 127 || start_node > end_node) return setNotice("扫描范围必须是 0 至 127 的递增整数。", "is-error");
  const body = await run("/api/can/scan", { transport:"can", port, profile, start_node, end_node }, "扫描 CAN 节点");
  if (!body) return;
  canScanNodes = (body.nodes || []).filter((entry) => Number.isInteger(entry.node)); renderCanScanResults(body.errors || []);
}
async function connectCanNodes() {
  const port = $("#can-port").value; const profile = $("#can-connection-form select[name=profile]").value; const nodes = selectedCanScanNodes();
  if (!port) return setNotice("请先选择 CAN 适配器路径。", "is-error");
  if (!nodes.length) return setNotice("至少选择一个已扫描节点。", "is-error");
  const body = await run("/api/can/connect", { transport:"can", port, profile, nodes }, "连接所选 CAN 节点");
  if (body?.errors?.length) renderCanScanResults(body.errors);
}
$("#active-transport").addEventListener("change", (event) => selectTransport(event.target.value));
$("#active-can-node").addEventListener("change", (event) => { const node = Number(event.target.value); if (Number.isInteger(node)) selectTransport("can", { force:true, node }); });
for (const transport of ["usb", "can"]) {
  $("#" + transport + "-connection-form").addEventListener("submit", (event) => {
    event.preventDefault();
    const values = new FormData(event.currentTarget);
    if (transport === "usb") {
      const serial = String(values.get("serial") || "");
      if (!serial) return setNotice("请先从列表选择 USB 序列号。", "is-error");
      return run("/api/connect", { transport, serial }, "连接并核对身份");
    }
    const port = String(values.get("port") || "");
    if (!port) return setNotice("请先从列表选择 CAN 适配器路径。", "is-error");
    const rawNode = String(values.get("node") ?? "").trim();
    const node = Number(rawNode);
    if (!rawNode || !Number.isInteger(node) || node < 0 || node > 127) return setNotice("Customer CAN 节点号必须为 0 至 127 的整数。", "is-error");
    run("/api/connect", { transport, port, node, profile:values.get("profile") }, "连接并核对身份");
  });
  $("#disconnect-" + transport).addEventListener("click", () => run("/api/disconnect", { transport }, "关闭本地连接"));
  $("#reconnect-" + transport).addEventListener("click", () => run("/api/reconnect", { transport }, "重新连接"));
  $("#discover-" + transport).addEventListener("click", () => discoverDevices(transport));
}
$("#can-scan-nodes").addEventListener("click", scanCanNodes);
$("#can-connect-nodes").addEventListener("click", connectCanNodes);
$$("button[data-action]").forEach((button) => button.addEventListener("click", () => command(button.dataset.action, button.dataset.view ? { view:button.dataset.view } : {})));
$$(".command-form").forEach((form) => form.addEventListener("submit", (event) => { event.preventDefault(); try { const action = form.dataset.command; const fields = action === "position" ? { mm:number(form, "mm") } : action === "velocity" ? { mm_s:number(form, "mm_s") } : action === "force" ? { n:number(form, "n") } : { equilibrium_mm:number(form, "equilibrium_mm"), stiffness_n_per_mm:number(form, "stiffness_n_per_mm"), damping_ns_per_mm:number(form, "damping_ns_per_mm") }; localControlSubmission = null; facts($("#control-submission"), [], "正在等待本地提交结果。"); command(action, fields); } catch (error) { setNotice(error.message, "is-error"); } }));
$$("[data-stop-control]").forEach((button) => button.addEventListener("click", () => command("stop")));
$("#config-record").addEventListener("input", () => { configDraft.dirty = true; configDraft.warning = null; updateDraftState(); });
$("#validate-config").addEventListener("click", () => command("config_validate", { record:$("#config-record").value }));
$("#save-config").addEventListener("click", () => {
  if (configDraft.source && configDraft.source !== sourceKey() && !window.confirm("此草稿来自另一设备或运行实例。确认将它保存到当前已核对设备吗？")) return;
  command("config_save", { record:$("#config-record").value });
});
$("#restore-factory").addEventListener("click", () => command("restore_factory"));
$("#operation-key").addEventListener("input", () => {
  operationKeyManual = true;
  operationKeySource = null;
  $("#operation-key-state").textContent = "手动输入的操作键；只会在你明确查询或释放时使用。";
  updateActionAvailability();
});
$("#query-result").addEventListener("click", () => command("maintenance_result", { operation_key:$("#operation-key").value.trim() }));
$("#release-result").addEventListener("click", () => command("maintenance_release", { operation_key:$("#operation-key").value.trim() }));
$("#reset-application").addEventListener("click", () => command("reset_application"));
$("#enter-update").addEventListener("click", () => command("enter_update"));

$("#discover-odrive").addEventListener("click", () => discoverDevices("odrive"));
$("#read-odrive").addEventListener("click", async () => {
  if (busy || odriveBusy) return;
  const serial = $("#odrive-serial").value.trim();
  if (!serial) return setNotice("请先从列表选择 ODrive USB 序列号。", "is-error");
  odriveRequestEpoch++;
  setOdriveBusy(true);
  const workingNoticeEpoch = setNotice("正在明确读取 ODrive USB…", "is-working");
  try {
    const body = await post("/api/odrive/read", { serial });
    renderOdriveResponse(body);
    if (workingNoticeEpoch === noticeRevision) setNotice("ODrive USB 快照已读取；它不表示 H723 内部链路状态。");
  } catch (error) {
    const body = error.detail || { ok:false, message:error.message, serial_number:serial };
    renderOdriveResponse(body);
    if (workingNoticeEpoch === noticeRevision) setNotice(`ODrive USB 读取失败：${error.message}`, "is-error");
  } finally {
    odriveRequestEpoch++;
    setOdriveBusy(false);
  }
});

function setTheme(theme) {
  const light = theme === "light";
  document.documentElement.dataset.theme = light ? "light" : "dark";
  localStorage.setItem("eha-theme", light ? "light" : "dark");
  $("#theme-toggle").setAttribute("aria-pressed", String(light));
  const label = light ? "切换至深色主题" : "切换至浅色主题";
  $("#theme-toggle").setAttribute("aria-label", label);
  $("#theme-toggle").title = label;
  $("#theme-icon").setAttribute("href", light ? "#icon-moon" : "#icon-sun");
  chartController.refreshTheme();
}
setTheme(localStorage.getItem("eha-theme") || "dark");
$("#theme-toggle").addEventListener("click", () => setTheme(document.documentElement.dataset.theme === "dark" ? "light" : "dark"));

const mobileNavigation = window.matchMedia("(max-width: 760px)");
function setSidebar(open, restoreFocus = false) {
  open = open && mobileNavigation.matches;
  $("#sidebar").classList.toggle("open", open);
  $("#sidebar").inert = mobileNavigation.matches && !open;
  $("#sidebar-scrim").hidden = !open;
  $("#menu-button").setAttribute("aria-expanded", String(open));
  $(".workspace").inert = open;
  if (open) $("#sidebar-close").focus();
  else if (restoreFocus) $("#menu-button").focus();
}
$("#menu-button").addEventListener("click", () => setSidebar(true));
$("#sidebar-close").addEventListener("click", () => setSidebar(false, true));
$("#sidebar-scrim").addEventListener("click", () => setSidebar(false, true));
document.addEventListener("keydown", (event) => {
  if (!$("#sidebar").classList.contains("open")) return;
  if (event.key === "Escape") {
    event.preventDefault();
    setSidebar(false, true);
  } else if (event.key === "Tab") {
    const controls = $$("#sidebar a[href], #sidebar button:not(:disabled)");
    const first = controls[0];
    const last = controls.at(-1);
    if (event.shiftKey && document.activeElement === first) {
      event.preventDefault();
      last.focus();
    } else if (!event.shiftKey && document.activeElement === last) {
      event.preventDefault();
      first.focus();
    }
  }
});
mobileNavigation.addEventListener("change", () => {
  const focusInSidebar = $("#sidebar").contains(document.activeElement);
  setSidebar(false, mobileNavigation.matches && focusInSidebar);
  if (focusInSidebar && !mobileNavigation.matches) $(".nav-list [aria-current=page]").focus();
});
setSidebar(false);

const pages = $$("[data-page]");
const routeAliases = { connection: "overview", firmware: "diagnostics", driver: "diagnostics", "odrive-usb": "diagnostics", "telemetry-panel": "telemetry", control: "telemetry" };
function syncRoute(focus = false) {
  const requested = location.hash.slice(1);
  const candidate = routeAliases[requested] || requested;
  const route = pages.some((page) => page.dataset.page === candidate) ? candidate : "overview";
  if (requested !== route) history.replaceState(null, "", `#${route}`);
  for (const page of pages) page.hidden = page.dataset.page !== route;
  for (const link of $$(".nav-list [data-route]")) {
    const active = link.dataset.route === route;
    link.classList.toggle("active", active);
    if (active) {
      link.setAttribute("aria-current", "page");
      $("#page-title").textContent = link.textContent.trim();
    } else link.removeAttribute("aria-current");
  }
  document.title = `${$("#page-title").textContent} · EHA Control Center`;
  setSidebar(false);
  if (focus) {
    $("#main-content").focus({ preventScroll: true });
    window.scrollTo({ top: 0, behavior: "instant" });
  }
  chartController.render();
}
window.addEventListener("hashchange", () => syncRoute(true));
$(".skip-link").addEventListener("click", (event) => {
  event.preventDefault();
  $("#main-content").focus();
});
$$(".nav-list [data-route]").forEach((link) => link.addEventListener("click", () => {
  if (location.hash === link.hash) syncRoute(true);
}));
syncRoute();

function selectControlMode() {
  const mode = $("input[name=control-mode]:checked").value;
  for (const form of $$(".command-form")) form.hidden = form.dataset.command !== mode;
}
$$('input[name="control-mode"]').forEach((input) => input.addEventListener("change", selectControlMode));
selectControlMode();

async function refresh() {
  if (busy) return;
  const revision = sessionRevision;
  const odriveEpoch = odriveRequestEpoch;
  const node = activeTransport === "can" && Number.isInteger(activeCanNode) ? `&node=${encodeURIComponent(activeCanNode)}` : "";
  const query = `?transport=${activeTransport}${node}${chartController.cursor === null ? "" : `&after=${encodeURIComponent(chartController.cursor)}`}`;
  try {
    const body = await get(`/api/snapshot${query}`, { timeout:5000 });
    if (revision === sessionRevision) applyResponse(body, odriveEpoch);
  } catch (_) { if (revision === sessionRevision) unavailable(); }
}
async function poll() {
  await refresh();
  window.setTimeout(poll, 250);
}
updateActionAvailability();
$(".app-shell").inert = false;
void configEditor.loadSchema();
void recordingUi.refresh();
poll();
void discoverDevices("usb", false);
void discoverDevices("can", false);
