// Copyright The eha-sdk Contributors

const CHANNELS = [
  { id: "position", name: "位置", unit: "mm", color: "--primary" },
  { id: "velocity", name: "速度", unit: "mm/s", color: "--accent" },
  { id: "force", name: "主要力", unit: "N", color: "--warning" },
];
const MAX_POINTS = 3000;
const RETAIN_US = 30_000_000;

// 只根据当前新鲜遥测提示入口联系，不把本地心跳调度当作固件已经确认联系有效。
export function selectedContactGuidance(transport, identity, telemetry, heartbeat = {}) {
  const label = transport === "can" ? "CAN" : "USB";
  if (heartbeat.error) {
    return { title:"心跳调度错误", detail:"心跳调度报告错误，查看原始错误和入口新遥测。", link:"查看心跳与控制" };
  }
  const maximumAgeMs = identity?.host_contact_max_age_ms;
  if (!Number.isFinite(maximumAgeMs) || maximumAgeMs <= 0) {
    return { title:"等待联系条件", detail:`等待当前运行实例的 Identity 与新遥测，再确认 ${label} 联系有效后提交目标。`, link:"查看遥测与控制" };
  }
  const age = telemetry?.received_age_ms;
  if (!Number.isFinite(age) || age < 0 || age > maximumAgeMs) {
    return heartbeat.enabled
      ? { title:"等待入口联系", detail:`心跳已调度；等待当前运行实例的新遥测，再确认 ${label} 联系有效后提交目标。`, link:"查看遥测与控制" }
      : { title:"等待当前遥测", detail:`等待当前运行实例的新遥测，再判断 ${label} 入口联系与控制条件。`, link:"查看遥测与控制" };
  }
  if (telemetry[`${transport}_contact`] === 1) {
    const heartbeatAgeUs = telemetry[`${transport}_heartbeat_age_us`];
    if (!Number.isFinite(heartbeatAgeUs) || heartbeatAgeUs < 0) {
      return { title:"等待入口联系", detail:`当前 ${label} 联系曾报告有效，但缺少可用的心跳年龄；等待新遥测后再提交目标。`, link:"查看遥测与控制" };
    }
    if (age + heartbeatAgeUs / 1000 > maximumAgeMs) {
      return { title:"等待入口联系", detail:`上次反馈已不足确认当前 ${label} 联系；等待新遥测后再提交目标。`, link:"查看遥测与控制" };
    }
    return null;
  }
  return heartbeat.enabled
    ? { title:"等待入口联系", detail:`心跳已调度；等待固件报告 ${label} 联系有效后再提交目标。`, link:"查看心跳与控制" }
    : { title:"启用入口心跳", detail:`当前 ${label} 联系未建立或已过期。先启用 ${label} 心跳，等待固件报告联系有效后再提交目标。`, link:"前往心跳与控制" };
}

function plotValue(value) {
  return value?.result === 1 && value.quality === 1 && value.stale === false
    && typeof value.value === "number" && Number.isFinite(value.value) ? value.value : null;
}

// 这里只消费主机接收历史。HTTP 刷新时间和序列号都不能代替设备采样时间。
export class TelemetryHistory {
  points = [];
  cursor = null;
  source = null;
  dropped = 0;
  truncated = false;
  revision = 0;

  ingest(trend) {
    if (!trend || !Array.isArray(trend.points)) return false;
    const source = JSON.stringify(trend.source);
    const reset = source !== this.source || trend.reset;
    if (reset) {
      this.points = [];
      this.cursor = null;
      this.source = source;
    } else if (this.cursor !== null && trend.cursor < this.cursor) return false;
    let changed = reset;
    for (const point of trend.points) {
      if (!Number.isSafeInteger(point.cursor) || !Number.isSafeInteger(point.time_us) || point.time_us < 0) continue;
      if (this.cursor !== null && point.cursor <= this.cursor) continue;
      this.cursor = point.cursor;
      if (this.points.length && point.time_us <= this.points.at(-1).time_us) continue;
      this.points.push(point);
      changed = true;
    }
    if (Number.isSafeInteger(trend.cursor)) this.cursor = Math.max(this.cursor ?? 0, trend.cursor);
    const cutoff = (this.points.at(-1)?.time_us ?? 0) - RETAIN_US;
    let discard = Math.max(0, this.points.length - MAX_POINTS);
    while (discard < this.points.length && this.points[discard].time_us < cutoff) discard++;
    if (discard) this.points.splice(0, discard);
    this.dropped = trend.dropped ?? 0;
    this.truncated = Boolean(trend.truncated);
    if (changed) this.revision++;
    return changed;
  }

