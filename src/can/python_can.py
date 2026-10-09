# Copyright The eha-sdk Contributors
"""用户配置的 python-can Bus 通用帧桥接。

外部 Rust 进程负责请求顺序和期限。本文件刻意不包含适配器名称、传输协议、位速率、
USB 枚举或配置写入。
"""

import argparse
import json
import sys
import traceback


MAX_LINE = 8 * 1024
MAX_DATA = 64
MAX_TIMEOUT_MS = 2_000
FD_DLC_TO_LENGTH = {
    0: 0, 1: 1, 2: 2, 3: 3, 4: 4, 5: 5, 6: 6, 7: 7, 8: 8,
    9: 12, 10: 16, 11: 20, 12: 24, 13: 32, 14: 48, 15: 64,
}
LENGTH_TO_FD_DLC = {length: dlc for dlc, length in FD_DLC_TO_LENGTH.items()}


def emit(payload):
    encoded = json.dumps(payload, separators=(",", ":"), ensure_ascii=True)
    if len(encoded) > MAX_LINE:
        encoded = '{"id":null,"ok":false,"error":{"message":"桥接响应超过长度上限"}}'
    sys.stdout.write(encoded + "\n")
    sys.stdout.flush()


def failure(request_id, message):
    emit({"id": request_id, "ok": False, "error": {"message": message}})


def error_detail(error):
    """保留外部驱动的异常因果链，避免通用包装掩盖实际 I/O 故障。"""
    details = []
    seen = set()
    while error is not None and id(error) not in seen and len(details) < 4:
        seen.add(id(error))
        details.append(f"{type(error).__name__}: {error}")
        error = error.__cause__
    return "；由以下异常引起：".join(details)


def require(value, name, expected):
    if not isinstance(value, expected):
        raise ValueError(f"{name} 字段无效")
    return value


def frame_from_wire(value):
    value = require(value, "frame", dict)
    frame_id = require(value.get("id"), "id", int)
    extended = require(value.get("extended"), "extended", bool)
    rtr = require(value.get("rtr"), "rtr", bool)
    fdf = require(value.get("fdf"), "fdf", bool)
    brs = require(value.get("brs"), "brs", bool)
    esi = require(value.get("esi"), "esi", bool)
    dlc = require(value.get("dlc"), "dlc", int)
    data = require(value.get("data"), "data", list)
    if isinstance(frame_id, bool) or frame_id < 0 or frame_id > (0x1FFFFFFF if extended else 0x7FF):
        raise ValueError("CAN 标识符超出范围")
    if isinstance(dlc, bool) or dlc not in FD_DLC_TO_LENGTH:
        raise ValueError("CAN DLC 无效")
    if not fdf and dlc > 8:
        raise ValueError("经典 CAN DLC 无效")
    if len(data) > MAX_DATA or any(not isinstance(byte, int) or isinstance(byte, bool) or byte < 0 or byte > 255 for byte in data):
        raise ValueError("CAN 数据无效")
    if rtr:
        if fdf or brs or esi or dlc > 8 or data:
            raise ValueError("远程 CAN 帧必须为空且使用经典 CAN")
        return frame_id, extended, rtr, fdf, brs, esi, dlc, b""
    expected = FD_DLC_TO_LENGTH[dlc]
    if len(data) != expected:
        raise ValueError("CAN DLC 与数据长度不一致")
    if not fdf and (brs or esi):
        raise ValueError("经典 CAN 帧不能带 BRS 或 ESI")
    return frame_id, extended, rtr, fdf, brs, esi, dlc, bytes(data)


def frame_to_wire(message):
    if message.is_error_frame:
        raise ValueError(f"python-can 报告了错误帧: id=0x{message.arbitration_id:X}")
    data = bytes(message.data)
    if len(data) > MAX_DATA:
        raise ValueError("python-can 帧数据超过 64 字节")
    fdf = bool(message.is_fd)
    rtr = bool(message.is_remote_frame)
    dlc_length = message.dlc
    if not isinstance(dlc_length, int):
        raise ValueError("python-can 消息 DLC 无效")
    if rtr:
        if fdf or message.bitrate_switch or message.error_state_indicator or dlc_length < 0 or dlc_length > 8 or data:
            raise ValueError("python-can 远程帧格式错误")
        return {
            "id": message.arbitration_id,
            "extended": bool(message.is_extended_id),
            "rtr": True,
            "fdf": False,
            "brs": False,
            "esi": False,
            "dlc": dlc_length,
            "data": [],
        }
    if dlc_length != len(data):
        raise ValueError("python-can 消息 DLC 与实际数据长度不一致")
    try:
        dlc = LENGTH_TO_FD_DLC[dlc_length] if fdf else dlc_length
    except KeyError as error:
        raise ValueError("python-can 消息长度没有有效的 CAN FD DLC") from error
    if not fdf and (message.bitrate_switch or message.error_state_indicator):
        raise ValueError("经典 python-can 帧带有 FD 标志")
    return {
        "id": message.arbitration_id,
        "extended": bool(message.is_extended_id),
        "rtr": False,
        "fdf": fdf,
        "brs": bool(message.bitrate_switch),
        "esi": bool(message.error_state_indicator),
        "dlc": dlc,
        "data": list(data),
    }


