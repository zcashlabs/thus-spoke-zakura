#!/usr/bin/env python3
"""Lifecycle regressions for the local native prototype; no Docker or real nodes."""

import json
import http.server
import os
from pathlib import Path
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
import tomllib
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "tools"))
import native_run

ROOT = Path(__file__).resolve().parents[1]
RUNNER = ROOT / "tools/native_run.py"
CONFIG = '''[network]
network = "Regtest"
listen_addr = "0.0.0.0:18233"
[network.testnet_parameters]
lockbox_disbursements = [{ address = "test-marker", amount = 0 }]
[network.testnet_parameters.activation_heights]
"NU7" = 1
[rpc]
listen_addr = "0.0.0.0:18232"
enable_cookie_auth = false
[state]
cache_dir = "/data"
[mining]
miner_address = "test-miner"
extra_coinbase_data = "thus-spoke-zakura"
'''

# Real executable stand-ins model only the expensive service boundaries. They
# reject wrong configuration and expose sockets so cleanup is observable.
FIXTURE = r'''
import http.server, json, os, pathlib, socket, sys, time, tomllib
name = pathlib.Path(sys.argv[0]).name
root = pathlib.Path.cwd()
with (root / "pids.jsonl").open("a") as log:
    log.write(json.dumps({"name": name, "pid": os.getpid()}) + "\n")
if "THS_ZAKURA_RPC" in os.environ:
    assert os.environ["THS_ZAKURA_RPC"].startswith("http://127.0.0.1:")
assert "RPC_HOST" not in os.environ
assert "ZAKURA_NETWORK__NETWORK" not in os.environ
def arg(key): return sys.argv[sys.argv.index(key) + 1]
def bind(address):
    host, port = address.split(":")
    assert host == "127.0.0.1", address
    sock = socket.socket()
    sock.bind((host, int(port)))
    sock.listen()
    return sock
if name == "ths-server" and sys.argv[1] == "init":
    if (pathlib.Path(__file__).parent / "slow-init").exists(): time.sleep(60)
    path = pathlib.Path(arg("--config-dir"))
    path.mkdir(parents=True, exist_ok=True)
    (path / "zakurad.toml").write_text(CONFIG)
    sys.exit(0)
if (pathlib.Path(__file__).parent / ("fail-" + name)).exists(): sys.exit(31)
if name == "zakurad":
    config = tomllib.loads(pathlib.Path(arg("--config")).read_text())
    assert config["network"]["network"] == "Regtest"
    # Zakura 1.7 defaults to a second, public QUIC listener on a fixed port.
    assert config["network"]["p2p_stack"] == "legacy"
    assert config["network"]["testnet_parameters"]["activation_heights"]["NU7"] == 1
    assert pathlib.Path(config["state"]["cache_dir"]).is_relative_to(root)
    peer = bind(config["network"]["listen_addr"])
    address = config["rpc"]["listen_addr"]
elif name == "lightwalletd":
    assert arg("--rpchost") == "127.0.0.1"
    grpc = bind(arg("--grpc-bind-addr"))
    metrics = bind(arg("--http-bind-addr"))
    while True: time.sleep(.1)
else:
    assert os.environ["THS_MINING_MODE"] == "manual"
    address = os.environ["THS_LISTEN"]
class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *args): pass
    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers.get("Content-Length", "0"))))
        genesis = "029f11d80ef9765602235e1bc9727e3eb6ba20839319f761fee920d63401e327"
        if (pathlib.Path(__file__).parent / "wrong-genesis").exists(): genesis = "foreign"
        self.reply({"result": genesis if body["method"] == "getblockhash" else {"chain": "test", "blocks": 1}})
    def do_GET(self):
        mismatch = (pathlib.Path(__file__).parent / "wrong-instance").exists()
        pending = (pathlib.Path(__file__).parent / "never-ready").exists()
        with socket.socket() as probe:
            pending |= probe.connect_ex(("127.0.0.1", int(os.environ["THS_LIGHTWALLETD"].rsplit(":", 1)[1]))) != 0
        self.reply({"instance": "foreign" if mismatch else os.environ["THS_INSTANCE"],
                    "network": "Regtest", "settled": not pending})
    def reply(self, value):
        self.send_response(200); self.end_headers()
        if name == "zakurad" and (pathlib.Path(__file__).parent / "slow-response").exists():
            (root / "probe-started").touch()
            for _ in range(80):
                self.wfile.write(b" "); self.wfile.flush(); time.sleep(.05)
        self.wfile.write(json.dumps(value).encode())
host, port = address.split(":")
assert host == "127.0.0.1"
http.server.HTTPServer((host, int(port)), Handler).serve_forever()
'''


class NativeRuntimeTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="ths-native-test-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.bins = self.root / "bin"
        self.bins.mkdir()
        for name in ["zakurad", "lightwalletd", "ths-server"]:
            executable = self.bins / name
            executable.write_text(f"#!{sys.executable}\nCONFIG = {CONFIG!r}\n" + FIXTURE)
            executable.chmod(0o700)
        self.state = self.root / "instance"

    def command(self, code="pass", timeout=5):
        return [sys.executable, str(RUNNER), "--bin-dir", str(self.bins),
                "--state-dir", str(self.state), "--timeout", str(timeout),
                "--", sys.executable, "-c", code]

    def invoke(self, code="pass", timeout=5):
        env = os.environ | {"ZAKURA_NETWORK__NETWORK": "Mainnet",
                            "RPC_HOST": "example.invalid", "HTTP_PROXY": "http://127.0.0.1:1"}
        result = subprocess.run(self.command(code, timeout), env=env,
                                capture_output=True, text=True, timeout=15)
        self.assert_clean()
        return result

    def assert_clean(self):
        pids = self.state / "pids.jsonl"
        if pids.exists():
            for line in pids.read_text().splitlines():
                with self.assertRaises(ProcessLookupError, msg=line):
                    os.kill(json.loads(line)["pid"], 0)
        endpoints = self.state / "endpoints.json"
        if endpoints.exists():
            data = json.loads(endpoints.read_text())
            for key in ["dashboard", "rpc", "lightwalletd", "p2p"]:
                port = int(data[key].rsplit(":", 1)[1])
                with socket.socket() as sock:
                    self.assertNotEqual(sock.connect_ex(("127.0.0.1", port)), 0, key)

    def test_application_receives_isolated_endpoints_and_exit_code(self):
        result = self.invoke('''
import os, json, urllib.request
assert os.environ["THS_NETWORK"] == "regtest"
assert os.environ["THS_LIGHTWALLETD_TLS"] == "false"
for key in ["THS_API_URL", "THS_ZAKURA_RPC_URL", "THS_LIGHTWALLETD_URL"]:
    assert os.environ[key].startswith("http://127.0.0.1:")
assert os.environ["THS_P2P_ADDR"].startswith("127.0.0.1:")
raise SystemExit(7)
''')
        self.assertEqual(result.returncode, 7, result.stderr)
        self.assertEqual(self.state.stat().st_mode & 0o777, 0o700)
        config = tomllib.loads((self.state / "config/zakurad.toml").read_text())
        self.assertEqual(config["mining"]["miner_address"], "test-miner")
        self.assertEqual(config["network"]["testnet_parameters"]["lockbox_disbursements"],
                         [{"address": "test-marker", "amount": 0}])

    def test_existing_directory_is_never_reused(self):
        self.state.mkdir()
        marker = self.state / "precious"
        marker.write_text("keep")
        result = self.invoke()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("already exists", result.stderr)
        self.assertEqual(marker.read_text(), "keep")
        self.assertFalse((self.state / "pids.jsonl").exists())

    def test_stopping_one_instance_keeps_the_other_running(self):
        other = NativeRuntimeTests()
        other.setUp()
        self.addCleanup(other.doCleanups)
        runners = []
        try:
            for fixture in [self, other]:
                runners.append(subprocess.Popen(fixture.command("import time; time.sleep(60)"),
                               stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True))
            deadline = time.monotonic() + 10
            while not all((fixture.state / "endpoints.json").exists() for fixture in [self, other]):
                self.assertLess(time.monotonic(), deadline)
                self.assertTrue(all(process.poll() is None for process in runners))
                time.sleep(.02)
            endpoints = [json.loads((fixture.state / "endpoints.json").read_text()) for fixture in [self, other]]
            ports = [value[key].rsplit(":", 1)[1] for value in endpoints
                     for key in ["dashboard", "rpc", "lightwalletd", "p2p"]]
            self.assertEqual(len(set(ports)), 8)
            runners[0].terminate()
            _, error = runners[0].communicate(timeout=10)
            self.assertEqual(runners[0].returncode, 143, error)
            self.assert_clean()
            self.assertIsNone(runners[1].poll())
            with native_run.HTTP.open(endpoints[1]["dashboard"] + "/api/v1/status", timeout=1) as response:
                self.assertTrue(json.load(response)["settled"])
        finally:
            for process in runners:
                if process.poll() is None:
                    process.terminate()
                process.communicate(timeout=10)
        other.assert_clean()

    def test_service_failure_stops_siblings_before_application(self):
        (self.bins / "fail-lightwalletd").touch()
        result = self.invoke("print('APP_EXECUTED')")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("lightwalletd", result.stderr)
        self.assertNotIn("APP_EXECUTED", result.stdout)

    def test_timeout_stops_services(self):
        (self.bins / "never-ready").touch()
        result = self.invoke(timeout=1)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("timed out", result.stderr)

    def test_trickling_probe_cannot_extend_startup_deadline(self):
        (self.bins / "slow-response").touch()
        started = time.monotonic()
        result = self.invoke(timeout=2)
        self.assertTrue((self.state / "probe-started").exists(), result.stderr)
        self.assertNotEqual(result.returncode, 0)
        self.assertLess(time.monotonic() - started, 3.5, result.stderr)

    def test_cleanup_reaps_exited_leaders_before_signalling_groups(self):
        ready = self.root / "sleeper-ready"
        sleeper = subprocess.Popen([sys.executable, "-c",
            f"import pathlib,signal,time; signal.signal(signal.SIGTERM,signal.SIG_IGN); pathlib.Path({str(ready)!r}).touch(); time.sleep(60)"],
            start_new_session=True)
        exited = subprocess.Popen([sys.executable, "-c", "pass"], start_new_session=True)
        try:
            deadline = time.monotonic() + 5
            while True:
                status = subprocess.check_output(["ps", "-o", "stat=", "-p", str(exited.pid)], text=True).strip()
                if status.startswith("Z") and ready.exists():
                    break
                self.assertLess(time.monotonic(), deadline)
                time.sleep(.02)
            native_run.stop_processes([("sleeper", sleeper), ("exited", exited)])
            self.assertEqual(sleeper.poll(), -signal.SIGKILL)
            self.assertEqual(exited.returncode, 0)
        finally:
            for process in [sleeper, exited]:
                if process.poll() is None:
                    process.kill()
                process.wait(timeout=3)

    def test_cleanup_failure_does_not_skip_other_groups(self):
        sleeper = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(60)"], start_new_session=True)
        failing = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(60)"], start_new_session=True)
        real_signal = native_run.signal_group

        def fail_one_signal(process, signum):
            if process is failing and signum == signal.SIGTERM:
                raise PermissionError("simulated signalling failure")
            real_signal(process, signum)

        try:
            with patch.object(native_run, "signal_group", side_effect=fail_one_signal):
                with self.assertRaisesRegex(RuntimeError, "simulated signalling failure"):
                    native_run.stop_processes([("sleeper", sleeper), ("failing", failing)])
            self.assertIsNotNone(sleeper.poll())
            self.assertIsNotNone(failing.poll())
        finally:
            for process in [sleeper, failing]:
                if process.poll() is None:
                    process.kill()
                process.wait(timeout=3)

    def test_foreign_status_is_never_accepted(self):
        (self.bins / "wrong-instance").touch()
        result = self.invoke()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("identity", result.stderr)

    def test_foreign_node_genesis_is_rejected(self):
        (self.bins / "wrong-genesis").touch()
        result = self.invoke("print('APP_EXECUTED')")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("genesis", result.stderr)
        self.assertNotIn("APP_EXECUTED", result.stdout)

    def test_signal_during_initializer_cleans_up(self):
        (self.bins / "slow-init").touch()
        process = subprocess.Popen(self.command(), stdout=subprocess.PIPE,
                                   stderr=subprocess.PIPE, text=True)
        try:
            deadline = time.monotonic() + 5
            while not (self.state / "pids.jsonl").exists() and process.poll() is None:
                self.assertLess(time.monotonic(), deadline)
                time.sleep(.02)
            process.send_signal(signal.SIGTERM)
            _, error = process.communicate(timeout=10)
            self.assertEqual(process.returncode, 143, error)
            self.assert_clean()
        finally:
            if process.poll() is None:
                process.kill()
                process.wait()

    def test_non_regtest_initializer_output_is_rejected(self):
        server = self.bins / "ths-server"
        server.write_text(server.read_text().replace('network = "Regtest"', 'network = "Mainnet"'))
        result = self.invoke()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Regtest", result.stderr)
        pids = [json.loads(line)["name"] for line in (self.state / "pids.jsonl").read_text().splitlines()]
        self.assertEqual(pids, ["ths-server"])

    def test_missing_binary_fails_before_allocating_state(self):
        (self.bins / "zakurad").unlink()
        result = self.invoke()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("zakurad", result.stderr)
        self.assertFalse(self.state.exists())

    def test_initializer_timeout_stops_initializer(self):
        (self.bins / "slow-init").touch()
        result = self.invoke(timeout=.2)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("timed out", result.stderr)

    def test_invalid_timeout_does_not_create_state(self):
        for value in ["0", "-1", "nan", "inf"]:
            with self.subTest(value=value):
                result = self.invoke(timeout=value)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("finite positive", result.stderr)
                self.assertFalse(self.state.exists())

    def test_unknown_config_listener_is_rejected(self):
        server = self.bins / "ths-server"
        server.write_text(server.read_text().replace("CONFIG = " + repr(CONFIG),
                          "CONFIG = " + repr(CONFIG + '\n[metrics]\nendpoint_addr = "0.0.0.0:9999"\n')))
        result = self.invoke()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("unexpected initializer configuration", result.stderr)

    def test_unicode_state_directory_is_supported(self):
        self.state = self.root / "native-🧪"
        result = self.invoke()
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_changed_network_header_is_rejected_before_starting_node(self):
        server = self.bins / "ths-server"
        server.write_text(server.read_text().replace("CONFIG = " + repr(CONFIG),
                          "CONFIG = " + repr(CONFIG.replace('[network]', '[network] # comment'))))
        result = self.invoke()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("unexpected initializer configuration", result.stderr)
        pids = [json.loads(line)["name"] for line in (self.state / "pids.jsonl").read_text().splitlines()]
        self.assertEqual(pids, ["ths-server"])

    def test_application_descendants_are_stopped_on_normal_exit(self):
        marker = self.root / "descendant"
        code = f'''
import pathlib, subprocess, sys, time
p = subprocess.Popen([sys.executable, "-c", "import signal,time; signal.signal(signal.SIGTERM, signal.SIG_IGN); time.sleep(60)"])
pathlib.Path({str(marker)!r}).write_text(str(p.pid))
time.sleep(.2)
'''
        result = self.invoke(code)
        self.assertEqual(result.returncode, 0, result.stderr)
        pid = int(marker.read_text())
        deadline = time.monotonic() + 3
        while time.monotonic() < deadline:
            try:
                os.kill(pid, 0)
            except ProcessLookupError:
                break
            time.sleep(.05)
        else:
            self.fail(f"application descendant {pid} survived cleanup")

    def test_service_exit_while_app_runs_stops_the_application(self):
        code = f'''
import json, os, pathlib, signal, time
for line in pathlib.Path({str(self.state / 'pids.jsonl')!r}).read_text().splitlines():
    item = json.loads(line)
    if item["name"] == "lightwalletd": os.kill(item["pid"], signal.SIGTERM)
time.sleep(60)
'''
        result = self.invoke(code)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("lightwalletd exited", result.stderr)

    def test_sigint_stops_application_and_services(self):
        marker = self.root / "application-pid"
        code = f"import os,pathlib,time; pathlib.Path({str(marker)!r}).write_text(str(os.getpid())); time.sleep(60)"
        process = subprocess.Popen(self.command(code), stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        try:
            deadline = time.monotonic() + 5
            while not marker.exists() and process.poll() is None:
                self.assertLess(time.monotonic(), deadline)
                time.sleep(.02)
            process.send_signal(signal.SIGINT)
            _, error = process.communicate(timeout=10)
            self.assertEqual(process.returncode, 130, error)
            with self.assertRaises(ProcessLookupError):
                os.kill(int(marker.read_text()), 0)
            self.assert_clean()
        finally:
            if process.poll() is None:
                process.kill()
                process.wait()

    def test_completed_initializer_descendants_are_stopped(self):
        server = self.bins / "ths-server"
        server.write_text(server.read_text().replace('    sys.exit(0)', '''    import subprocess
    child = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(60)"])
    (root / "initializer-child").write_text(str(child.pid))
    sys.exit(0)'''))
        result = self.invoke()
        self.assertEqual(result.returncode, 0, result.stderr)
        pid = int((self.state / "initializer-child").read_text())
        try:
            with self.assertRaises(ProcessLookupError):
                os.kill(pid, 0)
        finally:
            try:
                os.kill(pid, signal.SIGKILL)
            except ProcessLookupError:
                pass

    def test_port_race_never_probes_or_adopts_a_foreign_node(self):
        node = self.bins / "zakurad"
        node.write_text(node.read_text().replace('if name == "zakurad":\n',
                        'if name == "zakurad":\n    time.sleep(2)\n'))
        requests = []

        class ForeignNode(http.server.BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def do_POST(self):
                requests.append(self.rfile.read(int(self.headers["Content-Length"])))
                self.send_response(200)
                self.end_headers()
                self.wfile.write(b'{"result":"029f11d80ef9765602235e1bc9727e3eb6ba20839319f761fee920d63401e327"}')

        process = subprocess.Popen(self.command("print('APP_EXECUTED')"),
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        foreign = None
        thread = None
        try:
            deadline = time.monotonic() + 5
            config_path = self.state / "config/zakurad.toml"
            while True:
                self.assertLess(time.monotonic(), deadline)
                try:
                    address = tomllib.loads(config_path.read_text())["rpc"]["listen_addr"]
                    if address.startswith("127.0.0.1:"):
                        break
                except (FileNotFoundError, KeyError, tomllib.TOMLDecodeError):
                    pass
                time.sleep(.005)
            port = int(address.rsplit(":", 1)[1])
            foreign = http.server.HTTPServer(("127.0.0.1", port), ForeignNode)
            thread = threading.Thread(target=foreign.serve_forever, daemon=True)
            thread.start()
            output, error = process.communicate(timeout=10)
            self.assertNotEqual(process.returncode, 0, error)
            self.assertNotIn("APP_EXECUTED", output)
            self.assertEqual(requests, [], "probed a node before proving process ownership")
            with socket.create_connection(("127.0.0.1", port), timeout=1):
                pass  # The foreign service must still be running.
        finally:
            if process.poll() is None:
                process.terminate()
                process.wait(timeout=5)
            if foreign:
                foreign.shutdown()
                foreign.server_close()
                thread.join(timeout=2)
        self.assert_clean()


if __name__ == "__main__":
    unittest.main()
