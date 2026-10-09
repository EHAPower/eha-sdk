// Copyright The eha-sdk Contributors

import { trialState, trialStatusText } from "./trial-state.js";

const $ = (selector) => document.querySelector(selector);
const number = (value, label, { optional = false } = {}) => {
  const raw = String(value ?? "").trim();
  if (!raw && optional) return null;
  const parsed = Number(raw); if (!raw || !Number.isFinite(parsed)) throw new Error(`${label} 必须是有限数值。`); return parsed;
};

export function createTrialUi({ run, activeTransport }) {
  let canNodes = []; let rememberedNodes = new Set(); let lastPositionTargetAt = 0;
  let positionTimer; let activeScope = "single"; let lastSnapshot; let lastGroupNodes = []; let lastSessions = {}; let localMessage;
  $("#trial-start").disabled = true;
  $("#trial-stop").disabled = true;
  $("#group-heartbeat-start").disabled = true;
  $("#group-heartbeat-stop").disabled = true;
  const message = (text, state = "") => { const target = $("#trial-message"); target.textContent = text; target.className = `helper ${state}`; };
  const clearLocalMessage = () => { localMessage = undefined; };
  const showLocalMessage = (text, state = "") => { localMessage = { text, state }; message(text, state); };
  const fieldLabel = (name) => ({
    position_min_mm:"位置下限", position_max_mm:"位置上限", velocity_abs_max_mm_s:"速度绝对值上限",
    force_abs_max_n:"力绝对值上限", stiffness_max_n_per_mm:"刚度上限", damping_max_ns_per_mm:"阻尼上限", duration_max_s:"最长时长",
  })[name] || name;
  const selectedNodes = () => [...document.querySelectorAll("#trial-members input:checked:not(:disabled)")].map((input) => Number(input.value));
  const envelope = () => Object.fromEntries([...document.querySelectorAll(".trial-envelope input")].map((input) => [input.name, number(input.value, fieldLabel(input.name))]));
  const command = () => {
    const mode = $("input[name=trial-mode]:checked").value; const form = $(`.trial-command[data-trial-command=${mode}]`);
    const fields = { position:["mm"], velocity:["mm_s"], force:["n"], impedance:["equilibrium_mm", "stiffness_n_per_mm", "damping_ns_per_mm"] }[mode];
    const labels = { mm:"目标位置", mm_s:"目标速度", n:"目标力", equilibrium_mm:"平衡位置", stiffness_n_per_mm:"刚度", damping_ns_per_mm:"阻尼" };
    const values = Object.fromEntries(fields.map((key) => [key, number(new FormData(form).get(key), labels[key])]));
    return { action:mode, ...values };
  };
  const trial = () => {
    const active = $("input[name=trial-mode]:checked").value;
    const duration = number($("#trial-duration").value, "有限运行时间", { optional:true });
    const position = $("#trial-position");
    const tolerance = active === "position" ? number(new FormData(position).get("tolerance_mm"), "到位容差", { optional:true }) : null;
    const settle = active === "position" ? number(new FormData(position).get("settle_ms"), "到位稳定时间", { optional:true }) : null;
    return { command:command(), envelope:envelope(), duration_s:duration, reach:tolerance === null ? null : { tolerance_mm:tolerance, settle_ms:settle ?? 0 } };
  };
  const scope = () => $("input[name=trial-scope]:checked").value;
  const updateButtons = (snapshot = lastSnapshot, groupNodes = lastGroupNodes) => {
    const state = trialState(snapshot, canNodes, groupNodes, lastSessions);
    const groupMode = scope() === "group";
    const selectedGroupNodes = selectedNodes();
    const groupReady = selectedGroupNodes.length >= 2;
    const scopeConnected = groupMode ? groupReady : Boolean(snapshot?.connected);
    if (state.groupScope) activeScope = "group";
    else if (state.live) activeScope = "single";
    $$("#page-trial input").filter((input) => !input.closest("#trial-members")).forEach((input) => { input.disabled = state.live && !(input.id === "trial-position-slider" && activeScope === "single" && snapshot?.trial?.state === "active"); });
    $$("#trial-members input").forEach((input) => {
      const node = canNodes.find((entry) => entry.node === Number(input.value));
      input.disabled = state.live || !node?.snapshot?.connected || Boolean(node.snapshot.transport?.disconnected);
    });
    $("#trial-start").disabled = !scopeConnected || state.live;
    $("#trial-stop").disabled = !state.stopAvailable;
    $("#group-heartbeat-start").disabled = state.live || !groupReady;
    $("#group-heartbeat-stop").disabled = state.live || !groupReady;
    $("#trial-state").textContent = state.terminalUnknown ? "结果未知（已中断）" : state.stopping ? "停止中" : state.live ? "运行中" : "未运行";
    $("#trial-state").className = `status-label ${state.terminalUnknown ? "is-warning" : state.live ? "is-success" : "is-idle"}`;
    if (localMessage) message(localMessage.text, localMessage.state);
    else message(groupMode && !groupReady && !state.live ? "已选 CAN 群组至少需要两台已连接 EHA。" : trialStatusText(snapshot?.trial), groupMode && !groupReady && !state.live ? "is-warning" : state.terminalUnknown ? "is-warning" : "");
  };
  const renderNodes = () => {
    const root = $("#trial-members"); const priorBoxes = [...root.querySelectorAll("input")];
    if (priorBoxes.length) rememberedNodes = new Set(priorBoxes.filter((input) => input.checked).map((input) => Number(input.value)));
    if (!canNodes.length) { root.replaceChildren("连接 CAN 节点后可选择群组成员。"); return; }
    [...root.childNodes].filter((node) => node.nodeType !== Node.ELEMENT_NODE || !node.matches("label[data-node]")).forEach((node) => node.remove());
    const available = new Set(canNodes.map((entry) => entry.node));
    [...root.querySelectorAll("label[data-node]")].filter((label) => !available.has(Number(label.dataset.node))).forEach((label) => label.remove());
    for (const entry of canNodes) {
      let label = [...root.querySelectorAll("label[data-node]")].find((candidate) => Number(candidate.dataset.node) === entry.node);
      let box = label?.querySelector("input");
      if (!label) {
        label = document.createElement("label"); label.dataset.node = entry.node;
        box = document.createElement("input"); box.type = "checkbox"; box.value = entry.node;
        box.addEventListener("change", () => { rememberedNodes = new Set([...document.querySelectorAll("#trial-members input:checked")].map((input) => Number(input.value))); updateButtons(); });
        const text = document.createElement("span"); text.dataset.memberLabel = "";
        label.append(box, text); root.append(label);
      }
      const checked = rememberedNodes.has(entry.node); const disabled = !entry.snapshot?.connected || Boolean(entry.snapshot.transport?.disconnected);
      if (box.checked !== checked) box.checked = checked;
      if (box.disabled !== disabled) box.disabled = disabled;
      const memberText = `节点 ${entry.node} · ${entry.snapshot?.identity?.uid || "等待 Identity"} · ${trialStatusText(entry.trial || entry.snapshot?.trial)}`;
      const text = label.querySelector("[data-member-label]");
      if (text.textContent !== memberText) text.textContent = memberText;
    }
  };
  const send = async (action) => {
    if (action === "stop") {
      if (positionTimer) { window.clearTimeout(positionTimer); positionTimer = undefined; }
      try { if (activeScope === "group") await run("/api/group", { action:"stop" }, "停止群组试验"); else await run("/api/trial", { transport:activeTransport(), action:"stop" }, "停止本次试验"); }
      catch (error) { showLocalMessage(error.message, "is-error"); } return;
    }
    clearLocalMessage();
    try {
      activeScope = scope();
      const payload = { action, trial:trial() };
      if (scope() === "group") { const nodes = selectedNodes(); if (nodes.length < 2) throw new Error("群组至少选择两台已连接 CAN EHA。"); await run("/api/group", { nodes, action:"trial_start", trial:payload.trial }, "群组预检并开始试验"); }
      else await run("/api/trial", { transport:activeTransport(), action, trial:payload.trial }, action === "start" ? "预检并开始试验" : "停止本次试验");
    } catch (error) { showLocalMessage(error.message, "is-error"); }
  };
  $("#trial-start").addEventListener("click", () => send("start"));
  $("#trial-stop").addEventListener("click", () => send("stop"));
  for (const [id, action, label] of [["#group-heartbeat-start", "heartbeat_start", "启用群组心跳"], ["#group-heartbeat-stop", "heartbeat_stop", "停用群组心跳"]]) $(id).addEventListener("click", async () => {
    const nodes = selectedNodes();
    clearLocalMessage();
    if (nodes.length < 2) return showLocalMessage("群组心跳至少选择两台已连接 CAN EHA。", "is-warning");
    await run("/api/group", { nodes, action }, label);
  });
  $$("input[name=trial-scope]").forEach((input) => input.addEventListener("change", () => updateButtons()));
  $("#page-trial").addEventListener("input", clearLocalMessage, true);
  $$('input[name="trial-mode"]').forEach((input) => input.addEventListener("change", () => { const mode = $("input[name=trial-mode]:checked").value; document.querySelectorAll(".trial-command").forEach((form) => { form.hidden = form.dataset.trialCommand !== mode; }); }));
  $$(".trial-command").forEach((form) => form.addEventListener("submit", (event) => event.preventDefault()));
  const slider = $("#trial-position-slider");
  slider.addEventListener("input", () => {
    let min; let max;
    try {
      min = number(document.querySelector('[name="position_min_mm"]').value, "位置下限", { optional:true });
      max = number(document.querySelector('[name="position_max_mm"]').value, "位置上限", { optional:true });
    } catch (error) { showLocalMessage(error.message, "is-error"); return; }
    if (min === null || max === null || min >= max) return;
    $("#trial-position input[name=mm]").value = (min + (max - min) * Number(slider.value) / 1000).toFixed(3);
    if (scope() !== "single" || window.__ehaSnapshot?.trial?.state !== "active") return;
    const submitLatest = () => {
      positionTimer = undefined; lastPositionTargetAt = performance.now();
      try { run("/api/trial", { transport:activeTransport(), action:"target", mm:number($("#trial-position input[name=mm]").value, "目标位置") }, "更新连续位置目标"); }
      catch (error) { showLocalMessage(error.message, "is-error"); }
    };
    const delay = Math.max(0, 50 - (performance.now() - lastPositionTargetAt));
    if (!positionTimer && delay === 0) submitLatest();
    else if (!positionTimer) positionTimer = window.setTimeout(submitLatest, delay);
  });
  return { update(snapshot, nodes = [], groupNodes = [], sessions = {}) { canNodes = nodes; lastSnapshot = snapshot; lastGroupNodes = groupNodes; lastSessions = sessions; window.__ehaSnapshot = snapshot; renderNodes(); updateButtons(); }, message };
}
const $$ = (selector) => [...document.querySelectorAll(selector)];