  clear() {
    this.points = [];
    // 保留游标，否则下一轮会重新拉回刚清掉的历史。
    this.revision++;
  }
}

export function seriesWindow(points, seconds, hz) {
  const last = points.at(-1);
  const max = last ? last.time_us / 1e6 : seconds;
  const min = Math.max(0, max - seconds);
  const visible = points.filter((point) => point.time_us / 1e6 >= min);
  const gapUs = Number.isFinite(hz) && hz > 0 ? 2.5e6 / hz : 1_000_000;
  let gaps = 0;
  const series = CHANNELS.map(({ id }) => ({ id, data: [] }));
  let previous;
  for (const point of visible) {
    const x = point.time_us / 1e6;
    if (previous && (point.gap || point.time_us - previous.time_us > gapUs)) {
      // 仅插入缺失标记，不插值、不伪造量测。使用间隙中点避免吞掉其两端实测值。
      const gapX = (point.time_us + previous.time_us) / 2e6;
      for (const entry of series) entry.data.push([gapX, null]);
      gaps++;
    }
    for (const [index, channel] of CHANNELS.entries()) series[index].data.push([x, plotValue(point[channel.id])]);
    previous = point;
  }
  return { series, min, max: Math.max(max, min + 0.1), count: visible.length, gaps };
}

const formatNumber = (value) => Number(value).toLocaleString("zh-CN", { maximumFractionDigits: 3 });

