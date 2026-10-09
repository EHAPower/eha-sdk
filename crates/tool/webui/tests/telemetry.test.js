// Copyright The eha-sdk Contributors
import test from "node:test";
import assert from "node:assert/strict";
import { TelemetryHistory, selectedContactGuidance, seriesWindow, telemetryReception } from "../static/telemetry.js";

const good = (value) => ({ value, result: 1, quality: 1, stale: false });
const point = (cursor, time_us, overrides = {}) => ({ cursor, time_us, sequence: cursor, position: good(cursor), velocity: good(-cursor), force: good(cursor * 10), ...overrides });
const source = (run_nonce = "a") => ({ connection: { transport: "usb", serial: "fixture" }, uid: "fixture", run_nonce });
const batch = (points, overrides = {}) => ({ source: source(), points, cursor: points.at(-1)?.cursor ?? 0, reset: false, dropped: 0, ...overrides });
const identity = { host_contact_max_age_ms: 100 };

test("closed sessions show reception stopped rather than an increasing live age", () => {
  for (const age of [5, 5000, 60000]) {
    assert.deepEqual(telemetryReception({ connected:false, telemetry:{ received_age_ms:age } }), { label:"已停止接收", state:"idle" });
    assert.deepEqual(telemetryReception({ connected:true, transport:{ disconnected:"USB removed" }, telemetry:{ received_age_ms:age } }), { label:"已停止接收", state:"error" });
  }
  assert.deepEqual(telemetryReception({ connected:false }), { label:"未连接", state:"idle" });
});

test("stopping control preserves live telemetry age and warns if new frames stop", () => {
  const snapshot = (age) => ({ connected:true, telemetry:{ target_mode:0, received_age_ms:age } });
  assert.deepEqual(telemetryReception(snapshot(5)), { label:"5 ms", state:"success" });
  assert.deepEqual(telemetryReception(snapshot(2001)), { label:"遥测未更新 · 2001 ms", state:"warning" });
  assert.deepEqual(telemetryReception(snapshot(3001)), { label:"遥测未更新 · 3001 ms", state:"warning" });
  assert.deepEqual(telemetryReception({ connected:true }), { label:"等待遥测", state:"idle" });
});

test("incremental batches preserve every received sample and use actual firmware time", () => {
  const history = new TelemetryHistory();
  history.ingest(batch([point(1, 10_000_000), point(2, 10_010_000), point(3, 10_030_000)]));
  history.ingest(batch([point(3, 10_030_000), point(4, 10_040_000)]));
  assert.equal(history.cursor, 4);
  assert.equal(history.points.length, 4);
  assert.deepEqual(seriesWindow(history.points, 30, 100).series[0].data, [[10, 1], [10.01, 2], [10.03, 3], [10.04, 4]]);
});

test("invalid, stale and unqualified values break their own curve instead of becoming zero", () => {
  const points = [point(1, 1_000_000), point(2, 1_010_000, {
    position: { ...good(10), stale: true }, velocity: { ...good(20), quality: 2 }, force: { ...good(30), result: 2 },
  }), point(3, 1_020_000, { force: good(Number.NaN) })];
  const { series } = seriesWindow(points, 30, 100);
  assert.deepEqual(series[0].data, [[1, 1], [1.01, null], [1.02, 3]]);
  assert.equal(series[1].data[1][1], null);
  assert.deepEqual(series[2].data.map((item) => item[1]), [10, null, null]);
});

test("accepted target history is rendered separately from feedback and does not fill another mode", () => {
  const { targets } = seriesWindow([
    point(1, 1_000_000, { target_mode:1, target_values:[12, 0, 0] }),
    point(2, 1_010_000, { target_mode:2, target_values:[3, 0, 0] }),
    point(3, 1_020_000, { target_mode:4, target_values:[20, 4, 1] }),
  ], 30, 100);
  assert.deepEqual(targets[0].data.map((item) => item[1]), [12, null, 20]);
  assert.deepEqual(targets[1].data.map((item) => item[1]), [null, 3, null]);
  assert.deepEqual(targets[2].data.map((item) => item[1]), [null, null, null]);
});

