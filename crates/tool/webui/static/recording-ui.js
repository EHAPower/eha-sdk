// Copyright The eha-sdk Contributors

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
        const title = document.createElement("strong"); title.textContent = runEntry.id || "未命名运行"; item.append(title);
        const detail = document.createElement("small"); detail.textContent = runEntry.metadata?.started_at || runEntry.metadata?.start_time || "记录元数据可下载查看。"; item.append(detail);
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
