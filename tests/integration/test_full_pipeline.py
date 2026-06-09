import json
import os
import queue
import socket
import subprocess
import sys
import textwrap
import threading
import time
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

import pytest
import requests


REPO_ROOT = Path(__file__).resolve().parents[2]
COMPOSE_FILE = REPO_ROOT / "docker" / "compose.yml"
MONITOR_SCRIPT = REPO_ROOT / "scripts" / "setup_health_monitoring.py"
REQUEST_TIMEOUT = 5
READY_TIMEOUT_SECONDS = 180
VITALS_TIMEOUT_SECONDS = 90


def compact(value, limit=500):
    text = str(value).replace("\r", "").strip()
    if len(text) <= limit:
        return text
    return text[: limit - 3] + "..."


class PipelineReport:
    def __init__(self):
        self.results = []

    def record(self, name, passed, detail=""):
        self.results.append((name, passed, compact(detail)))

    def print(self):
        print("\nRuvSense full pipeline integration report")
        print("=" * 43)
        for name, passed, detail in self.results:
            status = "PASS" if passed else "FAIL"
            suffix = f" - {detail}" if detail else ""
            print(f"{status:4} {name}{suffix}")

    def assert_all_passed(self):
        failures = [f"{name}: {detail}" for name, passed, detail in self.results if not passed]
        assert not failures, "Full pipeline failures:\n" + "\n".join(failures)


def free_port():
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def to_compose_path(path):
    return path.resolve().as_posix()


def compose_env(http_port, ws_port, udp_port):
    env = os.environ.copy()
    env.update(
        {
            "CSI_SOURCE": "simulate",
            "RUVSENSE_ENABLE_SIMULATION": "true",
            "RUVSENSE_HTTP_PORT": str(http_port),
            "RUVSENSE_WS_PORT": str(ws_port),
            "RUVSENSE_UDP_PORT": str(udp_port),
            "RUVSENSE_MIN_NODES": "1",
            "RUVIEW_API_TOKEN": "",
            "SENSING_ALLOWED_HOSTS": "127.0.0.1,localhost",
        }
    )
    return env


def write_compose_override(tmp_path, suffix):
    data_dir = tmp_path / "ruvsense-data"
    data_dir.mkdir(parents=True, exist_ok=True)
    override = tmp_path / "compose.integration.override.yml"
    override.write_text(
        textwrap.dedent(
            f"""
            name: ruvsense-edge-it-{suffix}

            services:
              ruvsense-master:
                container_name: ruvsense-master-it-{suffix}
                volumes:
                  - "{to_compose_path(data_dir)}:/var/lib/ruvsense"
            """
        ).strip()
        + "\n",
        encoding="utf-8",
    )
    return override


def docker_compose(override_file, env, *args, timeout=300, check=True):
    cmd = [
        "docker",
        "compose",
        "-f",
        str(COMPOSE_FILE),
        "-f",
        str(override_file),
        *args,
    ]
    completed = subprocess.run(
        cmd,
        cwd=REPO_ROOT,
        env=env,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        timeout=timeout,
    )
    if check and completed.returncode != 0:
        raise AssertionError(
            "Command failed with exit code {}:\n{}\n{}".format(
                completed.returncode, " ".join(cmd), completed.stdout
            )
        )
    return completed


def request_json(base_url, path):
    response = requests.get(base_url + path, timeout=REQUEST_TIMEOUT)
    response.raise_for_status()
    return response.json()


def wait_for_ready(base_url, timeout_seconds=READY_TIMEOUT_SECONDS):
    deadline = time.monotonic() + timeout_seconds
    last_error = None

    while time.monotonic() < deadline:
        try:
            response = requests.get(base_url + "/health/ready", timeout=REQUEST_TIMEOUT)
            if response.status_code == 200:
                return response.json()
            try:
                last_error = f"HTTP {response.status_code}: {response.json()}"
            except ValueError:
                last_error = f"HTTP {response.status_code}: {response.text[:200]}"
        except requests.RequestException as exc:
            last_error = str(exc)
        time.sleep(2)

    raise AssertionError(f"/health/ready did not return 200: {last_error}")


def nested_vitals(payload):
    vitals = payload.get("vital_signs")
    return vitals if isinstance(vitals, dict) else payload


def extract_number(payload, *keys):
    for key in keys:
        value = payload.get(key)
        if isinstance(value, (int, float)) and not isinstance(value, bool):
            return float(value)
    return None


