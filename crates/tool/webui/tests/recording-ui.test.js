// Copyright The eha-sdk Contributors

import test from "node:test";
import assert from "node:assert/strict";
import { recordingSummary } from "../static/recording-ui.js";

const entry = {
  id:"run-1791530655162-89413",
  metadata:{
    started_unix_time_ms:1791530655000,
    connection:{ transport:"can", node:1 },
    device_uid:"240018000e51343033333232",
    completion:{ active:false, events:13, telemetry:17_308, dropped:0, ended_unix_time_ms:1791530834248 },
  },
};

test("recording list summarizes actual metadata and completion", () => {
  const summary = recordingSummary(entry, (value) => `T${value}`);
  assert.equal(summary.title, "T1791530655000 · CAN 节点 1");
  assert.equal(summary.id, "run-1791530655162-89413");
  assert.equal(summary.detail, "设备 240018000e51343033333232 · 已完成：事件 13，遥测 17,308，丢弃 0，结束 T1791530834248");
});

test("invalid completion counts stay unrecorded instead of becoming zero", () => {
  const summary = recordingSummary({ ...entry, metadata:{ ...entry.metadata, completion:{ events:null, telemetry:undefined, dropped:-1 } } }, (value) => `T${value}`);
  assert.match(summary.detail, /事件 未记录，遥测 未记录，丢弃 未记录/);
});

test("older recordings fall back to the timestamp embedded in the run id", () => {
  const summary = recordingSummary({ id:"run-1791530655162-89413", metadata:{ connection:{ transport:"usb" }, identity:{ uid:"fixture" } } }, (value) => `T${value}`);
  assert.equal(summary.title, "T1791530655162 · USB");
  assert.equal(summary.detail, "设备 fixture · 未完成（未写入 completion）");
});
