#!/usr/bin/env python3
"""Provision a RuvSense Edge ESP32-C6 fleet for one master.

This wrapper keeps the authoritative NVS writer in provision.py and only
assigns fleet-safe defaults: node_id=1..N, tdm_slot=0..N-1, tdm_total=N.
It accepts one node for a small lab or up to 100 nodes for a larger fleet.
"""

import argparse
import ipaddress
import subprocess
import sys
from pathlib import Path


def parse_csv(value: str) -> list[str]:
    return [part.strip() for part in value.split(",") if part.strip()]


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Provision 1..100 ESP32-C6 nodes for a RuvSense Edge master.",
        epilog=(
            "Examples:\n"
            "  python provision_three_c6.py --ports COM12 "
            "--ssid LabWiFi --password secret --target-ip 192.168.1.20\n"
            "  python provision_three_c6.py --ports COM12,COM13,COM14 "
            "--ssid LabWiFi --password secret --target-ip 192.168.1.20"
        ),
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument("--ports", required=True, help="Comma-separated serial ports for the C6 nodes")
    parser.add_argument("--ssid", required=True, help="2.4 GHz WiFi SSID")
    parser.add_argument("--password", required=True, help="WiFi password")
    parser.add_argument("--target-ip", required=True, help="Raspberry Pi / ruvsense-master IP")
    parser.add_argument("--target-port", type=int, default=5005, help="Master UDP port (default: 5005)")
    parser.add_argument("--zones", default="", help="Comma-separated zones, or one zone reused for all nodes")
    parser.add_argument("--baud", type=int, default=460800, help="Flash baud rate")
    parser.add_argument("--chip", default="esp32c6", help="esptool chip target (default: esp32c6)")
    parser.add_argument("--edge-tier", type=int, choices=[0, 1, 2], default=2, help="Edge tier, default 2=vitals")
    parser.add_argument("--channel", type=int, help="Optional fixed WiFi channel")
    parser.add_argument("--hop-channels", help="Optional comma-separated hopping channels")
    parser.add_argument("--hop-dwell", type=int, default=200, help="Hopping dwell in ms")
    parser.add_argument("--state-dir", help="Optional shared provision.py state directory")
    parser.add_argument("--reset", action="store_true", help="Reset per-port state before provisioning")
    parser.add_argument("--dry-run", action="store_true", help="Generate NVS blobs without flashing")
    return parser


def resolve_zones(raw: str, count: int) -> list[str]:
    if not raw:
        return [f"zone-{index + 1}" for index in range(count)]

    zones = parse_csv(raw)
    if len(zones) == 1:
        return zones * count
    if len(zones) != count:
        raise ValueError(f"--zones must contain 1 or {count} entries, got {len(zones)}")
    return zones


def main() -> int:
    parser = build_parser()
    args = parser.parse_args()

    ports = parse_csv(args.ports)
    if not ports:
        parser.error("--ports must contain at least one ESP32-C6 serial port")
    if len(ports) > 100:
        parser.error("RuvSense Edge fleet provisioning supports up to 100 ESP32-C6 ports at once")
    if len(set(ports)) != len(ports):
        parser.error("--ports contains duplicates")
    if not (1 <= args.target_port <= 65535):
        parser.error("--target-port must be between 1 and 65535")

    try:
        ipaddress.ip_address(args.target_ip)
        zones = resolve_zones(args.zones, len(ports))
    except ValueError as exc:
        parser.error(str(exc))

    provisioner = Path(__file__).with_name("provision.py")
    tdm_total = len(ports)

    print(f"Provisioning {tdm_total} ESP32-C6 node(s) for RuvSense Edge master {args.target_ip}:{args.target_port}")
    for index, port in enumerate(ports):
        node_id = index + 1
        cmd = [
            sys.executable,
            str(provisioner),
            "--port",
            port,
            "--chip",
            args.chip,
            "--baud",
            str(args.baud),
            "--ssid",
            args.ssid,
            "--password",
            args.password,
            "--target-ip",
            args.target_ip,
            "--target-port",
            str(args.target_port),
            "--node-id",
            str(node_id),
            "--tdm-slot",
            str(index),
            "--tdm-total",
            str(tdm_total),
            "--edge-tier",
            str(args.edge_tier),
            "--zone",
            zones[index],
        ]
        if args.channel is not None:
            cmd.extend(["--channel", str(args.channel)])
        if args.hop_channels:
            cmd.extend(["--hop-channels", args.hop_channels, "--hop-dwell", str(args.hop_dwell)])
        if args.state_dir:
            cmd.extend(["--state-dir", args.state_dir])
        if args.reset:
            cmd.append("--reset")
        if args.dry_run:
            cmd.append("--dry-run")

        print(f"\n[{node_id}/{tdm_total}] {port}: node_id={node_id}, tdm_slot={index}, zone={zones[index]}")
        subprocess.run(cmd, check=True)

    print("\nFleet provisioning complete.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
