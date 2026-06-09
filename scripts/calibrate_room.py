#!/usr/bin/env python3
"""
Interactive RuvSense room calibration helper.

Uses the master REST API to recalibrate when the target room is empty, or to
raise the presence threshold when the room is occupied and false positives are
expected from furniture, electronics, or occasional motion outside the zone.
"""

import argparse
import os
import sys
import time

import requests


DEFAULT_DURATION_SECONDS = 30
OCCUPIED_ROOM_MULTIPLIER = 1.4


def auth_headers():
    token = os.environ.get("RUVIEW_API_TOKEN", "")
    if token:
        return {"Authorization": "Bearer " + token}
    return {}


def request_json(method, url, headers, payload=None, timeout=10, required=True):
    try:
        response = requests.request(method, url, json=payload, headers=headers, timeout=timeout)
        response.raise_for_status()
    except requests.RequestException as exc:
        if required:
            raise RuntimeError("{} {} failed: {}".format(method, url, exc)) from exc
        return {}

    if not response.text:
        return {}
    try:
        return response.json()
    except ValueError:
        return {"body": response.text}


def prompt_room_empty():
    while True:
        answer = input("La pièce est-elle vide maintenant ? (o/n) ").strip().lower()
        if answer in ("o", "oui", "y", "yes"):
            return True
        if answer in ("n", "non", "no"):
            return False
        print("Répondez par o ou n.", flush=True)


def countdown(seconds):
    for remaining in range(seconds, 0, -1):
        print("Recalibration en cours: {:02d}s restantes".format(remaining), end="\r", flush=True)
        time.sleep(1)
    print("Recalibration en cours: 00s restantes")


def nested_get(payload, keys):
    current = payload
    for key in keys:
        if not isinstance(current, dict) or key not in current:
            return None
        current = current[key]
    return current


def first_number(payload, paths):
    for path in paths:
        value = nested_get(payload, path)
        if isinstance(value, (int, float)) and not isinstance(value, bool):
            return float(value)
    return None


def first_bool(payload, paths):
    for path in paths:
        value = nested_get(payload, path)
        if isinstance(value, bool):
            return value
    return None


def current_threshold(url, headers, fallback_payload=None):
    payloads = []
    if isinstance(fallback_payload, dict):
        payloads.append(fallback_payload)
    payloads.append(request_json("GET", url + "/api/v1/config/presence-threshold", headers, required=False))
    payloads.append(request_json("GET", url + "/api/v1/config", headers, required=False))

    paths = [
        ("presence_threshold",),
        ("threshold",),
        ("value",),
        ("config", "presence_threshold"),
        ("presence", "threshold"),
        ("presence_threshold", "value"),
    ]
    for payload in payloads:
        value = first_number(payload, paths)
        if value is not None:
            return value
    return None


def recalibrate_empty_room(url, headers, duration_seconds):
    payload = {"recalibrate": True, "duration_seconds": duration_seconds}
    result = request_json("POST", url + "/api/v1/config", headers, payload=payload, timeout=15)
    countdown(duration_seconds)
    return result


def apply_occupied_room_mode(url, headers):
    current = current_threshold(url, headers)
    payload = {
        "mode": "occupied_room",
        "multiplier": OCCUPIED_ROOM_MULTIPLIER,
    }
    if current is not None:
        payload["value"] = current * OCCUPIED_ROOM_MULTIPLIER

    result = request_json(
        "PATCH",
        url + "/api/v1/config/presence-threshold",
        headers,
        payload=payload,
        timeout=15,
    )
    return result


def normalize_nodes(topology):
    nodes = topology.get("nodes", [])
    if isinstance(nodes, dict):
        nodes = list(nodes.values())
    return [node for node in nodes if isinstance(node, dict)]


def node_label(node):
    for key in ("node_id", "id", "label", "device_id"):
        value = node.get(key)
        if value is not None:
            return str(value)
    return "unknown"


