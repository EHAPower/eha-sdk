"""ODrive 0.5.1 USB只读桥；由 eha-sdk 内嵌执行，不能独立作为操作入口。"""

import json
import math
import sys
import time


USB_VENDOR = 0x1209
USB_PRODUCT = 0x0D32
EXPECTED_ODRIVE_VERSION = "0.5.1.post0"


def emit(value):
    print(
        json.dumps(value, ensure_ascii=False, separators=(",", ":"), allow_nan=False),
        flush=True,
    )


def fail(kind, message, detail=None):
    error = {"kind": kind, "message": message}
    if detail:
        error["detail"] = str(detail)
    emit({"source": "odrive_usb", "ok": False, "error": error})
    return 1


def parse_serial(text):
    text = str(text).strip()
    digits = text[2:] if text.lower().startswith("0x") else text
    try:
        value = int(digits, 16)
    except ValueError as error:
        raise ValueError("serial 必须是十六进制正整数，可省略 0x 前缀") from error
    if value <= 0:
        raise ValueError("serial 必须是十六进制正整数，可省略 0x 前缀")
    return value


def serial_text(value):
    return f"0x{value:X}"


def usb_serials(usb_core):
    import usb.util

    serials = []
    for device in usb_core.find(find_all=True, idVendor=USB_VENDOR, idProduct=USB_PRODUCT):
        try:
            text = device.serial_number
            if not text:
                raise RuntimeError("设备没有 USB 序列号")
            serials.append(parse_serial("0x" + text))
        except Exception as error:
            raise RuntimeError(f"无法读取 ODrive USB 序列号：{error}") from error
        finally:
            usb.util.dispose_resources(device)
    return sorted(serials)


def optional(read, name, unavailable):
    try:
        value = read()
        if isinstance(value, float) and not math.isfinite(value):
            raise ValueError("读取值不是有限数")
        return value
    except Exception as error:
        unavailable[name] = str(error)
        return None


def state_description(enums, value):
    for constant in dir(enums):
        if constant.startswith("AXIS_STATE_") and getattr(enums, constant) == value:
            return constant
    return f"AXIS_STATE_UNKNOWN_0x{value:X}"


def error_description(enums, prefix, value):
    if value == 0:
        return f"{prefix}NONE"
    names = []
    known = 0
    for constant in sorted(name for name in dir(enums) if name.startswith(prefix)):
        bit = getattr(enums, constant)
        if bit and value & bit:
            names.append(constant)
            known |= bit
    unknown = value & ~known
    if unknown:
        names.append(f"{prefix}UNKNOWN_BITS_0x{unknown:X}")
    return ", ".join(names)


def axis_snapshot(axis, name, unavailable, enums):
    current_state = int(axis.current_state)
    error = int(axis.error)
    motor_error = int(axis.motor.error)
    encoder_error = int(axis.encoder.error)
    controller_error = int(axis.controller.error)
    return {
        "current_state": current_state,
        "current_state_description": state_description(enums, current_state),
        "error": error,
        "error_description": error_description(enums, "AXIS_ERROR_", error),
        "motor_error": motor_error,
        "motor_error_description": error_description(enums, "MOTOR_ERROR_", motor_error),
        "encoder_error": encoder_error,
        "encoder_error_description": error_description(enums, "ENCODER_ERROR_", encoder_error),
        "controller_error": controller_error,
        "controller_error_description": error_description(enums, "CONTROLLER_ERROR_", controller_error),
        "motor_calibrated": bool(axis.motor.is_calibrated),
        "encoder_ready": bool(axis.encoder.is_ready),
        "motor_pre_calibrated": bool(axis.motor.config.pre_calibrated),
        "encoder_pre_calibrated": bool(axis.encoder.config.pre_calibrated),
        "encoder_position_estimate": optional(
            lambda: float(axis.encoder.pos_estimate),
            f"{name}.encoder_position_estimate",
            unavailable,
        ),
        "encoder_velocity_estimate": optional(
            lambda: float(axis.encoder.vel_estimate),
            f"{name}.encoder_velocity_estimate",
            unavailable,
        ),
    }