def assert_vitals_have_breathing(base_url, timeout_seconds=VITALS_TIMEOUT_SECONDS):
    deadline = time.monotonic() + timeout_seconds
    last_payload = None
    last_breathing = None

    while time.monotonic() < deadline:
        payload = request_json(base_url, "/api/v1/vital-signs")
        last_payload = payload
        vitals = nested_vitals(payload)
        breathing = extract_number(
            vitals,
            "breathing_rate",
            "breathing_rate_bpm",
            "respiration_rate_bpm",
        )
        last_breathing = breathing
        if breathing is not None and breathing > 0:
            return breathing
        time.sleep(2)

    assert last_breathing is not None, f"missing breathing rate in {last_payload}"
    raise AssertionError(f"expected breathing rate > 0, got {last_breathing}")


def assert_location_json(base_url):
    payload = request_json(base_url, "/api/v1/location")
    json.dumps(payload)
    assert isinstance(payload, dict), f"location response must be an object, got {type(payload)}"
    return payload


def assert_simulated_topology(base_url):
    payload = request_json(base_url, "/api/v1/topology")
    source = str(payload.get("source", "")).lower()
    assert "simulat" in source, f"topology source is not simulation: {payload}"
    nodes = payload.get("nodes")
    assert isinstance(nodes, list), f"topology nodes must be a list: {payload}"
    simulated_nodes = [
        node
        for node in nodes
        if isinstance(node, dict)
        and (
            node.get("active") is True
            or str(node.get("status", "")).lower() in {"live", "active"}
            or str(node.get("source", "")).lower() in {"simulate", "simulated"}
        )
    ]
    assert simulated_nodes, f"expected at least one simulated/active node: {payload}"
    return simulated_nodes


def run_monitor_once(http_port, tmp_path):
    env = os.environ.copy()
    env.update(
        {
            "HEALTH_MONITOR_ONCE": "1",
            "HEALTH_LOG_DIR": str(tmp_path / "health-logs"),
            "RUVSENSE_MASTER_IP": "127.0.0.1",
            "RUVSENSE_MASTER_PORT": str(http_port),
            "RUVIEW_API_TOKEN": "",
        }
    )
    completed = subprocess.run(
        [sys.executable, str(MONITOR_SCRIPT)],
        cwd=REPO_ROOT,
        env=env,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        timeout=90,
    )
    output = completed.stdout
    assert completed.returncode == 0, output
    assert "loaded health-monitor" in output, output
    assert "loaded sleep-apnea" in output, output
    assert "verified health-monitor" in output, output
    assert "vitals breathing=" in output, output
    return output


class ApneaMockHandler(BaseHTTPRequestHandler):
    server_version = "RuvSenseApneaMock/1.0"

    modules = [
        "respiration_tracking",
        "fall_detection",
        "sleep_apnea_screening",
        "cardiac_arrhythmia",
    ]

    def _send_json(self, status, payload):
        body = json.dumps(payload).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, _format, *_args):
        return

    def do_GET(self):
        if self.path == "/health/ready":
            self._send_json(200, {"status": "ready", "active_nodes": 1, "min_nodes": 1})
        elif self.path == "/api/v1/topology":
            self._send_json(
                200,
                {
                    "nodes": [
                        {
                            "node_id": 1,
                            "kind": "esp32_c6",
                            "status": "live",
                            "active": True,
                            "last_csi_ms": 10,
                            "frame_rate_hz": 10.0,
                        }
                    ]
                },
            )
        elif self.path == "/api/v1/modules":
            self._send_json(
                200,
                {
                    "modules": [
                        {
                            "id": module_id,
                            "enabled": False,
                            "status": "disabled",
                            "capability_status": "ready",
                        }
                        for module_id in self.modules
                    ]
                },
            )
        elif self.path == "/api/v1/vital-signs":
            self._send_json(
                200,
                {
                    "vital_signs": {
                        "breathing_rate": 0.0,
                        "breathing_rate_bpm": 0.0,
                        "heart_rate_bpm": 70.0,
                        "fall_detected": False,
                    }
                },
            )
        else:
            self._send_json(404, {"error": "not_found", "path": self.path})

    def do_PUT(self):
        prefix = "/api/v1/modules/"
        suffix = "/enabled"
        if self.path.startswith(prefix) and self.path.endswith(suffix):
            module_id = self.path[len(prefix) : -len(suffix)]
            self._send_json(
                200,
                {
                    "status": "ok",
                    "module": {
                        "id": module_id,
                        "enabled": True,
                        "status": "ready",
                        "capability_status": "ready",
                    },
                },
            )
        else:
            self._send_json(404, {"error": "not_found", "path": self.path})