export function createTelemetryChart({ container, summary, empty, pauseButton, clearButton, windowSelect }) {
  const views = { usb:{ history:new TelemetryHistory(), paused:null }, can:{ history:new TelemetryHistory(), paused:null } };
  let transport = "usb";
  let history = views.usb.history;
  let chart;
  let paused = null;
  let snapshot;
  let serviceAvailable = null;
  let themeChanged = true;
  let displayRevision = 0;
  let renderedRevision = -1;
  let width = 0;
  let height = 0;
  let frame;

  function schedule() {
    if (frame) return;
    frame = requestAnimationFrame(() => { frame = null; render(); });
  }

  function layout() {
    const style = getComputedStyle(document.documentElement);
    const text = style.getPropertyValue("--text-2").trim();
    const border = style.getPropertyValue("--border").trim();
    // 顶部标题、两组间距及底部时间刻度/轴名均须留出空间。
    const rowHeight = (height - 156) / 3;
    const tops = CHANNELS.map((_, index) => 30 + index * (rowHeight + 40));
    return {
      animation: false,
      textStyle: { color: text, fontFamily: "sans-serif" },
      title: CHANNELS.map((channel, index) => ({ text: `${channel.name} (${channel.unit})`, left: 12, top: tops[index] - 25, textStyle: { color: text, fontSize: 12, fontWeight: "normal" } })),
      grid: tops.map((top) => ({ left: 66, right: 24, top, height: rowHeight })),
      tooltip: {
        trigger: "axis", renderMode: "richText", confine: true, transitionDuration: 0,
        formatter: (params) => {
          if (!params.length) return "";
          const lines = [`运行后 ${formatNumber(params[0].value[0])} s`];
          for (const item of params) {
            const channel = CHANNELS.find((entry) => entry.id === item.seriesId);
            lines.push(`${channel.name}：${item.value[1] === null ? "无有效量测" : `${formatNumber(item.value[1])} ${channel.unit}`}`);
          }
          return lines.join("\n");
        },
      },
      axisPointer: { link: [{ xAxisIndex: "all" }] },
      xAxis: CHANNELS.map((channel, index) => ({
        id: channel.id, gridIndex: index, type: "value", boundaryGap: false,
        name: index === 2 ? "运行后 (s)" : "", nameLocation: "middle", nameGap: 25,
        axisLabel: { show: index === 2, color: text, hideOverlap: true, formatter: formatNumber },
        axisLine: { lineStyle: { color: border } }, axisTick: { show: false },
        splitLine: { show: false }, axisPointer: { label: { show: false } },
      })),
      yAxis: CHANNELS.map((channel, index) => ({
        id: channel.id, gridIndex: index, type: "value", scale: true, splitNumber: 2,
        axisLabel: { color: text, hideOverlap: true, formatter: formatNumber },
        splitLine: { lineStyle: { color: border } },
      })),
      series: CHANNELS.map((channel, index) => ({
        id: channel.id, name: channel.name, type: "line", xAxisIndex: index, yAxisIndex: index,
        showSymbol: true, symbol: "circle", symbolSize: 3, connectNulls: false, smooth: false,
        itemStyle: { color: style.getPropertyValue(channel.color).trim() }, lineStyle: { width: 1.5 },
        emphasis: { disabled: true },
      })),
    };
  }

  function render() {
    const points = paused ?? history.points;
    const view = seriesWindow(points, Number(windowSelect.value), snapshot?.identity?.telemetry_hz);
    const hasValue = view.series.some((series) => series.data.some(([, value]) => value !== null));
    const connected = snapshot?.connected && !snapshot.transport?.disconnected;
    const stale = !connected || (snapshot?.telemetry?.received_age_ms ?? Infinity) > 1000;
    const state = serviceAvailable === null ? "等待会话状态" : !serviceAvailable ? "本机服务不可达，保留已接收历史" : paused ? "绘图已暂停，后台仍接收" : !connected ? "连接未就绪，保留已接收历史" : stale ? "等待新遥测，保留已接收历史" : "实时趋势";
    summary.textContent = `${state} · ${view.count} 个样本 · 最近 ${windowSelect.value} s；横轴为设备启动后的秒数。${view.gaps ? ` ${view.gaps} 处接收间隙已断线。` : ""}${history.dropped ? ` 主机接收缓存曾溢出 ${history.dropped} 帧。` : ""}${history.truncated ? " 较早历史已超出保留窗口。" : ""}`;
    pauseButton.textContent = paused ? "恢复绘图" : "暂停绘图";
    pauseButton.setAttribute("aria-pressed", String(paused !== null));
    clearButton.disabled = points.length === 0;
    empty.hidden = hasValue;
    empty.textContent = !window.echarts ? "图表资源未加载，请刷新页面。" : view.count ? "当前窗口没有合格的新鲜量测；旧值和不可用值保留为断线。" : serviceAvailable === null ? "正在取得本机会话状态…" : !serviceAvailable ? "等待本机服务恢复后继续接收遥测。" : paused ? "绘图已暂停。恢复绘图后查看新数据。" : !connected ? "连接设备后显示遥测趋势。" : "等待接收遥测；无需为绘图启用心跳或提交目标。";
    if (!window.echarts || !container.clientWidth || !container.clientHeight) return;
    const resized = width !== container.clientWidth || height !== container.clientHeight;
    width = container.clientWidth;
    height = container.clientHeight;
    if (!chart) chart = window.echarts.init(container);
    else if (resized) chart.resize({ width, height });
    if (themeChanged || resized) {
      chart.setOption(layout());
      themeChanged = false;
      renderedRevision = -1;
    }
    if (renderedRevision !== displayRevision) {
      chart.setOption({
        xAxis: CHANNELS.map(({ id }) => ({ id, min: view.min, max: view.max })),
        series: view.series,
      }, { lazyUpdate: true });
      renderedRevision = displayRevision;
    }
  }

  function pause() {
    paused = paused ? null : history.points.slice();
    displayRevision++;
    schedule();
  }
  function clear() {
    history.clear();
    if (paused) paused = [];
    displayRevision++;
    schedule();
  }
  pauseButton.addEventListener("click", pause);
  clearButton.addEventListener("click", clear);
  windowSelect.addEventListener("change", () => { displayRevision++; schedule(); });
  new ResizeObserver(schedule).observe(container);

  return {
    get cursor() { return history.cursor; },
    selectTransport(next) {
      if (next === transport) return;
      views[transport].paused = paused;
      transport = next;
      history = views[next].history;
      paused = views[next].paused;
      snapshot = undefined;
      serviceAvailable = null;
      displayRevision++;
      schedule();
    },
    update(next) {
      if (!next) return;
      const priorSource = history.source;
      serviceAvailable = true;
      snapshot = next;
      const changed = history.ingest(next.trend);
      if (priorSource !== history.source) {
        if (paused) paused = [];
        displayRevision++;
      } else if (changed && !paused) displayRevision++;
      // 暂停固定可见窗口；缓存照常接收。render 仍更新断连/过期提示。
      schedule();
    },
    render: schedule,
    unavailable() { serviceAvailable = false; schedule(); },
    refreshTheme() { themeChanged = true; schedule(); },
    pause,
    clear,
  };
}
