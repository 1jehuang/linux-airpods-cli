import unittest

from linux_airpods_cli.main import (
    Device,
    airpods_like_name,
    parse_bluetooth_devices,
    parse_bluetooth_info,
    parse_short_cards,
    parse_short_sinks,
)


class ParsingTests(unittest.TestCase):
    def test_parse_bluetooth_devices(self) -> None:
        output = """Device 74:77:86:57:67:2A AirPods Pro 3\nDevice 28:34:FF:27:A6:B2 iPhone\n"""
        devices = parse_bluetooth_devices(output)
        self.assertEqual(devices, [
            Device(mac="74:77:86:57:67:2A", name="AirPods Pro 3"),
            Device(mac="28:34:FF:27:A6:B2", name="iPhone"),
        ])

    def test_parse_bluetooth_info(self) -> None:
        output = """Name: AirPods Pro 3\nConnected: yes\nPaired: yes\nTrusted: no\nBREDR.Connected: yes\n"""
        info = parse_bluetooth_info(output)
        self.assertEqual(info["Name"], "AirPods Pro 3")
        self.assertEqual(info["Connected"], "yes")
        self.assertEqual(info["BREDR.Connected"], "yes")

    def test_parse_short_sinks(self) -> None:
        output = """63 alsa_output.pci-0000_00_1f.3-platform-sof_sdw.HiFi__Speaker__sink PipeWire s32le 2ch 48000Hz SUSPENDED\n105 bluez_output.74:77:86:57:67:2A PipeWire float32le 2ch 48000Hz RUNNING\n"""
        sinks = parse_short_sinks(output)
        self.assertEqual(sinks[1].name, "bluez_output.74:77:86:57:67:2A")
        self.assertEqual(sinks[1].state, "RUNNING")

    def test_parse_short_cards(self) -> None:
        output = """52 alsa_card.pci-0000_00_1f.3-platform-sof_sdw alsa\n94 bluez_card.74_77_86_57_67_2A module-bluez5-device.c\n"""
        cards = parse_short_cards(output)
        self.assertEqual(cards[1].name, "bluez_card.74_77_86_57_67_2A")

    def test_airpods_name_detection(self) -> None:
        self.assertTrue(airpods_like_name("AirPods Pro 3"))
        self.assertTrue(airpods_like_name("Beats Studio Buds"))
        self.assertFalse(airpods_like_name("iPhone"))


if __name__ == "__main__":
    unittest.main()
