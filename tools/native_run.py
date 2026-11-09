#!/usr/bin/env python3
"""Local POSIX prototype: run THS from native binaries, without Docker."""

import argparse
from contextlib import ExitStack
import json
import math
import os
from pathlib import Path
import queue
import signal
import shutil
import socket
import subprocess
import sys
import threading
import time
import tomllib
import urllib.error
import urllib.request
import uuid


def native_config(text, chain_dir, rpc_port, p2p_port):
    config = tomllib.loads(text)
    if config.get("network", {}).get("network") != "Regtest":
        raise ValueError("initializer must produce a Regtest configuration")
    expected = {
        "network": {"network", "listen_addr", "testnet_parameters"},
        "rpc": {"listen_addr", "enable_cookie_auth"},
        "state": {"cache_dir"},
        "mining": {"miner_address", "extra_coinbase_data"},
    }
    if set(config) != set(expected) or any(set(config[key]) != fields for key, fields in expected.items()):
        raise ValueError("unexpected initializer configuration; review the native adapter")
    # Fail on initializer format drift instead of silently leaving a public bind.
    replacements = {
        # Native QUIC otherwise opens 0.0.0.0:8234, outside Docker's isolation.
        '[network]\n': '[network]\np2p_stack = "legacy"\n',
        'listen_addr = "0.0.0.0:18233"': f'listen_addr = "127.0.0.1:{p2p_port}"',
        'listen_addr = "0.0.0.0:18232"': f'listen_addr = "127.0.0.1:{rpc_port}"',
        'cache_dir = "/data"': f'cache_dir = {json.dumps(str(chain_dir), ensure_ascii=False)}',
    }
    for old, new in replacements.items():
        if text.count(old) != 1:
            raise ValueError(f"unexpected initializer configuration: {old}")
        text = text.replace(old, new)
    tomllib.loads(text)
    return text


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *args):
        raise ValueError("refusing a redirect from a local service")


HTTP = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())


def read_json(url, timeout, check, body=None):
    result = queue.Queue(maxsize=1)

    def fetch():
        request = urllib.request.Request(url, data=None if body is None else json.dumps(body).encode(),
                                         headers={"Content-Type": "application/json"})
        try:
            with HTTP.open(request, timeout=timeout) as response:
                result.put(json.load(response))
        except (urllib.error.URLError, TimeoutError, ConnectionError):
            result.put(None)  # Retry while dependencies become ready.
        except Exception as error:
            result.put(error)

    # Socket timeouts only bound inactivity. Keep checking the overall deadline
    # and cancellation even if the owned service continuously drips bytes.
    # Only one probe is in flight; cancellation stops its service during cleanup.
    threading.Thread(target=fetch, daemon=True).start()
    while True:
        check()
        try:
            value = result.get(timeout=.05)
        except queue.Empty:
            continue
        if isinstance(value, Exception):
            raise value
        return value


def signal_group(process, signum):
    try:
        os.killpg(process.pid, signum)
    except ProcessLookupError:
        pass  # The group has already exited.


def stop_processes(processes):
    errors = []
    for name, process in reversed(processes):
        # macOS refuses to signal a group whose leader is an unreaped zombie.
        process.poll()
        try:
            signal_group(process, signal.SIGTERM)
        except OSError as error:
            errors.append(f"{name}: {error}")
    deadline = time.monotonic() + 3
    for _, process in reversed(processes):
        try:
            process.wait(timeout=max(0, deadline - time.monotonic()))
        except subprocess.TimeoutExpired:
            pass  # Escalate below, including descendants whose leader exited.
    remaining = list(processes)
    while remaining and time.monotonic() < deadline:
        alive = []
        for item in remaining:
            try:
                os.killpg(item[1].pid, 0)
                alive.append(item)
            except ProcessLookupError:
                pass
            except PermissionError:
                # macOS returns EPERM for a group containing only zombies;
                # allow the system to reap orphaned descendants before escalation.
                alive.append(item)
        remaining = alive
        if remaining:
            time.sleep(.02)
    deadline = time.monotonic() + 3
    for name, process in reversed(remaining):
        try:
            signal_group(process, signal.SIGKILL)
        except OSError as error:
            errors.append(f"{name}: {error}")
        try:
            process.wait(timeout=max(0, deadline - time.monotonic()))
        except subprocess.TimeoutExpired:
            errors.append(f"{name}: still running after SIGKILL")
    if errors:
        raise RuntimeError(f"process cleanup failed: {'; '.join(errors)}")