def version_is_supported(version):
    parts = version.split(".")
    try:
        major, minor, patch = int(parts[0]), int(parts[1]), int(parts[2])
    except (IndexError, ValueError):
        return False
    return major == 4 and (minor, patch) >= (6, 1)


def configured_bus(can, context):
    """通过 python-can 的公共配置加载器解析命名上下文。

    合并后的选择必须同时给出驱动接口和通道。把这些值直接传给 `Bus` 可避免无关的
    进程级默认值选择其他物理通道，同时仍由 python-can 负责规范化其余配置和位时序对象。
    """
    file_config = can.util.load_file_config(section=context) or {}
    environment_config = can.util.load_environment_config(context) or {}
    config = dict(file_config)
    config.update(environment_config)
    if "interface" not in config and "bustype" in config:
        config["interface"] = config.pop("bustype")
    interface = config.get("interface")
    channel = config.get("channel")
    if (
        not isinstance(interface, str)
        or not interface.strip()
        or not isinstance(channel, (str, int))
        or isinstance(channel, bool)
        or (isinstance(channel, str) and not channel.strip())
    ):
        raise ValueError(
            f"python-can 上下文 {context!r} 必须解析出明确的 interface 和 channel"
        )
    return can.Bus(config_context=context, **config)


def main():
    parser = argparse.ArgumentParser(add_help=False)
    parser.add_argument("--context", required=True)
    args = parser.parse_args()
    if not args.context.strip():
        failure(0, "python-can 配置上下文不能为空")
        return 2
    try:
        import can
    except Exception as error:  # 此包不由本 SDK 管理。
        failure(0, f"无法导入 python-can: {error}")
        return 2
    if not version_is_supported(can.__version__):
        failure(0, f"不支持 python-can {can.__version__}；要求 >=4.6.1,<5")
        return 2
    try:
        bus = configured_bus(can, args.context)
    except Exception as error:
        failure(0, f"无法打开外部配置的 python-can 上下文 {args.context!r}: {error}")
        return 2
    emit({"id": 0, "ok": True, "op": "ready", "version": can.__version__})
    try:
        for line in sys.stdin:
            if len(line.encode("utf-8", "replace")) > MAX_LINE:
                failure(None, "桥接请求超过长度上限")
                continue
            try:
                request = json.loads(line)
                request = require(request, "request", dict)
                request_id = require(request.get("id"), "id", int)
                operation = require(request.get("op"), "op", str)
                payload = require(request.get("payload"), "payload", dict)
                if operation == "close":
                    try:
                        bus.shutdown()
                    except Exception as error:
                        failure(request_id, f"无法关闭 python-can Bus: {error}")
                        return 1
                    bus = None
                    emit({"id": request_id, "ok": True})
                    return 0
                if operation == "send":
                    frame_id, extended, rtr, fdf, brs, esi, dlc, data = frame_from_wire(payload)
                    timeout_ms = payload.get("timeout_ms")
                    if not isinstance(timeout_ms, int) or isinstance(timeout_ms, bool) or not 0 <= timeout_ms <= MAX_TIMEOUT_MS:
                        raise ValueError("send 超时无效")
                    message = can.Message(
                        arbitration_id=frame_id,
                        is_extended_id=extended,
                        is_remote_frame=rtr,
                        is_fd=fdf,
                        bitrate_switch=brs,
                        error_state_indicator=esi,
                        data=data,
                        # python-can 将 FD DLC 表示为字节数；Rust 传输层保留线路编码，
                        # 因此此处使用已校验的负载长度。
                        dlc=dlc if rtr else len(data),
                        check=True,
                    )
                    bus.send(message, timeout=timeout_ms / 1000)
                    emit({"id": request_id, "ok": True, "submitted": True})
                    continue
                if operation == "receive":
                    timeout_ms = payload.get("timeout_ms")
                    if not isinstance(timeout_ms, int) or isinstance(timeout_ms, bool) or not 0 <= timeout_ms <= MAX_TIMEOUT_MS:
                        raise ValueError("receive 超时无效")
                    message = bus.recv(timeout=timeout_ms / 1000)
                    emit({"id": request_id, "ok": True, "frame": None if message is None else frame_to_wire(message)})
                    continue
                raise ValueError("未知桥接操作")
            except Exception as error:
                failure(locals().get("request_id"), error_detail(error))
    finally:
        if bus is not None:
            try:
                bus.shutdown()
            except Exception:
                traceback.print_exc(file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
