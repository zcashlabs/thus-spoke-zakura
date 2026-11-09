#!/usr/bin/env python3
"""Run as the native runner's child against a fresh, settled Regtest instance."""

import json
import os
import time
import urllib.parse
import urllib.request
import uuid

API = os.environ["THS_API_URL"]
RPC = os.environ["THS_ZAKURA_RPC_URL"]
assert os.environ["THS_NETWORK"] == "regtest"
assert os.environ["THS_LIGHTWALLETD_TLS"] == "false"
for url in [API, RPC, os.environ["THS_LIGHTWALLETD_URL"]]:
    assert urllib.parse.urlparse(url).hostname == "127.0.0.1"
HTTP = urllib.request.build_opener(urllib.request.ProxyHandler({}))


def request(url, body=None):
    req = urllib.request.Request(url, data=None if body is None else json.dumps(body).encode(),
                                 headers={"Content-Type": "application/json"})
    with HTTP.open(req, timeout=120) as response:
        return json.load(response)


def api(path, body=None):
    return request(API + "/api/v1/" + path, body)


def rpc(method, params=None):
    value = request(RPC, {"jsonrpc": "2.0", "id": 1, "method": method, "params": params or []})
    assert not value.get("error"), value
    return value["result"]


def wait(predicate, description):
    deadline = time.monotonic() + 120
    while time.monotonic() < deadline:
        result = predicate()
        if result:
            return result
        time.sleep(.1)
    raise AssertionError(f"timed out: {description}")


def settled():
    status = api("status")
    if not status["settled"]:
        return None
    assert status["instance"] == os.environ["THS_INSTANCE"]
    assert status["network"] == "Regtest"
    assert status["mining_mode"] == "manual"
    assert status["node"]["bestblockhash"] == status["wallet_sync"]["fully_scanned_hash"]
    assert status["node"]["blocks"] == status["wallet_sync"]["fully_scanned_height"]
    return status


def main():
    checks = []
    with HTTP.open(API, timeout=5) as response:
        assert b'<div id="root">' in response.read()
    checks.append("dashboard served")
    initial = settled()
    assert initial is not None
    accounts = api("accounts")
    assert [account["id"] for account in accounts] == [1, 2, 3, 4, 5]
    assert accounts[0]["ironwood_zatoshi"] == 500_000_000
    assert accounts[1]["ironwood_zatoshi"] == 0
    checks.append("five accounts; treasury hidden; 5 ZEC funded")
    start_height = initial["node"]["blocks"]
    mined = api("mine", {"blocks": 10})
    assert mined["blocks"] == 10 and len(set(mined["hashes"])) == 10
    status = wait(settled, "mine 10 settlement")
    assert status["node"]["blocks"] == start_height + 10
    checks.append("mine exactly 10 blocks and synchronize")

    payment = {"from_account": 1, "to_account": 2, "source_pool": "ironwood",
               "destination_pool": "ironwood", "amount_zatoshi": 1_234_567,
               "memo": "native runtime smoke", "idempotency_key": str(uuid.uuid4())}
    first = api("send", payment)
    replay = api("send", payment)
    assert first["id"] == replay["id"]
    wait(lambda: rpc("getrawmempool"), "payment broadcast")
    api("mine", {"blocks": 3})
    wait(settled, "payment settlement")
    received = api("accounts")[1]
    assert received["ironwood_zatoshi"] == 1_234_567, received["ironwood_zatoshi"]
    # Wallet settlement and the payment journal have separate background loops.
    wait(lambda: api("activity?idempotency_key=" + payment["idempotency_key"])[0]["status"] == "confirmed",
         "payment journal confirmation")
    replay = api("send", payment)
    assert replay["id"] == first["id"]
    assert replay["status"] == "confirmed", replay
    assert replay["amount_zatoshi"] == 1_234_567
    assert rpc("getrawmempool") == []
    checks.append("exact shielded payment, confirmation, and idempotent replay")

    api("mine", {"blocks": 2})
    before = wait(settled, "pre-reorg settlement")["node"]
    reorg = api("reorg", {"depth": 1, "keep_txs": False})
    after = wait(settled, "reorg settlement")["node"]
    assert after["blocks"] == before["blocks"]
    assert after["bestblockhash"] != before["bestblockhash"]
    assert len(reorg["removed_hashes"]) == len(reorg["replacement_hashes"]) == 1
    assert api("accounts")[1]["ironwood_zatoshi"] == 1_234_567
    checks.append("same-height reorg changes hash and reconciles wallet")

    api("rollback", {"depth": 1})
    rolled = wait(settled, "rollback settlement")["node"]
    assert rolled["blocks"] == after["blocks"] - 1
    assert api("accounts")[1]["ironwood_zatoshi"] == 1_234_567
    checks.append("rollback reconciles wallet without changing earlier payment")
    print(json.dumps({"result": "PASS", "checks": checks, "height": rolled["blocks"]}), flush=True)


if __name__ == "__main__":
    main()
