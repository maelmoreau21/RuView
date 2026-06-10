#!/usr/bin/env python3
"""
RuvSense Edge stable health monitoring bootstrap.

Loads only stable runtime modules, then polls respiration and fall/non-movement
signals from /api/v1/vital-signs into logs/.
"""

import datetime
import json
import os
import sys
import time

import requests


SCRIPT_DIR = os.path.dirname(os.path.abspath(__file__))
REPO_ROOT = os.path.abspath(os.path.join(SCRIPT_DIR, ".."))
DEFAULT_CONFIG_PATH = os.path.join(SCRIPT_DIR, "health_alert_config.json")
DEFAULT_LOG_DIR = os.path.join(REPO_ROOT, "logs")

DEFAULT_CONFIG = {
    "apnea_threshold_seconds": 20,
    "breathing_min_bpm": 4,
    "breathing_max_bpm": 40,
    "master_ip": "127.0.0.1",
    "master_port": 3000,
}

MODULE_LOAD_PLAN = [
    {
        "requested_id": "coherence-gate",
        "priority": 0,
        "runtime_ids": ["coherence_gate", "coherence-gate"],
    },
    {
        "requested_id": "health-monitor",
        "priority": 1,
        "runtime_ids": ["health_monitor", "respiration_tracking", "fall_detection"],
    },
    {
        "requested_id": "sleep-apnea",
        "priority": 2,
        "runtime_ids": ["sleep_apnea", "sleep_apnea_screening"],
    },
]


def utc_now():
    return (
        datetime.datetime.now(datetime.timezone.utc)
        .replace(microsecond=0)
        .isoformat()
        .replace("+00:00", "Z")
    )


def color(text, code):
    return "\033[" + code + "m" + text + "\033[0m"


def red(text):
    return color(text, "1;31")


def yellow(text):
    return color(text, "33")


def log_info(message):
    print("[health-monitor] " + message, flush=True)


def log_warning(message):
    print(yellow("[health-monitor] WARNING: " + message), flush=True)


def alert(message):
    print("\a" + red("[health-monitor] ALERT: " + message), flush=True)


def load_config():
    path = os.environ.get("HEALTH_ALERT_CONFIG", DEFAULT_CONFIG_PATH)
    config = dict(DEFAULT_CONFIG)

    if os.path.exists(path):
        with open(path, "r", encoding="utf-8-sig") as f:
            loaded = json.load(f)
        config.update(loaded)
    elif os.environ.get("HEALTH_ALERT_CONFIG"):
        raise FileNotFoundError(path)
    else:
        log_warning("config file not found; using built-in defaults: " + path)

    if os.environ.get("RUVSENSE_MASTER_IP"):
        config["master_ip"] = os.environ["RUVSENSE_MASTER_IP"]
    if os.environ.get("RUVSENSE_MASTER_PORT"):
        config["master_port"] = int(os.environ["RUVSENSE_MASTER_PORT"])
    return config


def base_url(config):
    return "http://{}:{}".format(config["master_ip"], int(config["master_port"]))


def auth_headers():
    token = os.environ.get("RUVIEW_API_TOKEN", "")
    if token:
        return {"Authorization": "Bearer " + token}
    return {}


def request_json(method, url, headers, payload=None, timeout=5):
    response = requests.request(method, url, json=payload, headers=headers, timeout=timeout)
    response.raise_for_status()
    if response.text:
        return response.json()
    return {}


def wait_for_server_ready(url, headers, timeout_seconds=120):
    deadline = time.time() + timeout_seconds
    last_payload = None
    last_error = None
    log_info("waiting for " + url + "/health/ready")

    while time.time() < deadline:
        try:
            response = requests.get(url + "/health/ready", headers=headers, timeout=5)
            try:
                last_payload = response.json()
            except ValueError:
                last_payload = {"status_code": response.status_code, "body": response.text[:200]}

            if response.status_code == 200:
                log_info("server ready: " + json.dumps(last_payload, sort_keys=True))
                return True

            last_error = "HTTP {} {}".format(response.status_code, last_payload)
        except requests.RequestException as exc:
            last_error = str(exc)

        time.sleep(2)

    if last_payload and last_payload.get("status") == "not_ready":
        log_warning(
            "server responded but did not reach ready quorum within {}s; continuing so topology can report missing nodes".format(
                timeout_seconds
            )
        )
        return False

    raise RuntimeError("server did not become reachable within {}s: {}".format(timeout_seconds, last_error))


def is_number(value):
    return isinstance(value, (int, float)) and not isinstance(value, bool)


def is_live_esp32_c6_node(node):
    kind = str(node.get("kind", "")).lower().replace("-", "_")
    is_c6 = "esp32" in kind and "c6" in kind
    live = node.get("active") is True or node.get("status") == "live" or node.get("health_status") == "live"
    last_csi_ms = node.get("last_csi_ms")
    frame_rate_hz = node.get("frame_rate_hz")
    has_recent_csi = is_number(last_csi_ms) and last_csi_ms <= 15000
    has_frame_rate = is_number(frame_rate_hz) and frame_rate_hz > 0
    return is_c6 and live and (has_recent_csi or has_frame_rate)


def check_topology(url, headers):
    topology = request_json("GET", url + "/api/v1/topology", headers)
    nodes = topology.get("nodes", [])
    live_nodes = [node for node in nodes if isinstance(node, dict) and is_live_esp32_c6_node(node)]
    if live_nodes:
        ids = [str(node.get("node_id", "?")) for node in live_nodes]
        log_info("live ESP32-C6 CSI nodes: " + ", ".join(ids))
    else:
        log_warning("no live ESP32-C6 CSI node found in /api/v1/topology; continuing")
    return topology