test("overflow and missing time intervals produce explicit breaks without inventing samples", () => {
  const points = [point(1, 1_000_000), point(2, 1_010_000, { gap: true }), point(3, 2_000_000)];
  const view = seriesWindow(points, 30, 100);
  assert.equal(view.gaps, 2);
  assert.deepEqual(view.series[0].data.map((item) => item[1]), [1, null, 2, null, 3]);
  assert.deepEqual(view.focus.map((entry) => entry.dataIndex), [0, 2, 4]);
  assert.deepEqual(view.focus.map((entry) => entry.point.cursor), [1, 2, 3]);
});

test("a new source and server reset discard old curves even if timestamps overlap", () => {
  const history = new TelemetryHistory();
  history.ingest(batch([point(1, 100_000_000)]));
  history.ingest(batch([point(2, 1_000_000)], { source: source("b") }));
  assert.deepEqual(history.points.map((item) => item.cursor), [2]);
  history.ingest(batch([], { source: source("b"), reset: true, cursor: 3 }));
  assert.equal(history.points.length, 0);
  assert.equal(history.cursor, 3);
});

test("late batches cannot rewind the cursor or append out-of-order device time", () => {
  const history = new TelemetryHistory();
  history.ingest(batch([point(10, 10_000_000)]));
  history.ingest(batch([point(9, 9_000_000)]));
  history.ingest(batch([point(11, 9_500_000), point(12, 10_010_000)]));
  assert.equal(history.cursor, 12);
  assert.deepEqual(history.points.map((item) => item.cursor), [10, 12]);
});

test("clear keeps the receive cursor and bounded history survives sequence wrap", () => {
  const history = new TelemetryHistory();
  history.ingest(batch([point(1, 1_000_000, { sequence: 0xffffffff })]));
  history.clear();
  history.ingest(batch([point(1, 1_000_000), point(2, 1_010_000, { sequence: 0 })]));
  assert.deepEqual(history.points.map((item) => item.cursor), [2]);
  history.ingest(batch(Array.from({ length: 3200 }, (_, index) => point(index + 3, 1_020_000 + index * 10_000))));
  assert.ok(history.points.length <= 3000);
  const window = seriesWindow(history.points, 10, 100);
  assert.equal(window.max - window.min, 10);
  assert.ok(window.series[0].data.length <= 1001);
});

test("selected entry guidance waits for fresh Active contact before suggesting a target", () => {
  const expired = { received_age_ms: 10, usb_contact: 2 };
  assert.match(selectedContactGuidance("usb", identity, expired, { enabled:false }).detail, /先启用 USB 心跳/);
  assert.match(selectedContactGuidance("usb", identity, expired, { enabled:true }).detail, /心跳已调度；等待固件报告 USB 联系有效/);
  assert.match(selectedContactGuidance("usb", identity, { received_age_ms: 101, usb_contact: 1 }, { enabled:true }).detail, /等待当前运行实例的新遥测/);
  assert.equal(selectedContactGuidance("usb", identity, { received_age_ms: 10, usb_contact: 1, usb_heartbeat_age_us: 20_000 }, { enabled:false }), null);
});

test("Active contact is historical when its reported age exceeds the Identity allowance", () => {
  const guidance = selectedContactGuidance("usb", identity, { received_age_ms: 50, usb_contact: 1, usb_heartbeat_age_us: 60_000 });
  assert.match(guidance.detail, /上次反馈已不足确认当前 USB 联系；等待新遥测/);
  assert.match(selectedContactGuidance("usb", null, { received_age_ms: 10, usb_contact: 1, usb_heartbeat_age_us: 0 }).detail, /等待当前运行实例的 Identity/);
});

test("current contact clears a retained heartbeat scheduler error", () => {
  assert.equal(selectedContactGuidance("usb", identity, { received_age_ms: 10, usb_contact: 1, usb_heartbeat_age_us: 20_000 }, { enabled:true, error:"USB write failed" }), null);
});

test("a retained heartbeat error does not remain current after scheduling stops", () => {
  const guidance = selectedContactGuidance("usb", identity, { received_age_ms: 10, usb_contact: 2 }, { enabled:false, error:"USB write failed" });
  assert.equal(guidance.title, "启用入口心跳");
  assert.match(guidance.detail, /先启用 USB 心跳/);
});

test("current heartbeat scheduling errors remain visible until contact is confirmed", () => {
  const guidance = selectedContactGuidance("usb", identity, { received_age_ms: 10, usb_contact: 2 }, { enabled:true, error:"USB write failed" });
  assert.equal(guidance.title, "心跳调度错误");
  assert.match(guidance.detail, /查看原始错误和入口新遥测/);
});
