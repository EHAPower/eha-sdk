// Copyright The eha-sdk Contributors

import test from "node:test";
import assert from "node:assert/strict";
import { autoconnectStatus, canableAdapters, connectionControls, connectionErrorGuidance, managedConnectPayload, managedProfilePayload, runtimeStatus, shouldSelectAutoconnectedCan } from "../static/can-connection-ui.js";

test("saved connection posts only its server profile id", () => {
  assert.deepEqual(managedConnectPayload({ profileId:"shop-floor", adapterSerial:"ignored", node:1, mode:"fd" }), {
    transport:"can", profile_id:"shop-floor",
  });
});

test("new managed connection binds Identity only after its connect request succeeds", () => {
  assert.deepEqual(managedConnectPayload({ adapterSerial:"ABC123", node:1, mode:"fd", name:"工作台", autoconnect:true, isDefault:true }), {
    transport:"can",
    managed:{ adapter:{ kind:"canable2", serial:"ABC123" }, node:1, mode:"fd", name:"工作台", save:true, bind_identity:true, autoconnect:true, default:true },
  });
});

test("profile update keeps the selected server id and never carries a browser identity binding", () => {
  assert.deepEqual(managedProfilePayload({ id:"workbench", adapterSerial:"ABC123", node:2, mode:"fd", name:"工作台", autoconnect:false, isDefault:false }), {
    profile:{ id:"workbench", name:"工作台", adapter:{ kind:"canable2", serial:"ABC123" }, node:2, mode:"fd", autoconnect:false, default:false },
  });
});

test("connection controls require a selected profile or an adapter and keep profile saves behind Identity", () => {
  assert.equal(connectionControls({ busy:false, trialLocked:false, held:false, profileId:"", adapterSerial:"" }).connect, true);
  assert.equal(connectionControls({ busy:false, trialLocked:false, held:false, profileId:"", adapterSerial:"ABC123" }).connect, false);
  const held = connectionControls({ busy:false, trialLocked:false, held:true, profileId:"workbench", adapterSerial:"ABC123" });
  assert.equal(held.connect, true);
  assert.equal(held.adapter, true);
  assert.equal(connectionControls({ busy:false, trialLocked:false, held:false, identityReady:false, profileId:"workbench", adapterSerial:"ABC123" }).save, true);
  assert.equal(connectionControls({ busy:false, trialLocked:false, held:false, identityReady:true, profileId:"workbench", adapterSerial:"ABC123" }).save, false);
});

test("known managed-connection failures give an actionable next step", () => {
  assert.match(connectionErrorGuidance("identity_mismatch"), /不会替换绑定/);
  assert.equal(connectionErrorGuidance("unrelated"), null);
});

test("startup selects a verified managed CAN session without replacing a ready USB session", () => {
  assert.equal(shouldSelectAutoconnectedCan({ status:"connected", activeTransport:"usb", usbReady:false }), true);
  assert.equal(shouldSelectAutoconnectedCan({ status:"connected", activeTransport:"usb", usbReady:true }), false);
  assert.equal(shouldSelectAutoconnectedCan({ status:"failed", activeTransport:"usb", usbReady:false }), false);
});

test("actual adapter discovery response keeps the CANable2 without an invented stable field", () => {
  const response = { ok:true, adapters:[{ kind:"canable2", serial:"207033785743", path:"/dev/cu.usbmodem2070337857431", description:"CANable 2.0", vid:5840, pid:4478 }] };
  assert.deepEqual(canableAdapters(response), response.adapters);
});

test("actual connection response presents runtime health and preserves a string autoconnect reason", () => {
  const response = { ok:true, profiles:[], autoconnect:{ attempted:true, status:"failed", error:"未找到随附 Python 运行时" }, runtime:{ mode:"bundled", python:"/Applications/EHA/runtime/python/bin/python3", healthy:true } };
  assert.deepEqual(runtimeStatus(response.runtime), { text:"CAN 运行时：随附 Python 已验证。", error:false });
  assert.equal(autoconnectStatus(response.autoconnect), "启动自动连接失败：未找到随附 Python 运行时");
});