def start_apnea_mock_server():
    server = ThreadingHTTPServer(("127.0.0.1", 0), ApneaMockHandler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    return server


def assert_apnea_alert_under_25_seconds(tmp_path):
    server = start_apnea_mock_server()
    port = server.server_address[1]
    config_path = tmp_path / "apnea_config.json"
    config_path.write_text(
        json.dumps(
            {
                "apnea_threshold_seconds": 2,
                "breathing_min_bpm": 4,
                "breathing_max_bpm": 40,
                "heartrate_min_bpm": 40,
                "heartrate_max_bpm": 150,
                "master_ip": "127.0.0.1",
                "master_port": port,
            }
        ),
        encoding="utf-8",
    )

    env = os.environ.copy()
    env.update(
        {
            "HEALTH_ALERT_CONFIG": str(config_path),
            "HEALTH_LOG_DIR": str(tmp_path / "apnea-logs"),
            "RUVSENSE_MASTER_IP": "127.0.0.1",
            "RUVSENSE_MASTER_PORT": str(port),
            "RUVIEW_API_TOKEN": "",
        }
    )

    output_queue = queue.Queue()
    started = time.monotonic()
    process = subprocess.Popen(
        [sys.executable, str(MONITOR_SCRIPT)],
        cwd=REPO_ROOT,
        env=env,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        bufsize=1,
    )

    def reader():
        assert process.stdout is not None
        for line in process.stdout:
            output_queue.put(line)

    reader_thread = threading.Thread(target=reader, daemon=True)
    reader_thread.start()
    lines = []
    alert_elapsed = None
    deadline = started + 25

    try:
        while time.monotonic() < deadline:
            try:
                line = output_queue.get(timeout=0.25)
                lines.append(line)
                if "ALERT:" in line and "apnea threshold exceeded" in line:
                    alert_elapsed = time.monotonic() - started
                    break
            except queue.Empty:
                if process.poll() is not None:
                    break
    finally:
        process.terminate()
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=5)
        server.shutdown()
        server.server_close()

    output = "".join(lines)
    assert alert_elapsed is not None, output
    assert alert_elapsed < 25, f"alert took {alert_elapsed:.2f}s\n{output}"
    return alert_elapsed


@pytest.mark.integration
def test_full_pipeline(tmp_path):
    report = PipelineReport()
    suffix = uuid.uuid4().hex[:8]
    http_port = int(os.environ.get("RUVSENSE_TEST_HTTP_PORT", free_port()))
    ws_port = int(os.environ.get("RUVSENSE_TEST_WS_PORT", free_port()))
    udp_port = int(os.environ.get("RUVSENSE_TEST_UDP_PORT", free_port()))
    base_url = f"http://127.0.0.1:{http_port}"
    env = compose_env(http_port, ws_port, udp_port)
    override_file = write_compose_override(tmp_path, suffix)
    stack_started = False

    try:
        try:
            result = docker_compose(
                override_file,
                env,
                "up",
                "-d",
                "--build",
                "--force-recreate",
                "ruvsense-master",
                timeout=900,
            )
            stack_started = True
            report.record("launch Docker stack in simulation mode", True, result.stdout)
        except Exception as exc:
            report.record("launch Docker stack in simulation mode", False, str(exc))

        try:
            ready = wait_for_ready(base_url)
            report.record("wait for /health/ready 200", True, json.dumps(ready, sort_keys=True))
        except Exception as exc:
            report.record("wait for /health/ready 200", False, str(exc))

        try:
            breathing = assert_vitals_have_breathing(base_url)
            report.record("GET /api/v1/vital-signs breathing_rate > 0", True, f"{breathing:.2f}")
        except Exception as exc:
            report.record("GET /api/v1/vital-signs breathing_rate > 0", False, str(exc))

        try:
            location = assert_location_json(base_url)
            report.record("GET /api/v1/location returns valid JSON", True, json.dumps(location)[:160])
        except Exception as exc:
            report.record("GET /api/v1/location returns valid JSON", False, str(exc))

        try:
            nodes = assert_simulated_topology(base_url)
            node_ids = [str(node.get("node_id", "?")) for node in nodes]
            report.record("GET /api/v1/topology has simulated node", True, ",".join(node_ids))
        except Exception as exc:
            report.record("GET /api/v1/topology has simulated node", False, str(exc))

        try:
            monitor_output = run_monitor_once(http_port, tmp_path)
            report.record("setup_health_monitoring.py loads modules", True, monitor_output[:240])
        except Exception as exc:
            report.record("setup_health_monitoring.py loads modules", False, str(exc))

        try:
            elapsed = assert_apnea_alert_under_25_seconds(tmp_path)
            report.record("mock apnea alert fires in < 25s", True, f"{elapsed:.2f}s")
        except Exception as exc:
            report.record("mock apnea alert fires in < 25s", False, str(exc))

    finally:
        if stack_started:
            docker_compose(
                override_file,
                env,
                "down",
                "--remove-orphans",
                "--volumes",
                timeout=180,
                check=False,
            )
        report.print()

    report.assert_all_passed()