def run(args):
    if os.name != "posix":
        raise ValueError("the native prototype currently requires macOS or Linux")
    lsof = shutil.which("lsof", path=os.defpath + ":/usr/sbin:/sbin")
    if lsof is None:
        raise ValueError("lsof is required to verify native listener ownership")
    binaries = {name: (args.bin_dir / name).resolve() for name in ["zakurad", "lightwalletd", "ths-server"]}
    for name, path in binaries.items():
        if not path.is_file() or not os.access(path, os.X_OK):
            raise ValueError(f"missing executable {name}: {path}")
    state = args.state_dir.absolute()
    try:
        state.mkdir(mode=0o700)
    except FileExistsError as error:
        raise ValueError(f"state path already exists; choose a new directory: {state}") from error
    state = state.resolve()
    for directory in ["wallet", "chain", "config", "lightwalletd", "logs", "tmp"]:
        (state / directory).mkdir(mode=0o700)
    name = f"native-{uuid.uuid4().hex}"
    interrupted = []
    processes = []
    services = []
    previous = {}
    for signum in [signal.SIGINT, signal.SIGTERM]:
        previous[signum] = signal.signal(signum, lambda value, frame: interrupted.append(value))
    deadline = time.monotonic() + args.timeout
    # These processes must not inherit config, proxies, or backend addresses
    # from a developer's shell. The application keeps its normal environment.
    env = {"PATH": os.defpath, "HOME": str(state), "TMPDIR": str(state / "tmp"), "LANG": "C"}

    def check(starting=True):
        if interrupted:
            raise InterruptedError(interrupted[0])
        for service, process in services:
            if process.poll() is not None:
                raise RuntimeError(f"{service} exited ({process.returncode}); see {state / 'logs'}")
        if starting and time.monotonic() >= deadline:
            raise TimeoutError(f"startup timed out; see {state / 'logs'}")

    def start(label, command, environment, log=True, cwd=state):
        check()
        with ExitStack() as stack:
            output = stack.enter_context((state / "logs" / f"{label}.log").open("wb")) if log else None
            process = subprocess.Popen(command, cwd=cwd, env=environment,
                                       stdin=subprocess.DEVNULL, stdout=output,
                                       stderr=subprocess.STDOUT if log else None,
                                       start_new_session=True)
            processes.append((label, process))
        return process

    def wait_for(probe):
        while True:
            check()
            if probe(max(.01, min(1, deadline - time.monotonic()))):
                check()
                return
            time.sleep(.05)

    def owns_listener(process, port, timeout):
        result = subprocess.run([lsof, "-nP", "-a", "-p", str(process.pid),
                                 f"-iTCP:{port}", "-sTCP:LISTEN", "-Fpn"],
                                env=env, stdin=subprocess.DEVNULL, capture_output=True,
                                text=True, timeout=timeout)
        if result.returncode not in (0, 1) or result.stderr:
            raise RuntimeError(f"checking listener ownership: {result.stderr.strip()}")
        lines = result.stdout.splitlines()
        return f"p{process.pid}" in lines and f"n127.0.0.1:{port}" in lines

    try:
        with ExitStack() as reservations:
            sockets = {}
            for key in ["dashboard", "rpc", "lightwalletd", "p2p"]:
                sock = reservations.enter_context(socket.socket())
                sock.bind(("127.0.0.1", 0))
                sockets[key] = sock
            ports = {key: sock.getsockname()[1] for key, sock in sockets.items()}
            endpoints = {key: f"{'http://' if key != 'p2p' else ''}127.0.0.1:{port}"
                         for key, port in ports.items()} | {"network": "regtest", "tls": False}
            initializer = start("init", [str(binaries["ths-server"]), "init", "--data-dir",
                                         str(state / "wallet"), "--config-dir", str(state / "config")], env)
            wait_for(lambda _: initializer.poll() is not None)
            if initializer.returncode != 0:
                raise RuntimeError(f"initializer exited ({initializer.returncode}); see {state / 'logs/init.log'}")
            stop_processes([("init", initializer)])
            processes.remove(("init", initializer))
            config = state / "config/zakurad.toml"
            config.write_text(native_config(config.read_text(), state / "chain", ports["rpc"], ports["p2p"]))
            # Binaries cannot inherit sockets. Verify the new listener's PID
            # before any RPC so a bind race cannot adopt another local node.
            sockets["rpc"].close()
            sockets["p2p"].close()
            services.append(("zakurad", start("zakurad", [str(binaries["zakurad"]), "--config", str(config), "start"], env)))
            node = services[-1][1]
            wait_for(lambda timeout: owns_listener(node, ports["rpc"], timeout))
            wait_for(lambda timeout: owns_listener(node, ports["p2p"], timeout))

            def node_ready(timeout):
                # Zakura labels both Testnet and Regtest as "test"; the genesis
                # hash is the same Regtest check used by the server's chain API.
                value = read_json(endpoints["rpc"], timeout, check,
                                  {"jsonrpc": "2.0", "id": 1, "method": "getblockhash", "params": [0]})
                if value is None or value.get("error") or not value.get("result"):
                    return False
                if value["result"] != "029f11d80ef9765602235e1bc9727e3eb6ba20839319f761fee920d63401e327":
                    raise ValueError("node genesis does not match Regtest")
                return True

            wait_for(node_ready)
            sockets["lightwalletd"].close()
            services.append(("lightwalletd", start("lightwalletd", [str(binaries["lightwalletd"]),
                "--no-tls-very-insecure", "--grpc-bind-addr", f"127.0.0.1:{ports['lightwalletd']}",
                "--http-bind-addr", "127.0.0.1:0", "--rpchost", "127.0.0.1", "--rpcport", str(ports["rpc"]),
                "--rpcuser", "unused", "--rpcpassword", "unused", "--data-dir", str(state / "lightwalletd"),
                "--log-file", "/dev/stdout"], env)))
            indexer = services[-1][1]
            wait_for(lambda timeout: owns_listener(indexer, ports["lightwalletd"], timeout))
            sockets["dashboard"].close()
            server_env = env | {
                "THS_INSTANCE": name, "THS_LISTEN": f"127.0.0.1:{ports['dashboard']}",
                "THS_ZAKURA_RPC": endpoints["rpc"], "THS_LIGHTWALLETD": endpoints["lightwalletd"],
                "THS_PUBLIC_ZAKURA_RPC": endpoints["rpc"], "THS_PUBLIC_LIGHTWALLETD": endpoints["lightwalletd"],
                "THS_PUBLIC_DASHBOARD": endpoints["dashboard"], "THS_PUBLIC_P2P": endpoints["p2p"],
                "THS_MINING_MODE": "manual", "THS_WEB_DIR": str(args.web_dir.resolve()),
            }
            services.append(("ths-server", start("ths-server", [str(binaries["ths-server"]), "serve", "--data-dir", str(state / "wallet")], server_env)))
            server = services[-1][1]
            wait_for(lambda timeout: owns_listener(server, ports["dashboard"], timeout))

            def settled(timeout):
                value = read_json(endpoints["dashboard"] + "/api/v1/status", timeout, check)
                if value is None:
                    return False
                if value.get("instance") != name or value.get("network") != "Regtest":
                    raise ValueError("server identity or Regtest network mismatch")
                return value.get("settled") is True

            wait_for(settled)
            (state / "endpoints.json").write_text(json.dumps(endpoints, indent=2) + "\n")
            print(json.dumps(endpoints), flush=True)
        if args.command:
            child_env = os.environ | {
                "THS_INSTANCE": name, "THS_NETWORK": "regtest", "THS_API_URL": endpoints["dashboard"],
                "THS_ZAKURA_RPC_URL": endpoints["rpc"], "THS_LIGHTWALLETD_URL": endpoints["lightwalletd"],
                "THS_LIGHTWALLETD_TLS": "false", "THS_P2P_ADDR": endpoints["p2p"],
            }
            child = start("application", args.command, child_env, log=False, cwd=Path.cwd())
            while child.poll() is None:
                check(starting=False)
                time.sleep(.05)
            check(starting=False)
            return child.returncode if child.returncode >= 0 else 128 - child.returncode
        while True:
            check(starting=False)
            time.sleep(.1)
    except InterruptedError as error:
        return 128 + error.args[0]
    finally:
        try:
            stop_processes(processes)
        finally:
            for signum, handler in previous.items():
                signal.signal(signum, handler)
        print(f"Native processes stopped; state and logs retained at {state}", file=sys.stderr)


def positive_seconds(value):
    seconds = float(value)
    if not math.isfinite(seconds) or seconds <= 0:
        raise argparse.ArgumentTypeError("timeout must be a finite positive number")
    return seconds


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, required=True)
    parser.add_argument("--state-dir", type=Path, required=True, help="new directory; parent must exist")
    parser.add_argument("--web-dir", type=Path, default=Path(__file__).resolve().parents[1] / "web/dist")
    parser.add_argument("--timeout", type=positive_seconds, default=180)
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    if args.command[:1] == ["--"]:
        args.command = args.command[1:]
    try:
        return run(args)
    except (OSError, ValueError, RuntimeError) as error:
        print(f"native runtime: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
