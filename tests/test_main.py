import unittest

from linux_airpods_cli.aap import parse_battery_packet, parse_magic_keys_packet, parse_metadata_packet
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

    def test_parse_aap_battery_packet(self) -> None:
        packet = bytes.fromhex('04000400040003020160020104016202010801000401')
        battery = parse_battery_packet(packet)
        assert battery is not None
        self.assertEqual(battery.right.level, 96)
        self.assertEqual(battery.left.level, 98)
        self.assertTrue(battery.left.available)
        self.assertFalse(battery.case.available)
        self.assertEqual(battery.primary, 'right')
        self.assertEqual(battery.secondary, 'left')

    def test_parse_aap_metadata_packet(self) -> None:
        packet = bytes.fromhex(
            '040004001d0002ed000400'
            '416972506f64732050726f203300'
            '413330363300'
            '4170706c6520496e632e00'
        )
        metadata = parse_metadata_packet(packet)
        assert metadata is not None
        self.assertEqual(metadata.device_name, 'AirPods Pro 3')
        self.assertEqual(metadata.model_number, 'A3063')
        self.assertEqual(metadata.manufacturer, 'Apple Inc.')

    def test_parse_magic_keys_packet(self) -> None:
        packet = bytes.fromhex(
            '0400040031000201'
            '001000b0b6db71ab06f97626b7715fad262204'
            '0400100030f3071127b81b2cb42a809fafe9d1f8'
        )
        keys = parse_magic_keys_packet(packet)
        assert keys is not None
        self.assertEqual(keys.irk.hex(), 'b0b6db71ab06f97626b7715fad262204')
        self.assertEqual(keys.enc_key.hex(), '30f3071127b81b2cb42a809fafe9d1f8')


if __name__ == "__main__":
    unittest.main()
