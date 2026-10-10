// Copyright The eha-sdk Contributors

import test from "node:test";
import assert from "node:assert/strict";
import { rawJsonNumber } from "../static/config-editor.js";
import { trialState, trialStatusText } from "../static/trial-state.js";
import { sliderTarget } from "../static/trial-ui.js";

test("连续位置滑块拒绝超出可计算范围的十进制指数", () => {
  assert.throws(
    () => sliderTarget(rawJsonNumber("1e1001", "位置下限"), rawJsonNumber("2", "位置上限"), 500n),
    /范围过大/,
  );
});

test("completed selected member still exposes group Stop while another member is stopping", () => {
  const view = trialState(
    { connection:{ node:1 }, trial:{ state:"completed" } },
    [{ node:1, trial:{ state:"completed" } }, { node:2, trial:{ state:"stopping" } }],
    [1, 2],
  );
  assert.equal(view.live, true);
  assert.equal(view.groupScope, true);
  assert.equal(view.stopAvailable, true);
  assert.equal(view.label, "停止中");
});

test("terminal unknown warns without holding the recovery lock", () => {
  const view = trialState({ trial:{ state:"interrupted_unknown", unknown:true } });
  assert.equal(view.live, false);
  assert.equal(view.stopAvailable, false);
  assert.equal(view.terminalUnknown, true);
});

test("USB Identity CAN node never overwrites the CAN node's live trial", () => {
  const view = trialState(
    { connection:{ transport:"usb" }, identity:{ active_can_node:1 }, trial:{ state:"idle" } },
    [{ node:1, trial:{ state:"active" } }],
  );
  assert.equal(view.live, true);
});

test("a live session on the other transport retains the global write lock", () => {
  const view = trialState(
    { connection:{ transport:"can", node:1 }, trial:{ state:"idle" } },
    [], [], { usb:{ trial:{ state:"active" } }, can:{ trial:{ state:"idle" } } },
  );
  assert.equal(view.live, true);
  assert.equal(view.stopAvailable, false);
});

test("completed status exposes its stop reason and observed confirmation", () => {
  assert.equal(trialState({ trial:{ state:"completed" } }).label, "已完成");
  assert.match(trialStatusText({ state:"completed", stop_reason:"position_reached", stop_observed:true }), /位置到位.*无目标且驱动 Idle/);
  assert.match(trialStatusText({ state:"completed", stop_reason:"explicit", stop_observed:true, stop_submission:{ unknown_no_retry:true } }), /Stop 提交结果未确认.*无目标且驱动 Idle/);
  assert.match(trialStatusText({ state:"interrupted_unknown", reason:"transport_disconnected:lost" }), /结果未知.*transport_disconnected/);
});