def module_map(catalog):
    modules = catalog.get("modules", [])
    result = {}
    for module in modules:
        module_id = module.get("id") if isinstance(module, dict) else None
        if module_id:
            result[module_id] = module
    return result


def fetch_modules(url, headers):
    return request_json("GET", url + "/api/v1/modules", headers)


def enable_runtime_module(url, headers, runtime_id):
    return request_json(
        "PUT",
        url + "/api/v1/modules/" + runtime_id + "/enabled",
        headers,
        payload={"enabled": True},
    )


def pick_runtime_ids(requested, available_ids):
    selected = []
    for runtime_id in requested["runtime_ids"]:
        if runtime_id in available_ids and runtime_id not in selected:
            selected.append(runtime_id)
    return selected


def load_modules(url, headers):
    catalog = fetch_modules(url, headers)
    available = module_map(catalog)

    for module in MODULE_LOAD_PLAN:
        runtime_ids = pick_runtime_ids(module, available)
        if not runtime_ids:
            log_warning(
                "module {} priority {} has no runtime equivalent in /api/v1/modules".format(
                    module["requested_id"], module["priority"]
                )
            )
            continue

        for runtime_id in runtime_ids:
            try:
                response = enable_runtime_module(url, headers, runtime_id)
                status = response.get("module", {}).get("status", "unknown")
                enabled = response.get("module", {}).get("enabled")
                log_info(
                    "loaded {} priority {} as {}: enabled={} status={}".format(
                        module["requested_id"], module["priority"], runtime_id, enabled, status
                    )
                )
            except requests.RequestException as exc:
                log_warning("failed to load {} as {}: {}".format(module["requested_id"], runtime_id, exc))


def nested_vitals(payload):
    vitals = payload.get("vital_signs")
    if isinstance(vitals, dict):
        return vitals
    return payload


def first_number(source, keys):
    for key in keys:
        value = source.get(key)
        if is_number(value):
            return float(value)
    return None


def first_bool(payload, vitals, keys):
    for source in (vitals, payload):
        for key in keys:
            value = source.get(key)
            if isinstance(value, bool):
                return value
    return False


def open_log_file():
    os.makedirs(os.environ.get("HEALTH_LOG_DIR", DEFAULT_LOG_DIR), exist_ok=True)
    log_dir = os.environ.get("HEALTH_LOG_DIR", DEFAULT_LOG_DIR)
    date = datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%d")
    return open(os.path.join(log_dir, "health_" + date + ".jsonl"), "a", encoding="utf-8")


def write_health_log(handle, payload):
    record = {
        "timestamp": utc_now(),
        "data": payload,
    }
    handle.write(json.dumps(record, sort_keys=True) + "\n")
    handle.flush()


def monitor_vitals(url, headers, config):
    low_breathing_started_at = None
    low_breathing_alert_active = False
    once = os.environ.get("HEALTH_MONITOR_ONCE", "") == "1"

    with open_log_file() as log_file:
        while True:
            try:
                payload = request_json("GET", url + "/api/v1/vital-signs", headers)
                write_health_log(log_file, payload)
                vitals = nested_vitals(payload)
                breathing = first_number(
                    vitals,
                    ["breathing_bpm", "breathing_rate", "breathing_rate_bpm", "respiration_rate_bpm"],
                )
                fall_detected = first_bool(payload, vitals, ["fall_suspected", "fall_detected", "fall"])
                non_movement = first_bool(
                    payload,
                    vitals,
                    ["non_movement_prolonged", "prolonged_non_movement", "immobility_detected"],
                )

                if breathing is not None and breathing < float(config["breathing_min_bpm"]):
                    if low_breathing_started_at is None:
                        low_breathing_started_at = time.time()
                    low_breathing_seconds = time.time() - low_breathing_started_at
                    if (
                        low_breathing_seconds > float(config["apnea_threshold_seconds"])
                        and not low_breathing_alert_active
                    ):
                        low_breathing_alert_active = True
                        alert(
                            "low/paused breathing threshold exceeded: breathing_rate={:.1f} bpm for {:.0f}s".format(
                                breathing, low_breathing_seconds
                            )
                        )
                else:
                    low_breathing_started_at = None
                    low_breathing_alert_active = False

                if breathing is not None and breathing > float(config["breathing_max_bpm"]):
                    alert("breathing anomaly: breathing_rate={:.1f} bpm".format(breathing))

                if fall_detected:
                    alert("fall detected")
                if non_movement:
                    alert("prolonged non-movement detected")

                log_info(
                    "vitals breathing={} fall_detected={} non_movement={}".format(
                        breathing, fall_detected, non_movement
                    )
                )
            except requests.RequestException as exc:
                write_health_log(log_file, {"error": str(exc)})
                log_warning("vital-sign poll failed: " + str(exc))

            if once:
                return
            time.sleep(5)


def main():
    try:
        config = load_config()
        url = base_url(config)
        headers = auth_headers()
        wait_for_server_ready(url, headers)
        check_topology(url, headers)
        load_modules(url, headers)
        monitor_vitals(url, headers, config)
    except KeyboardInterrupt:
        log_info("stopped")
    except Exception as exc:
        print(red("[health-monitor] ERROR: " + str(exc)), file=sys.stderr, flush=True)
        sys.exit(1)


if __name__ == "__main__":
    main()
