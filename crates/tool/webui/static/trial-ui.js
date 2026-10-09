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
  let positionTimer; let activeScope = "single"; let lastSnapshot; let lastGroupNodes = []; let lastSessions = {};
  $("#trial-start").disabled = true;
  $("#trial-stop").disabled = true;
  $("#group-heartbeat-start").disabled = true;
  $("#group-heartbeat-stop").disabled = true;
  const message = (text, state = "") => { const target = $("#trial-message"); target.textContent = text; target.className = `helper ${state}`; };
  const selectedNodes = () => [...document.querySelectorAll("#trial-members input:checked")].map((input) => Number(input.value));
  const envelope = () => Object.fromEntries([...document.querySelectorAll(".trial-envelope input")].map((input) => [input.name, number(input.value, input.previousElementSibling?.textContent || input.name)]));
  const command = () => {
    const mode = $("input[name=trial-mode]:checked").value; const form = $(`.trial-command[data-trial-command=${mode}]`);
    const fields = { position:["mm"], velocity:["mm_s"], force:["n"], impedance:["equilibrium_mm", "stiffness_n_per_mm", "damping_ns_per_mm"] }[mode];
    const values = Object.fromEntries(fields.map((key) => [key, number(new FormData(form).get(key), key)]));
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
    if (state.groupScope) activeScope = "group";
    else if (state.live) activeScope = "single";
    $$("#page-trial input").forEach((input) => { input.disabled = state.live && !(input.id === "trial-position-slider" && activeScope === "single" && snapshot?.trial?.state === "active"); });
    $("#trial-start").disabled = !snapshot?.connected || state.live || !$("#trial-arm").checked;
    $("#trial-stop").disabled = !state.stopAvailable;
    $("#group-heartbeat-start").disabled = state.live;
    $("#group-heartbeat-stop").disabled = state.live;
    $("#trial-state").textContent = state.terminalUnknown ? "结果未知（已中断）" : state.stopping ? "停止中" : state.live ? "运行中" : "未运行";
    $("#trial-state").className = `status-label ${state.terminalUnknown ? "is-warning" : state.live ? "is-success" : "is-idle"}`;
    message(trialStatusText(snapshot?.trial), state.terminalUnknown ? "is-warning" : "");
  };
  const renderNodes = () => {
    const root = $("#trial-members"); const priorBoxes = [...root.querySelectorAll("input")];
    if (priorBoxes.length) rememberedNodes = new Set(priorBoxes.filter((input) => input.checked).map((input) => Number(input.value)));
    root.replaceChildren();
    if (!canNodes.length) { root.textContent = "连接 CAN 节点后可选择群组成员。"; return; }
    for (const entry of canNodes) {
      const label = document.createElement("label"); const box = document.createElement("input"); box.type = "checkbox"; box.value = entry.node; box.checked = rememberedNodes.has(entry.node); box.disabled = !entry.snapshot?.connected;
      box.addEventListener("change", () => { rememberedNodes = new Set([...document.querySelectorAll("#trial-members input:checked")].map((input) => Number(input.value))); });
      label.append(box, document.createTextNode(`节点 ${entry.node} · ${entry.snapshot?.identity?.uid || "等待 Identity"} · ${trialStatusText(entry.trial || entry.snapshot?.trial)}`)); root.append(label);
    }
  };
  const send = async (action) => {
    if (action === "stop") {
      if (positionTimer) { window.clearTimeout(positionTimer); positionTimer = undefined; }
      try { if (activeScope === "group") await run("/api/group", { action:"stop" }, "停止群组试验"); else await run("/api/trial", { transport:activeTransport(), action:"stop" }, "停止本次试验"); }
      catch (error) { message(error.message, "is-error"); } return;
    }
    if (!$("#trial-arm").checked) return message("请先确认机械空间、外部断能和本次范围。", "is-warning");
    try {
      activeScope = scope();
      const payload = { action, trial:trial() };
      if (scope() === "group") { const nodes = selectedNodes(); if (nodes.length < 2) throw new Error("群组至少选择两台已连接 CAN EHA。"); await run("/api/group", { nodes, action:"trial_start", trial:payload.trial }, "群组预检并开始试验"); }
      else await run("/api/trial", { transport:activeTransport(), action, trial:payload.trial }, action === "start" ? "预检并开始试验" : "停止本次试验");
    } catch (error) { message(error.message, "is-error"); }
  };
  $("#trial-start").addEventListener("click", () => send("start"));
  $("#trial-stop").addEventListener("click", () => send("stop"));
  for (const [id, action, label] of [["#group-heartbeat-start", "heartbeat_start", "启用群组心跳"], ["#group-heartbeat-stop", "heartbeat_stop", "停用群组心跳"]]) $(id).addEventListener("click", async () => {
    const nodes = selectedNodes();
    if (nodes.length < 2) return message("群组心跳至少选择两台已连接 CAN EHA。", "is-warning");
    await run("/api/group", { nodes, action }, label);
  });
  $("#trial-arm").addEventListener("change", () => updateButtons());
  $$('input[name="trial-mode"]').forEach((input) => input.addEventListener("change", () => { const mode = $("input[name=trial-mode]:checked").value; document.querySelectorAll(".trial-command").forEach((form) => { form.hidden = form.dataset.trialCommand !== mode; }); }));
  const slider = $("#trial-position-slider");
  slider.addEventListener("input", () => {
    const min = number(document.querySelector('[name="position_min_mm"]').value, "位置下限", { optional:true }); const max = number(document.querySelector('[name="position_max_mm"]').value, "位置上限", { optional:true });
    if (min === null || max === null || min >= max) return;
    $("#trial-position input[name=mm]").value = (min + (max - min) * Number(slider.value) / 1000).toFixed(3);
    if (scope() !== "single" || window.__ehaSnapshot?.trial?.state !== "active") return;
    const submitLatest = () => { positionTimer = undefined; lastPositionTargetAt = performance.now(); run("/api/trial", { transport:activeTransport(), action:"target", mm:number($("#trial-position input[name=mm]").value, "目标位置") }, "更新连续位置目标"); };
    const delay = Math.max(0, 50 - (performance.now() - lastPositionTargetAt));
    if (!positionTimer && delay === 0) submitLatest();
    else if (!positionTimer) positionTimer = window.setTimeout(submitLatest, delay);
  });
  return { update(snapshot, nodes = [], groupNodes = [], sessions = {}) { canNodes = nodes; lastSnapshot = snapshot; lastGroupNodes = groupNodes; lastSessions = sessions; window.__ehaSnapshot = snapshot; renderNodes(); updateButtons(); }, message };
}
const $$ = (selector) => [...document.querySelectorAll(selector)];
