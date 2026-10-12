# Copyright The eha-sdk Contributors
"""只依赖标准库的外部 python-can 桥接合同检查。"""

import importlib.util
import pathlib
import unittest


MODULE = pathlib.Path(__file__).parents[1] / "src" / "can" / "python_can.py"
SPEC = importlib.util.spec_from_file_location("eha_python_can_bridge", MODULE)
BRIDGE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(BRIDGE)


class FakeUtil:
    file_config = {}
    environment_config = {}

    @classmethod
    def load_file_config(cls, section):
        del section
        return cls.file_config

    @classmethod
    def load_environment_config(cls, context):
        del context
        return cls.environment_config


class FakeCan:
    util = FakeUtil

    class BitTimingFd:
        def __init__(self, **kwargs):
            self.kwargs = kwargs

    @staticmethod
    def Bus(**kwargs):
        return kwargs


class FakeMessage:
    is_error_frame = False
    arbitration_id = 0x18FF50E5
    is_extended_id = True
    is_fd = True
    is_remote_frame = False
    bitrate_switch = True
    error_state_indicator = False
    dlc = 12
    data = bytes(range(12))


class PythonCanBridgeTests(unittest.TestCase):
    def setUp(self):
        FakeUtil.file_config = {}
        FakeUtil.environment_config = {}

    def test_missing_named_selection_cannot_fall_back(self):
        with self.assertRaisesRegex(ValueError, "interface 和 channel"):
            BRIDGE.configured_bus(FakeCan, "EHA_MISSING")

    def test_explicit_context_is_passed_to_upstream_bus(self):
        FakeUtil.file_config = {"interface": "virtual", "channel": "file-channel"}
        FakeUtil.environment_config = {"channel": 7}
        self.assertEqual(
            BRIDGE.configured_bus(FakeCan, "EHA"),
            {"config_context": "EHA", "interface": "virtual", "channel": 7},
        )

    def test_explicit_bus_ignores_user_defaults_and_preserves_fd_timing(self):
        FakeUtil.file_config = {"interface": "virtual", "channel": "wrong"}
        config = {
            "interface": "slcan",
            "channel": "/dev/cu.usbmodem-stable",
            "fd": True,
            "bitrate": 1_000_000,
            "data_bitrate": 2_000_000,
            "timing": {"f_clock": 80_000_000, "nom_brp": 1, "nom_tseg1": 63, "nom_tseg2": 16, "nom_sjw": 16, "data_brp": 1, "data_tseg1": 31, "data_tseg2": 8, "data_sjw": 8},
        }
        bus = BRIDGE.explicit_bus(FakeCan, BRIDGE.json.dumps(config))
        self.assertTrue(bus["ignore_config"])
        self.assertEqual(bus["interface"], "slcan")
        self.assertEqual(bus["channel"], "/dev/cu.usbmodem-stable")
        self.assertNotIn("bitrate", bus)
        self.assertNotIn("data_bitrate", bus)
        timing = bus["timing"].kwargs
        self.assertEqual(timing["f_clock"], 80_000_000)
        self.assertEqual(timing["nom_brp"], 1)
        self.assertEqual(timing["data_tseg2"], 8)

    def test_explicit_fd_timing_constructs_real_python_can_object(self):
        try:
            import can
        except ImportError:
            self.skipTest("真实 python-can 仅在受管运行时测试")

        class RealTimingCan:
            BitTimingFd = can.BitTimingFd

            @staticmethod
            def Bus(**kwargs):
                return kwargs

        config = {"interface": "slcan", "channel": "/dev/not-opened", "timing": {
            "f_clock": 80_000_000, "nom_brp": 1, "nom_tseg1": 63, "nom_tseg2": 16, "nom_sjw": 16,
            "data_brp": 1, "data_tseg1": 31, "data_tseg2": 8, "data_sjw": 8,
        }}
        timing = BRIDGE.explicit_bus(RealTimingCan, BRIDGE.json.dumps(config))["timing"]
        self.assertIsInstance(timing, can.BitTimingFd)
        self.assertEqual(timing.nom_bitrate, 1_000_000)
        self.assertEqual(timing.data_bitrate, 2_000_000)

    def test_fd_byte_length_is_converted_to_wire_dlc(self):
        frame = BRIDGE.frame_to_wire(FakeMessage())
        self.assertEqual(frame["dlc"], 9)
        self.assertEqual(frame["data"], list(range(12)))

    def test_driver_wrapper_preserves_underlying_io_failure(self):
        try:
            try:
                raise TimeoutError("write deadline expired")
            except TimeoutError as cause:
                raise RuntimeError("driver submission failed") from cause
        except RuntimeError as error:
            self.assertIn("TimeoutError: write deadline expired", BRIDGE.error_detail(error))


if __name__ == "__main__":
    unittest.main()