def node_rssi(node):
    direct = first_number(
        node,
        [
            ("mean_rssi",),
            ("avg_rssi",),
            ("rssi_avg",),
            ("rssi_dbm",),
            ("rssi",),
            ("metrics", "mean_rssi"),
            ("metrics", "rssi_dbm"),
        ],
    )
    if direct is not None:
        return direct

    history = node.get("rssi_history")
    if isinstance(history, list):
        values = [float(v) for v in history if isinstance(v, (int, float)) and not isinstance(v, bool)]
        if values:
            return sum(values) / len(values)
    return None


def estimated_false_positive_rate(was_empty, latest, config_payload):
    configured = first_number(
        config_payload,
        [
            ("false_positive_rate",),
            ("estimated_false_positive_rate",),
            ("presence", "false_positive_rate"),
            ("stats", "false_positive_rate"),
        ],
    )
    if configured is not None:
        return configured

    latest_rate = first_number(
        latest,
        [
            ("false_positive_rate",),
            ("estimated_false_positive_rate",),
            ("presence", "false_positive_rate"),
            ("stats", "false_positive_rate"),
        ],
    )
    if latest_rate is not None:
        return latest_rate

    if not was_empty:
        return None

    presence = first_bool(
        latest,
        [
            ("presence",),
            ("classification", "presence"),
            ("sensing", "presence"),
            ("latest", "presence"),
        ],
    )
    if presence is not None:
        return 1.0 if presence else 0.0
    return None


def print_stats(url, headers, was_empty, action_payload):
    config_payload = request_json("GET", url + "/api/v1/config", headers, required=False)
    threshold = current_threshold(url, headers, action_payload)
    topology = request_json("GET", url + "/api/v1/topology", headers, required=False)
    latest = request_json("GET", url + "/api/v1/sensing/latest", headers, required=False)
    false_positive_rate = estimated_false_positive_rate(was_empty, latest, config_payload)

    print()
    print("Stats après calibration")
    if threshold is None:
        print("- Seuil actuel: indisponible")
    else:
        print("- Seuil actuel: {:.3f}".format(threshold))

    nodes = normalize_nodes(topology)
    if not nodes:
        print("- RSSI moyen par nœud: indisponible")
    else:
        print("- RSSI moyen par nœud:")
        for node in nodes:
            rssi = node_rssi(node)
            if rssi is None:
                print("  - {}: indisponible".format(node_label(node)))
            else:
                print("  - {}: {:.1f} dBm".format(node_label(node), rssi))

    if false_positive_rate is None:
        print("- Taux de faux positifs estimé: indisponible")
    else:
        percent = false_positive_rate if false_positive_rate > 1.0 else false_positive_rate * 100.0
        print("- Taux de faux positifs estimé: {:.1f}%".format(percent))


def parse_args():
    parser = argparse.ArgumentParser(description="Calibrer une pièce RuvSense Edge.")
    parser.add_argument(
        "--master-ip",
        default=os.environ.get("RUVSENSE_MASTER_IP", "127.0.0.1"),
        help="Adresse du master RuvSense (défaut: RUVSENSE_MASTER_IP ou 127.0.0.1).",
    )
    parser.add_argument(
        "--master-port",
        type=int,
        default=int(os.environ.get("RUVSENSE_MASTER_PORT", "3000")),
        help="Port HTTP du master RuvSense (défaut: RUVSENSE_MASTER_PORT ou 3000).",
    )
    parser.add_argument(
        "--duration-seconds",
        type=int,
        default=DEFAULT_DURATION_SECONDS,
        help="Durée de recalibration quand la pièce est vide (défaut: 30).",
    )
    return parser.parse_args()


def main():
    args = parse_args()
    url = "http://{}:{}".format(args.master_ip, args.master_port)
    headers = auth_headers()
    was_empty = prompt_room_empty()

    try:
        if was_empty:
            print("Recalibration pièce vide demandée sur {}.".format(url), flush=True)
            payload = recalibrate_empty_room(url, headers, args.duration_seconds)
        else:
            print("Mode pièce occupée: augmentation du seuil de présence de 40%.", flush=True)
            payload = apply_occupied_room_mode(url, headers)

        print_stats(url, headers, was_empty, payload)
    except RuntimeError as exc:
        print("ERROR: " + str(exc), file=sys.stderr)
        return 1

    return 0


if __name__ == "__main__":
    sys.exit(main())