def read_snapshot(serial, timeout):
    try:
        import fibre
        import odrive
        import usb.core
        from odrive import enums
    except ImportError as error:
        raise LookupError(
            "缺少 ODrive 依赖；请安装 odrive==0.5.1.post0 及其 USB 依赖"
        ) from error
    installed_version = getattr(odrive, "__version__", None)
    if installed_version != EXPECTED_ODRIVE_VERSION:
        raise LookupError(
            f"ODrive Python 版本必须为 {EXPECTED_ODRIVE_VERSION}，实际为 {installed_version!r}"
        )

    requested = parse_serial(serial)
    matches = [candidate for candidate in usb_serials(usb.core) if candidate == requested]
    if not matches:
        raise ConnectionError(f"未发现请求的 ODrive USB 序列号 {serial_text(requested)}")
    if len(matches) != 1:
        raise ConnectionError(f"请求的 ODrive USB 序列号 {serial_text(requested)} 出现多个候选")

    try:
        drive = odrive.find_any(serial_number=f"{requested:X}", timeout=timeout)
    except Exception as error:
        raise ConnectionError(f"连接 ODrive 失败：{error}") from error
    if drive is None:
        raise ConnectionError(f"在 {timeout:g} 秒内未连接到 {serial_text(requested)}")

    try:
        actual = int(drive.serial_number)
    except (AttributeError, TypeError, ValueError) as error:
        raise RuntimeError("已连接设备无法读取序列号") from error
    if actual != requested:
        raise PermissionError(
            f"已连接 {serial_text(actual)}，与请求 {serial_text(requested)} 不一致"
        )

    unavailable = {}
    try:
        can_error = int(drive.can.error)
        return {
            "source": "odrive_usb",
            "ok": True,
            "decoder_source": f"odrive.enums=={EXPECTED_ODRIVE_VERSION}",
            "odrive_package_version": installed_version,
            "serial_number": serial_text(actual),
            "firmware_version": {
                "major": int(drive.fw_version_major),
                "minor": int(drive.fw_version_minor),
                "revision": int(drive.fw_version_revision),
                "unreleased": int(drive.fw_version_unreleased),
            },
            "can_error": can_error,
            "can_error_description": error_description(
                enums, "CAN_ERROR_", can_error
            ),
            "axis0": axis_snapshot(drive.axis0, "axis0", unavailable, enums),
            "axis1": axis_snapshot(drive.axis1, "axis1", unavailable, enums),
            "metrics": {
                "vbus_voltage": optional(lambda: float(drive.vbus_voltage), "vbus_voltage", unavailable),
                "ibus": optional(lambda: float(drive.ibus), "ibus", unavailable),
                "fet_temperature": optional(
                    lambda: float(drive.axis0.fet_thermistor.temperature),
                    "axis0.fet_thermistor.temperature",
                    unavailable,
                ),
                "motor_temperature": optional(
                    lambda: float(drive.axis0.motor_thermistor.temperature),
                    "axis0.motor_thermistor.temperature",
                    unavailable,
                ),
                "iq_measured": optional(
                    lambda: float(drive.axis0.motor.current_control.Iq_measured),
                    "axis0.iq_measured",
                    unavailable,
                ),
            },
            "unavailable": unavailable,
        }
    except (AttributeError, TypeError, ValueError, RuntimeError) as error:
        raise RuntimeError(f"无法完整读取 ODrive 状态：{error}") from error


def main(argv):
    if len(argv) != 4 or argv[1] not in ("discover", "read"):
        return fail("invalid_input", "用法：discover 或 read <serial> <timeout_seconds>")
    started = int(time.time() * 1000)
    try:
        timeout = float(argv[3])
        if timeout <= 0:
            raise ValueError("timeout 必须是正秒数")
        if argv[1] == "discover":
            try:
                import usb.core
            except ImportError as error:
                raise LookupError(
                    "缺少 PyUSB；请安装 odrive==0.5.1.post0 及其 USB 依赖"
                ) from error
            result = {"source": "odrive_usb", "ok": True, "devices": [
                {"serial_number": serial_text(serial)} for serial in usb_serials(usb.core)
            ]}
        else:
            result = read_snapshot(argv[2], timeout)
    except LookupError as error:
        return fail("dependency", str(error), error.__cause__)
    except ConnectionError as error:
        return fail("connection", str(error), error.__cause__)
    except PermissionError as error:
        return fail("identity_mismatch", str(error), error.__cause__)
    except ValueError as error:
        return fail("invalid_input", str(error), error.__cause__)
    except Exception as error:
        return fail("read", str(error), error)
    result["read_started_unix_ms"] = started
    result["read_finished_unix_ms"] = int(time.time() * 1000)
    emit(result)
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
