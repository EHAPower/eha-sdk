// Copyright The eha-sdk Contributors

const count = (value) => typeof value === "number" && Number.isFinite(value) && value >= 0 ? value.toLocaleString("zh-CN") : "未记录";

function timestamp(value) {
  const parsed = Number(value);
  return Number.isSafeInteger(parsed) && parsed > 0 ? parsed : null;
}

function runStartTimestamp(entry, metadata) {
  const recorded = timestamp(metadata?.started_unix_time_ms ?? entry?.started_unix_time_ms);
  if (recorded) return recorded;
  const match = /^run-(\d+)-/.exec(entry?.id || "");
  return timestamp(match?.[1]);
}

function connectionSummary(connection) {
  if (connection?.transport === "can") return `CAN 节点 ${connection.node ?? "未记录"}`;
  if (connection?.transport === "usb") return "USB";
  return "通路未记录";
}

export function recordingSummary(entry, formatTime = (value) => new Intl.DateTimeFormat("zh-CN", { dateStyle:"short", timeStyle:"medium" }).format(new Date(value))) {
  const metadata = entry?.metadata || {};
  const completion = metadata.completion;
  const started = runStartTimestamp(entry, metadata);
  const title = `${started ? formatTime(started) : "启动时间未记录"} · ${connectionSummary(metadata.connection)}`;
  const uid = metadata.device_uid || metadata.identity?.uid || "未记录";
  if (!completion || typeof completion !== "object") return { title, id:entry?.id || "未命名运行", detail:`设备 ${uid} · 未完成（未写入 completion）` };
  const ended = timestamp(completion.ended_unix_time_ms);
  const state = completion.active ? "结束状态异常" : "已完成";
  const ending = ended ? `，结束 ${formatTime(ended)}` : "";
  return { title, id:entry?.id || "未命名运行", detail:`设备 ${uid} · ${state}：事件 ${count(completion.events)}，遥测 ${count(completion.telemetry)}，丢弃 ${count(completion.dropped)}${ending}` };
}

export function createRecordingUi({ get, run, activeTransport }) {
  const root = document.querySelector("#recording-list"); const state = document.querySelector("#recording-state");
  const start = document.querySelector("#recording-start"); const stop = document.querySelector("#recording-stop");
  start.disabled = true; stop.disabled = true;
  async function refresh() {
    try {
      const body = await get("/api/recordings"); const runs = body.runs || [];
      root.replaceChildren();
      if (!runs.length) { root.textContent = "尚无由本机服务完成的运行记录。"; return; }
      for (const runEntry of runs) {
        const item = document.createElement("article"); item.className = "recording-item";
        const summary = recordingSummary(runEntry);
        const title = document.createElement("strong"); title.textContent = summary.title; item.append(title);
        const detail = document.createElement("small"); detail.textContent = summary.detail; item.append(detail);
        const identifier = document.createElement("small"); identifier.textContent = `运行 ID：${summary.id}`; item.append(identifier);
        const links = document.createElement("div"); links.className = "action-row";
        for (const name of runEntry.files || ["metadata.json", "events.jsonl", "telemetry.csv"]) { const link = document.createElement("a"); link.className = "button"; link.href = `/api/recordings/${encodeURIComponent(runEntry.id)}/${encodeURIComponent(name)}`; link.download = name; link.textContent = name; links.append(link); }
        item.append(links); root.append(item);
      }
    } catch (error) { root.textContent = `读取记录失败：${error.message}`; }
  }
  start.addEventListener("click", async () => { await run("/api/recording", { transport:activeTransport(), action:"start" }, "开始运行记录"); await refresh(); });
  stop.addEventListener("click", async () => { await run("/api/recording", { transport:activeTransport(), action:"stop" }, "停止运行记录"); await refresh(); });
  document.querySelector("#recording-refresh").addEventListener("click", refresh);
  return { update(snapshot, trialLive = false) {
    const recording = snapshot?.recording;
    start.disabled = !snapshot?.connected || Boolean(recording?.active) || Boolean(recording?.failed) || trialLive;
    stop.disabled = !snapshot?.connected || !recording?.active;
    if (!recording) { state.textContent = "等待会话状态。"; return; }
    if (recording.active) {
      state.textContent = `正在记录：${recording.id || recording.directory || "当前运行"}；事件 ${recording.events ?? 0}，遥测 ${recording.telemetry ?? 0}${recording.dropped ? `，丢弃 ${recording.dropped}` : ""}。`;
      return;
    }
    state.textContent = recording.failed ? `记录异常结束：${recording.error || "请查看部分记录"}` : recording.error ? `记录未开始或已异常结束：${recording.error}` : recording.ended_unix_time_ms ? `记录已结束：${recording.id || recording.directory || "可在下方下载"}${recording.dropped ? `；丢弃 ${recording.dropped}` : ""}。` : "当前没有后端运行记录。";
  }, refresh };
}
