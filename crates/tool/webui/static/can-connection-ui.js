// Copyright The eha-sdk Contributors

export function managedConnectPayload({ profileId, adapterSerial, node, mode, name, autoconnect, isDefault }) {
  if (profileId) return { transport:"can", profile_id:profileId };
  return {
    transport:"can",
    managed:{
      adapter:{ kind:"canable2", serial:adapterSerial },
      node,
      mode,
      ...(name ? { name } : {}),
      save:true,
      bind_identity:true,
      autoconnect:Boolean(autoconnect),
      default:Boolean(isDefault),
    },
  };
}

export function managedProfilePayload({ id, adapterSerial, node, mode, name, autoconnect, isDefault }) {
  return {
    profile:{
      ...(id ? { id } : {}),
      name,
      adapter:{ kind:"canable2", serial:adapterSerial },
      node,
      mode,
      autoconnect:Boolean(autoconnect),
      default:Boolean(isDefault),
    },
  };
}

export function connectionControls({ busy, trialLocked, held, identityReady, profileId, adapterSerial }) {
  const locked = Boolean(busy || trialLocked || held);
  return {
    profile:locked,
    adapter:locked,
    connect:locked || (!profileId && !adapterSerial),
    save:Boolean(busy || trialLocked || !identityReady || !profileId || !adapterSerial),
    remove:Boolean(busy || trialLocked || !profileId),
    refresh:Boolean(busy || trialLocked || held),
  };
}

export function connectionErrorGuidance(code) {
  return ({
    adapter_not_found:"未找到已绑定的 CANable 2。请刷新适配器列表；若设备已更换，选择新适配器后重新连接并保存。",
    adapter_ambiguous:"发现多个可用 CANable 2。请选择完整序列号后再连接。",
    profile_not_found:"保存的连接已不存在。请刷新已保存连接并重新选择。",
    identity_mismatch:"读取到的 Identity 与此连接绑定的设备不一致。不会替换绑定；请明确选择适配器和节点后新建连接。",
    connection_invalid:"保存的连接参数无效。请检查适配器、节点号和帧格式后更新。",
  })[code] || null;
}

export function shouldSelectAutoconnectedCan({ status, activeTransport, usbReady }) {
  return status === "connected" && activeTransport === "usb" && !usbReady;
}

export function canableAdapters(response) {
  return Array.isArray(response?.adapters)
    ? response.adapters.filter((adapter) => adapter?.kind === "canable2" && typeof adapter.serial === "string" && adapter.serial.length > 0)
    : [];
}

export function runtimeStatus(runtime) {
  if (!runtime) return { text:"CAN 运行时状态暂不可用。", error:true };
  if (runtime.mode === "bundled" && runtime.healthy === true) return { text:"CAN 运行时：随附 Python 已验证。", error:false };
  if (runtime.mode === "developer") return { text:"CAN 运行时：使用开发解释器。", error:false };
  return { text:`CAN 运行时不可用：${runtime.error || "请检查随附运行时。"}`, error:true };
}

export function autoconnectStatus(state) {
  if (!state || !state.attempted) return "保存的默认连接会在服务启动时自动尝试一次；页面不会重试。";
  if (state.status === "connected") return "服务启动时已自动连接并核对 Identity。";
  if (state.status === "failed") {
    const error = typeof state.error === "string" ? state.error : connectionErrorGuidance(state.error?.code) || state.error?.message;
    return `启动自动连接失败：${error || "请刷新适配器列表后明确选择。"}`;
  }
  return "服务正在处理启动自动连接。";
}
