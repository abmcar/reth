#!/usr/bin/env python3
"""Hermetic fault-injection tests for the Reth HA RPC workflow."""

from __future__ import annotations

import contextlib
import hashlib
import http.server
import importlib.util
import json
import os
import socket
import subprocess
import sys
import tempfile
import threading
import time
import unittest
import urllib.error
import urllib.request
from pathlib import Path
from types import SimpleNamespace
from typing import Any, Callable
from unittest import mock


MODULE_PATH = Path(__file__).resolve().parents[1] / "reth_rpc_ha.py"
SPEC = importlib.util.spec_from_file_location("reth_rpc_ha", MODULE_PATH)
assert SPEC and SPEC.loader
HA = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = HA
SPEC.loader.exec_module(HA)

GENESIS = HA.MAINNET_GENESIS_HASH
FINAL_HASH = "0x" + "22" * 32
OTHER_HASH = "0x" + "33" * 32
THIRD_HASH = "0x" + "44" * 32
TX_HASH = "0x" + "55" * 32
SECRET = "fixture-secret"


def response_for(request: dict[str, Any], block_hash: str = FINAL_HASH) -> Any:
    method = request["method"]
    params = request.get("params", [])
    if method == "eth_chainId":
        return "0x1"
    if method == "eth_syncing":
        return False
    if method == "eth_getBlockByNumber":
        selector = params[0]
        if selector == "0x0":
            return {
                "number": "0x0",
                "hash": GENESIS,
                "parentHash": "0x" + "00" * 32,
                "transactions": [],
            }
        return {
            "number": selector if selector != "finalized" else "0x10",
            "hash": block_hash,
            "parentHash": "0x" + "21" * 32,
            "transactions": [],
            "gasUsed": "0x0",
        }
    if method == "eth_getBlockByHash":
        return {
            "number": "0x10",
            "hash": params[0],
            "parentHash": "0x" + "21" * 32,
            "transactions": [TX_HASH],
            "gasUsed": "0x0",
        }
    if method == "eth_getTransactionByHash":
        return {"hash": params[0], "blockHash": FINAL_HASH}
    if method == "debug_getRawHeader":
        return "0x01"
    if method == "debug_executionWitnessByBlockHash":
        return {
            "state": ["0x01"],
            "codes": ["0x"],
            "keys": [],
            "headers": ["0x01"],
        }
    if method == "debug_getRawBlock":
        return "0x01"
    raise AssertionError(f"unexpected fake method: {method}")


Behavior = Callable[
    [dict[str, Any]],
    tuple[int, Any, dict[str, str], float],
]


class FakeRpc:
    def __init__(self, behavior: Behavior | None = None) -> None:
        self.behavior = behavior or self.default_behavior
        self.requests: list[dict[str, Any]] = []
        fake = self

        class Handler(http.server.BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, _format: str, *_args: Any) -> None:
                return

            def do_POST(self) -> None:
                length = int(self.headers["content-length"])
                request = json.loads(self.rfile.read(length))
                fake.requests.append(request)
                status, result, headers, delay = fake.behavior(request)
                if delay:
                    time.sleep(delay)
                if isinstance(result, bytes):
                    body = result
                elif isinstance(result, dict) and (
                    "result" in result or "error" in result or "jsonrpc" in result
                ):
                    document = dict(result)
                    document.setdefault("jsonrpc", "2.0")
                    document.setdefault("id", request.get("id"))
                    body = json.dumps(document).encode()
                else:
                    body = json.dumps(
                        {
                            "jsonrpc": "2.0",
                            "id": request.get("id"),
                            "result": result,
                        }
                    ).encode()
                try:
                    self.send_response(status)
                    self.send_header("content-type", "application/json")
                    self.send_header("content-length", str(len(body)))
                    for key, value in headers.items():
                        self.send_header(key, value)
                    self.end_headers()
                    self.wfile.write(body)
                except (BrokenPipeError, ConnectionResetError):
                    pass

        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    @staticmethod
    def default_behavior(
        request: dict[str, Any]
    ) -> tuple[int, Any, dict[str, str], float]:
        return 200, response_for(request), {}, 0

    @property
    def url(self) -> str:
        host, port = self.server.server_address
        authority = f"fixture-user:{SECRET}"
        return f"http://{authority}@{host}:{port}/rpc"

    def close(self) -> None:
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=5)


@contextlib.contextmanager
def fake_cluster(
    behaviors: list[Behavior | None] | None = None,
) -> Any:
    values = behaviors or [None, None, None]
    servers = [FakeRpc(behavior) for behavior in values]
    try:
        yield servers
    finally:
        for server in servers:
            server.close()


def write_config(
    root: Path,
    *,
    timeout: float = 0.5,
    attempts: int = 4,
    quorum: int = 2,
    backoff_initial_ms: int = 0,
    backoff_max_ms: int = 0,
) -> Path:
    path = root / "config.json"
    value = {
        "schema": HA.SCHEMA_CONFIG,
        "expectedChain": {"chainId": "0x1", "genesisHash": GENESIS},
        "policy": {
            "canonicalQuorum": quorum,
            "witnessReady": 2,
            "requestTimeoutSeconds": timeout,
            "witnessTimeoutSeconds": max(1, timeout),
            "maxAttempts": attempts,
            "backoffInitialMs": backoff_initial_ms,
            "backoffMaxMs": backoff_max_ms,
            "jitterRatio": 0,
            "requestsPerSecond": 1000,
            "burst": 1000,
            "poolSize": 2,
            "cacheMaxEntries": 64,
            "maxResponseBytes": 1048576,
        },
        "endpoints": [
            {
                "name": "primary",
                "role": "witness-primary",
                "urlEnv": "TEST_PRIMARY_URL",
            },
            {
                "name": "standby",
                "role": "witness-standby",
                "urlEnv": "TEST_STANDBY_URL",
            },
            {
                "name": "aux",
                "role": "canonical-aux",
                "urlEnv": "TEST_AUX_URL",
            },
        ],
    }
    path.write_text(json.dumps(value), encoding="utf-8")
    return path


def endpoint_environment(servers: list[FakeRpc]) -> dict[str, str]:
    return {
        "TEST_PRIMARY_URL": servers[0].url,
        "TEST_STANDBY_URL": servers[1].url,
        "TEST_AUX_URL": servers[2].url,
    }


class RethRpcHaTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def client(
        self,
        servers: list[FakeRpc],
        **config_options: Any,
    ) -> tuple[Any, Any, Any, Any]:
        config = HA.load_config(write_config(self.root, **config_options))
        patcher = mock.patch.dict(os.environ, endpoint_environment(servers), clear=False)
        patcher.start()
        self.addCleanup(patcher.stop)
        endpoints = HA.resolve_endpoints(config)
        metrics = HA.Metrics()
        client = HA.RpcClient(config, endpoints, metrics)
        self.addCleanup(client.close)
        return config, endpoints, metrics, client

    def test_readiness_success_has_exact_capability_matrix(self) -> None:
        with fake_cluster() as servers:
            config, endpoints, _metrics, client = self.client(servers)
            report = HA.readiness(config, endpoints, client)
        self.assertTrue(report["success"])
        self.assertEqual(report["frozenPin"]["hash"], FINAL_HASH)
        self.assertEqual(
            report["witnessReadiness"]["readyEndpoints"], ["primary", "standby"]
        )
        self.assertFalse(report["secretMaterialRecorded"])

    def test_readiness_retries_each_endpoint_vote_without_double_counting(self) -> None:
        attempts = [0, 0]

        def transient(index: int) -> Behavior:
            def behavior(
                request: dict[str, Any],
            ) -> tuple[int, Any, dict[str, str], float]:
                if request["method"] == "eth_chainId" and attempts[index] == 0:
                    attempts[index] += 1
                    return 503, {"error": "transient"}, {}, 0
                return FakeRpc.default_behavior(request)

            return behavior

        with fake_cluster([transient(0), transient(1)]) as servers:
            path = write_config(self.root, attempts=2, quorum=2)
            value = json.loads(path.read_text())
            value["endpoints"] = value["endpoints"][:2]
            path.write_text(json.dumps(value))
            config = HA.load_config(path)
            with mock.patch.dict(
                os.environ,
                {
                    "TEST_PRIMARY_URL": servers[0].url,
                    "TEST_STANDBY_URL": servers[1].url,
                },
                clear=False,
            ):
                endpoints = HA.resolve_endpoints(config)
                metrics = HA.Metrics()
                client = HA.RpcClient(config, endpoints, metrics)
                self.addCleanup(client.close)
                report = HA.readiness(config, endpoints, client)
        self.assertTrue(report["success"])
        self.assertEqual(attempts, [1, 1])
        self.assertEqual(metrics.retries, 2)
        self.assertEqual(
            [
                sum(request["method"] == "eth_chainId" for request in server.requests)
                for server in servers
            ],
            [2, 2],
        )

    def test_quorum_endpoint_vote_exhaustion_fails_closed(self) -> None:
        def unavailable(
            request: dict[str, Any],
        ) -> tuple[int, Any, dict[str, str], float]:
            if request["method"] == "eth_getBlockByNumber":
                return 503, {"error": "unavailable"}, {}, 0
            return FakeRpc.default_behavior(request)

        with fake_cluster([unavailable, unavailable, None]) as servers:
            _config, _endpoints, metrics, client = self.client(
                servers, attempts=2, quorum=2
            )
            with self.assertRaises(HA.RpcFailure) as raised:
                client.quorum_call(
                    "eth_getBlockByNumber",
                    ["0x10", False],
                    eligible_names={"primary", "standby"},
                )
        self.assertEqual(
            raised.exception.category,
            "canonical_quorum_unavailable",
        )
        self.assertEqual(metrics.retries, 2)
        self.assertEqual(
            [
                sum(
                    request["method"] == "eth_getBlockByNumber"
                    for request in server.requests
                )
                for server in servers[:2]
            ],
            [2, 2],
        )

    def test_config_requires_mainnet_identity_and_unique_url_env(self) -> None:
        path = write_config(self.root)
        value = json.loads(path.read_text())
        value["expectedChain"]["genesisHash"] = OTHER_HASH
        path.write_text(json.dumps(value))
        with self.assertRaisesRegex(
            HA.ConfigError, "production_requires_mainnet_identity"
        ):
            HA.load_config(path)

        path = write_config(self.root)
        value = json.loads(path.read_text())
        value["endpoints"][1]["urlEnv"] = value["endpoints"][0]["urlEnv"]
        path.write_text(json.dumps(value))
        with self.assertRaisesRegex(
            HA.ConfigError, "endpoint_url_env_must_be_unique"
        ):
            HA.load_config(path)

    def test_resolved_origins_are_distinct_and_headers_reject_crlf(self) -> None:
        config = HA.load_config(write_config(self.root))
        with mock.patch.dict(
            os.environ,
            {
                "TEST_PRIMARY_URL": "http://[::1]:18545/",
                "TEST_STANDBY_URL": "http://[0:0:0:0:0:0:0:1]:18545/",
                "TEST_AUX_URL": "http://[::1]:28545/",
            },
            clear=False,
        ):
            with self.assertRaisesRegex(
                HA.ConfigError, "endpoint_origins_must_be_distinct"
            ):
                HA.resolve_endpoints(config)

        with mock.patch.dict(
            os.environ,
            {
                "TEST_PRIMARY_URL": "http://127.0.0.1:18545/",
                "TEST_STANDBY_URL": "http://2130706433:18545/",
                "TEST_AUX_URL": "http://127.0.0.1:28545/",
            },
            clear=False,
        ):
            with self.assertRaisesRegex(
                HA.ConfigError, "endpoint_origins_must_be_distinct"
            ):
                HA.resolve_endpoints(config)

        port_secret = "PORT_SECRET_X"
        with mock.patch.dict(
            os.environ,
            {
                "TEST_PRIMARY_URL": f"http://127.0.0.1:{port_secret}/",
                "TEST_STANDBY_URL": "http://127.0.0.1:28545/",
                "TEST_AUX_URL": "http://127.0.0.1:38545/",
            },
            clear=False,
        ):
            with self.assertRaises(HA.ConfigError) as invalid_port:
                HA.resolve_endpoints(config)
        self.assertEqual(
            str(invalid_port.exception),
            "invalid_endpoint_secret:primary:url",
        )
        self.assertNotIn(port_secret, str(invalid_port.exception))

        with fake_cluster() as servers:
            config = HA.load_config(write_config(self.root))
            duplicate_environment = endpoint_environment(servers)
            duplicate_environment["TEST_STANDBY_URL"] = servers[0].url
            with mock.patch.dict(os.environ, duplicate_environment, clear=False):
                with self.assertRaisesRegex(
                    HA.ConfigError, "endpoint_origins_must_be_distinct"
                ):
                    HA.resolve_endpoints(config)

            value = json.loads(config.path.read_text())
            value["endpoints"][0]["headersEnv"] = "TEST_PRIMARY_HEADERS"
            config.path.write_text(json.dumps(value))
            config = HA.load_config(config.path)
            header_environment = endpoint_environment(servers)
            header_environment["TEST_PRIMARY_HEADERS"] = json.dumps(
                {"Authorization": f"Bearer {SECRET}\r\nX-Evil: yes"}
            )
            with mock.patch.dict(os.environ, header_environment, clear=False):
                with self.assertRaises(HA.ConfigError) as raised:
                    HA.resolve_endpoints(config)
            self.assertEqual(
                str(raised.exception), "invalid_endpoint_secret:primary:headers"
            )
            self.assertNotIn(SECRET, str(raised.exception))

            for header_value, category in (
                ({"Host": "rpc.invalid"}, "invalid_endpoint_secret"),
                (
                    {"Proxy-Authorization": "opaque"},
                    "invalid_endpoint_secret",
                ),
                (
                    {"Authorization": "first", "authorization": "second"},
                    "invalid_endpoint_secret",
                ),
                ({"Authorization": "explicit"}, "conflicting_endpoint_auth"),
            ):
                with self.subTest(category=category):
                    header_environment["TEST_PRIMARY_HEADERS"] = json.dumps(
                        header_value
                    )
                    with mock.patch.dict(
                        os.environ, header_environment, clear=False
                    ):
                        with self.assertRaisesRegex(HA.ConfigError, category):
                            HA.resolve_endpoints(config)

            header_environment["TEST_PRIMARY_HEADERS"] = (
                '{"X-API-Key":"first","X-API-Key":"second"}'
            )
            with mock.patch.dict(os.environ, header_environment, clear=False):
                with self.assertRaisesRegex(
                    HA.ConfigError, "invalid_endpoint_secret"
                ):
                    HA.resolve_endpoints(config)

            header_environment["TEST_PRIMARY_HEADERS"] = json.dumps(
                {"X-API-Key": "opaque"}
            )
            with mock.patch.dict(os.environ, header_environment, clear=False):
                endpoints = HA.resolve_endpoints(config)
            self.assertIn("x-api-key", endpoints[0].headers)
            self.assertNotIn("X-API-Key", endpoints[0].headers)

    def test_gateway_exposes_loopback_health_readiness_and_metrics(self) -> None:
        with fake_cluster() as servers:
            config, endpoints, metrics, client = self.client(servers)
            report = HA.readiness(config, endpoints, client)
            gateway = HA.Gateway(client, report, metrics)
            address = gateway.start()
            try:
                live = json.load(urllib.request.urlopen(address + "livez"))
                ready = json.load(urllib.request.urlopen(address + "readyz"))
                observed = json.load(urllib.request.urlopen(address + "metrics"))
            finally:
                gateway.stop()
        self.assertEqual(live["status"], "live")
        self.assertTrue(ready["success"])
        self.assertEqual(observed["schema"], HA.SCHEMA_METRICS)
        self.assertFalse(observed["secretMaterialRecorded"])

    def test_gateway_rejects_unlisted_debug_method(self) -> None:
        with fake_cluster() as servers:
            config, endpoints, metrics, client = self.client(servers)
            report = HA.readiness(config, endpoints, client)
            gateway = HA.Gateway(client, report, metrics)
            address = gateway.start()
            request = urllib.request.Request(
                address,
                data=json.dumps(
                    {
                        "jsonrpc": "2.0",
                        "id": 7,
                        "method": "debug_executionWitnessByBlockHash",
                        "params": [FINAL_HASH, "legacy"],
                    }
                ).encode(),
                headers={"content-type": "application/json"},
            )
            try:
                with self.assertRaises(urllib.error.HTTPError) as raised:
                    urllib.request.urlopen(request)
                with raised.exception:
                    error = json.load(raised.exception)
            finally:
                gateway.stop()
        self.assertEqual(raised.exception.code, 503)
        self.assertEqual(
            error["error"]["message"],
            "method_parameters_not_allowed",
        )
        self.assertFalse(
            any(
                request["method"] == "debug_executionWitnessByBlockHash"
                and request["params"] == [FINAL_HASH, "legacy"]
                for server in servers
                for request in server.requests
            )
        )

    def test_gateway_excludes_endpoint_that_failed_readiness(self) -> None:
        def syncing(
            request: dict[str, Any],
        ) -> tuple[int, Any, dict[str, str], float]:
            if request["method"] == "eth_syncing":
                return 200, {"startingBlock": "0x1", "currentBlock": "0x2"}, {}, 0
            return FakeRpc.default_behavior(request)

        with fake_cluster([None, None, syncing]) as servers:
            config, endpoints, metrics, client = self.client(servers)
            report = HA.readiness(config, endpoints, client)
            self.assertTrue(report["success"])
            self.assertEqual(
                report["endpoints"][2]["failureCategory"],
                "endpoint_syncing",
            )
            aux_request_count = len(servers[2].requests)
            gateway = HA.Gateway(client, report, metrics)
            address = gateway.start()
            request = urllib.request.Request(
                address,
                data=json.dumps(
                    {
                        "jsonrpc": "2.0",
                        "id": 8,
                        "method": "eth_getBlockByNumber",
                        "params": ["0x10", False],
                    }
                ).encode(),
                headers={"content-type": "application/json"},
            )
            try:
                with urllib.request.urlopen(request) as response:
                    result = json.load(response)
            finally:
                gateway.stop()
        self.assertEqual(result["result"]["hash"], FINAL_HASH)
        self.assertEqual(len(servers[2].requests), aux_request_count)

    def test_primary_failure_fails_over_to_standby(self) -> None:
        def primary(request: dict[str, Any]) -> tuple[int, Any, dict[str, str], float]:
            if request["method"] == "debug_getRawBlock":
                return 503, {"error": "unavailable"}, {}, 0
            return FakeRpc.default_behavior(request)

        with fake_cluster([primary, None, None]) as servers:
            _config, _endpoints, metrics, client = self.client(servers, attempts=2)
            result = client.call(
                "debug_getRawBlock",
                [{"blockHash": FINAL_HASH, "requireCanonical": True}],
                witness_only=True,
                use_cache=False,
            )
        self.assertEqual(result, "0x01")
        self.assertEqual(metrics.failovers, 1)
        self.assertEqual(metrics.by_category["upstream_5xx"], 1)

    def test_missing_witness_capability_fails_readiness_closed(self) -> None:
        def missing(request: dict[str, Any]) -> tuple[int, Any, dict[str, str], float]:
            if request["method"] == "debug_executionWitnessByBlockHash":
                return 200, {"error": {"code": -32601, "message": "missing"}}, {}, 0
            return FakeRpc.default_behavior(request)

        with fake_cluster([missing, None, None]) as servers:
            config, endpoints, _metrics, client = self.client(servers)
            report = HA.readiness(config, endpoints, client)
        self.assertFalse(report["success"])
        self.assertIn("witness_quorum_unavailable", report["failureCategories"])
        self.assertEqual(report["endpoints"][0]["failureCategory"], "capability_missing")

    def test_chain_mismatch_fails_before_capability_probe(self) -> None:
        def wrong_chain(
            request: dict[str, Any]
        ) -> tuple[int, Any, dict[str, str], float]:
            if request["method"] == "eth_chainId":
                return 200, "0x2", {}, 0
            return FakeRpc.default_behavior(request)

        with fake_cluster([wrong_chain, None, None]) as servers:
            config, endpoints, _metrics, client = self.client(servers)
            report = HA.readiness(config, endpoints, client)
        self.assertFalse(report["success"])
        self.assertEqual(report["endpoints"][0]["failureCategory"], "chain_mismatch")
        primary_methods = [request["method"] for request in servers[0].requests]
        self.assertEqual(primary_methods, ["eth_chainId"])

    def test_frozen_finality_hash_drift_fails_closed(self) -> None:
        def drift(request: dict[str, Any]) -> tuple[int, Any, dict[str, str], float]:
            return 200, response_for(request, OTHER_HASH), {}, 0

        frozen = {
            "numberHex": "0x10",
            "number": 16,
            "hash": FINAL_HASH,
            "agreedEndpoints": ["primary", "standby"],
        }
        with fake_cluster([drift, drift, drift]) as servers:
            config, endpoints, _metrics, client = self.client(servers)
            report = HA.readiness(config, endpoints, client, frozen_pin=frozen)
        self.assertFalse(report["success"])
        self.assertIn("finalized_hash_drift", report["failureCategories"])

    def test_quorum_disagreement_is_not_hidden_by_failover(self) -> None:
        def block_hash(value: str) -> Behavior:
            def behavior(
                request: dict[str, Any]
            ) -> tuple[int, Any, dict[str, str], float]:
                return 200, response_for(request, value), {}, 0

            return behavior

        with fake_cluster(
            [block_hash(FINAL_HASH), block_hash(OTHER_HASH), block_hash(THIRD_HASH)]
        ) as servers:
            _config, _endpoints, metrics, client = self.client(servers)
            with self.assertRaises(HA.RpcFailure) as raised:
                client.quorum_call("eth_getBlockByNumber", ["0x10", False])
        self.assertEqual(raised.exception.category, "canonical_quorum_disagreement")
        self.assertEqual(metrics.quorum_disagreements, 1)

    def test_reorg_is_returned_for_downstream_whole_window_recheck(self) -> None:
        def block_hash(value: str) -> Behavior:
            return lambda request: (200, response_for(request, value), {}, 0)

        with fake_cluster(
            [block_hash(OTHER_HASH), block_hash(OTHER_HASH), block_hash(FINAL_HASH)]
        ) as servers:
            _config, _endpoints, _metrics, client = self.client(servers)
            result = client.quorum_call("eth_getBlockByNumber", ["0x10", False])
        self.assertEqual(result["hash"], OTHER_HASH)
        self.assertNotEqual(result["hash"], FINAL_HASH)

    def test_429_retry_after_is_classified_and_bounded(self) -> None:
        def limited(request: dict[str, Any]) -> tuple[int, Any, dict[str, str], float]:
            if request["method"] == "debug_getRawBlock":
                return 429, {"error": "limited"}, {"retry-after": "1"}, 0
            return FakeRpc.default_behavior(request)

        sleeps: list[float] = []
        with fake_cluster([limited, None, None]) as servers:
            config, endpoints, metrics, _client = self.client(
                servers,
                attempts=2,
                backoff_initial_ms=0,
                backoff_max_ms=2000,
            )
            client = HA.RpcClient(
                config,
                endpoints,
                metrics,
                sleeper=sleeps.append,
            )
            self.addCleanup(client.close)
            result = client.call(
                "debug_getRawBlock",
                [{"blockHash": FINAL_HASH, "requireCanonical": True}],
                witness_only=True,
                use_cache=False,
            )
        self.assertEqual(result, "0x01")
        self.assertEqual(sleeps, [1.0])
        self.assertEqual(metrics.by_category["rate_limited"], 1)

    def test_timeout_fails_over(self) -> None:
        def slow(request: dict[str, Any]) -> tuple[int, Any, dict[str, str], float]:
            if request["method"] == "eth_getBlockByHash":
                return 200, response_for(request), {}, 0.2
            return FakeRpc.default_behavior(request)

        with fake_cluster([slow, None, None]) as servers:
            _config, _endpoints, metrics, client = self.client(
                servers, timeout=0.1, attempts=2
            )
            result = client.call(
                "eth_getBlockByHash",
                [FINAL_HASH, False],
                witness_only=False,
                use_cache=False,
            )
        self.assertEqual(result["hash"], FINAL_HASH)
        self.assertEqual(metrics.by_category["timeout"], 1)

    def test_malformed_response_fails_over(self) -> None:
        def malformed(
            request: dict[str, Any]
        ) -> tuple[int, Any, dict[str, str], float]:
            if request["method"] == "debug_getRawBlock":
                return 200, b"not-json", {}, 0
            return FakeRpc.default_behavior(request)

        with fake_cluster([malformed, None, None]) as servers:
            _config, _endpoints, metrics, client = self.client(servers, attempts=2)
            result = client.call(
                "debug_getRawBlock",
                [{"blockHash": FINAL_HASH, "requireCanonical": True}],
                witness_only=True,
                use_cache=False,
            )
        self.assertEqual(result, "0x01")
        self.assertEqual(metrics.by_category["malformed_response"], 1)

    def test_partial_cache_entry_is_rejected_and_replaced(self) -> None:
        with fake_cluster() as servers:
            config, endpoints, metrics, _client = self.client(servers)
            cache = HA.DiskCache(self.root / "cache", 64, metrics)
            client = HA.RpcClient(config, endpoints, metrics, cache)
            self.addCleanup(client.close)
            params = [{"blockHash": FINAL_HASH, "requireCanonical": True}]
            key = cache.key("debug_getRawBlock", params)
            cache.path_for(key).write_text('{"schema":', encoding="utf-8")
            result = client.call(
                "debug_getRawBlock", params, witness_only=True, use_cache=True
            )
            replacement = json.loads(cache.path_for(key).read_text())
        self.assertEqual(result, "0x01")
        self.assertEqual(metrics.by_category["cache_corrupt"], 1)
        self.assertEqual(replacement["resultSha256"], HA.sha256_bytes(b'"0x01"'))

    def test_resume_reuses_cached_hash_requests_and_redacts_credentials(self) -> None:
        script = self.root / "fake-capture.py"
        marker = self.root / "first-failure"
        script.write_text(
            """#!/usr/bin/env python3
import hashlib, json, os, pathlib, sys, urllib.request
arguments = sys.argv[1:]
output = pathlib.Path(arguments[arguments.index("--output") + 1])
count = int(arguments[arguments.index("--count") + 1])
replayer_manifest = pathlib.Path(arguments[arguments.index("--replayer-manifest") + 1])
replayer_approval = json.loads(replayer_manifest.read_text())
request = json.dumps({"jsonrpc":"2.0","id":1,"method":"eth_getBlockByHash","params":["%s",False]}).encode()
with urllib.request.urlopen(urllib.request.Request(os.environ["RETH_RPC_URL"], data=request, headers={"content-type":"application/json"})) as response:
    json.load(response)
marker = pathlib.Path(os.environ["TEST_RESUME_MARKER"])
if not marker.exists():
    marker.write_text("failed")
    print(json.dumps({"failureCategory":"injected_partial_failure"}))
    raise SystemExit(1)
output.mkdir()
(output / "bundles").mkdir()
blocks = []
for number in range(17 - count, 17):
    bundle = output / "bundles" / ("block-%%d.json" %% number)
    bundle.write_text(json.dumps({"block":number}))
    digest = hashlib.sha256(bundle.read_bytes()).hexdigest()
    blocks.append({
        "number":number,
        "numberHex":hex(number),
        "hash":"%s",
        "parentHash":"0x" + "21" * 32,
        "bundle":"bundles/" + bundle.name,
        "bundleSha256":digest
    })
manifest = {
    "schema":"reth-dtvm.atomic-capture-window.v1",
    "status":"success",
    "success":True,
    "requestedTag":"finalized",
    "count":count,
    "pinnedHead":{"number":16,"numberHex":"0x10","hash":"%s"},
    "canonicalRecheck":{"checkedCount":count,"allPinnedHashesUnchanged":True},
    "witness":{
        "method":"debug_executionWitnessByBlockHash",
        "mode":"canonical",
        "policy":"production"
    },
    "replayerIdentity":{
        "role":"downstream_replayer_identity",
        "manifestRealpath":str(replayer_manifest.resolve()),
        "manifestSha256":hashlib.sha256(replayer_manifest.read_bytes()).hexdigest(),
        "replayer":replayer_approval["replayer"]
    },
    "rpcUrlRecorded":False,
    "blocks":blocks
}
(output / "manifest.json").write_text(json.dumps(manifest))
print(json.dumps(manifest))
"""
            % (FINAL_HASH, FINAL_HASH, FINAL_HASH),
            encoding="utf-8",
        )
        script.chmod(0o700)
        identity = self.root / "identity.json"
        identity.write_text("{}")
        approved_replayer = self.root / "approved-replay-block"
        approved_replayer.write_bytes(b"approved-replayer")
        approved_replayer.chmod(0o700)
        approved_replayer_manifest = self.root / "approved-replayer.json"
        approved_replayer_manifest.write_text(
            json.dumps(
                {
                    "schema": "reth-dtvm.approved-replayer.v1",
                    "replayer": {
                        "realpath": str(approved_replayer),
                        "sha256": HA.sha256_file(approved_replayer),
                    },
                    "correctness": {"sealed": True, "passed": True},
                    "approval": {
                        "replayerIdentityApproved": True,
                        "mustMatchRealpathAndSha256": True,
                    },
                }
            )
        )
        output = self.root / "corpus"
        state_dir = self.root / "state"

        with fake_cluster() as servers, mock.patch.dict(
            os.environ,
            {
                **endpoint_environment(servers),
                "TEST_RESUME_MARKER": str(marker),
            },
            clear=False,
        ):
            config = HA.load_config(write_config(self.root))
            arguments = SimpleNamespace(
                output=str(output),
                state_dir=str(state_dir),
                count=1,
                capture_attempts=1,
                capture_timeout=10,
                dtvm_identity_manifest=str(identity),
                replayer_manifest=str(approved_replayer_manifest),
                capture_script=str(script),
                verify_witness=None,
            )
            with self.assertRaises(HA.RpcFailure) as first:
                HA.run_capture_workflow(arguments, config)
            self.assertEqual(first.exception.category, "injected_partial_failure")
            first_hits = sum(
                request["method"] == "eth_getBlockByHash"
                for server in servers
                for request in server.requests
            )
            original_publish = HA.atomic_publish_directory_noreplace

            def inject_publish_race(source: Path, target: Path) -> None:
                target.mkdir()
                original_publish(source, target)

            with mock.patch.object(
                HA,
                "atomic_publish_directory_noreplace",
                side_effect=inject_publish_race,
            ):
                with self.assertRaisesRegex(
                    HA.ConfigError, "publish_target_exists"
                ):
                    HA.run_capture_workflow(arguments, config)
            self.assertEqual(list(output.iterdir()), [])
            self.assertTrue(
                (output.parent / f".{output.name}.reth-ha-stage").is_dir()
            )
            output.rmdir()
            result = HA.run_capture_workflow(arguments, config)
            second_hits = sum(
                request["method"] == "eth_getBlockByHash"
                for server in servers
                for request in server.requests
            )
            state_path = state_dir / "resume-state.json"
            interrupted_state = json.loads(state_path.read_text())
            interrupted_state["phase"] = "checksummed"
            interrupted_state["status"] = "in_progress"
            state_path.write_text(json.dumps(interrupted_state))
            result = HA.run_capture_workflow(arguments, config)
            recovery_hits = sum(
                request["method"] == "eth_getBlockByHash"
                for server in servers
                for request in server.requests
            )
            self.assertTrue(result["publicationRecovered"])

            bundle = next((output / "bundles").iterdir())
            original_bundle = bundle.read_bytes()
            bundle.write_bytes(b"tampered")
            with self.assertRaisesRegex(HA.ConfigError, "bundle_checksum_mismatch"):
                HA.run_capture_workflow(arguments, config)
            bundle.write_bytes(original_bundle)
            bundles_directory = output / "bundles"
            external_bundles = self.root / "external-bundles"
            bundles_directory.rename(external_bundles)
            bundles_directory.symlink_to(external_bundles, target_is_directory=True)
            with self.assertRaisesRegex(
                HA.ConfigError, "bundle_missing_or_symlinked"
            ):
                HA.run_capture_workflow(arguments, config)
            bundles_directory.unlink()
            external_bundles.rename(bundles_directory)

            replay_script = self.root / "fake-replay.py"
            replay_script.write_text(
                """#!/usr/bin/env python3
import hashlib, json, os, pathlib, sys
if any(os.environ.get(name) for name in ("TEST_PRIMARY_URL", "TEST_STANDBY_URL", "TEST_AUX_URL")):
    raise SystemExit("endpoint secret reached replay")
output = pathlib.Path(sys.argv[4])
output.mkdir()
library_sha = hashlib.sha256(pathlib.Path(sys.argv[3]).read_bytes()).hexdigest()
manifest_path = pathlib.Path(sys.argv[1])
manifest = json.loads(manifest_path.read_text())
manifest_sha = hashlib.sha256(manifest_path.read_bytes()).hexdigest()
captured_block = manifest["blocks"][0]
result = {
    "schema": "reth-dtvm.corpus-correctness.v1",
    "corpus": {
        "manifestSha256": manifest_sha,
        "blockCount": len(manifest["blocks"])
    },
    "dtvm": {
        "librarySha256": library_sha,
        "loadedFromVerifiedSealedMemfd": True
    },
    "replayer": {
        "realpath": os.environ["DTVM_APPROVED_REPLAYER"],
        "sha256": os.environ["DTVM_APPROVED_REPLAYER_SHA256"]
    },
    "correctness": {
        "passed": True,
        "blockResults": [{
            "blockNumber": captured_block["number"],
            "blockHash": captured_block["hash"],
            "bundle": captured_block["bundle"],
            "bundleSha256": captured_block["bundleSha256"],
            "correctnessPassed": True,
            "differentialMatch": True,
            "rawBound": True,
            "preExecutionCommitments": True,
            "preStateRootVerified": True,
            "postStateRootVerified": True,
            "postExecutionCommitments": {
                "gasUsed": True,
                "receiptsRoot": True,
                "logsBloom": True,
                "requestsHash": True,
                "blobGasUsed": True
            }
        }]
    },
    "timingQualification": {
        "excludesFromFormalPr577PerformanceConclusion": True
    }
}
(output / "result.json").write_text(json.dumps(result))
"""
            )
            replay_script.chmod(0o700)
            library = self.root / "libdtvmapi.so"
            library.write_bytes(b"fake-library")
            replay_output = self.root / "replay"
            replay_arguments = SimpleNamespace(
                state_dir=str(state_dir),
                verify_corpus_script=str(replay_script),
                verify_corpus_sha256=HA.sha256_file(replay_script),
                dtvm_library=str(library),
                dtvm_library_sha256=HA.sha256_file(library),
                replay_output=str(replay_output),
                label="hermetic",
                replay_timeout=10,
            )
            replay_state = HA.run_replay(replay_arguments, config)
            validator = os.environ.get("DTVM_REPLAY_STATE_VALIDATOR")

            def validate_state(
                expected_status: str,
                validation_state_dir: Path = state_dir,
            ) -> subprocess.CompletedProcess[str]:
                assert validator is not None
                validation = subprocess.run(
                    [sys.executable, validator, str(validation_state_dir)],
                    capture_output=True,
                    text=True,
                    timeout=10,
                    check=False,
                    env={**os.environ, "PYTHONDONTWRITEBYTECODE": "1"},
                )
                document = json.loads(validation.stdout)
                self.assertEqual(document["status"], expected_status)
                return validation

            if validator:
                validation = validate_state("verified")
                self.assertEqual(validation.returncode, 0, validation.stdout)
                checksum_path = output / "bundle-checksums.json"
                original_checksum = checksum_path.read_bytes()
                checksum_value = json.loads(original_checksum)
                checksum_value["bundles"] = []
                os.chmod(checksum_path, 0o600)
                checksum_path.write_text(json.dumps(checksum_value))
                validation = validate_state("failure")
                self.assertNotEqual(validation.returncode, 0)
                checksum_path.write_bytes(original_checksum)
                os.chmod(checksum_path, 0o400)

            replay_result = replay_output / "result.json"
            original_replay_result = replay_result.read_bytes()
            original_replayed_state = state_path.read_bytes()

            if validator:
                bundles_directory = output / "bundles"
                external_bundles = self.root / "validator-external-bundles"
                bundles_directory.rename(external_bundles)
                bundles_directory.symlink_to(
                    external_bundles, target_is_directory=True
                )
                validation = validate_state("failure")
                self.assertNotEqual(validation.returncode, 0)
                bundles_directory.unlink()
                external_bundles.rename(bundles_directory)

            mutations = {
                "missing_corpus": lambda document: document.pop("corpus"),
                "wrong_bundle": lambda document: document["correctness"][
                    "blockResults"
                ][0].update({"bundle": "unexpected-bundle.json"}),
                "wrong_replayer_realpath": lambda document: document[
                    "replayer"
                ].update({"realpath": "/unexpected/replayer"}),
            }
            for mutation_name, mutate in mutations.items():
                with self.subTest(mutation=mutation_name):
                    mutated_report = json.loads(original_replay_result)
                    mutate(mutated_report)
                    replay_result.write_text(json.dumps(mutated_report))
                    mutated_state = json.loads(original_replayed_state)
                    mutated_state["replayResultSha256"] = HA.sha256_file(
                        replay_result
                    )
                    state_path.write_text(json.dumps(mutated_state))
                    with self.assertRaisesRegex(
                        HA.ConfigError, "replay_state_evidence_changed"
                    ):
                        HA.run_replay(replay_arguments, config)
                    with self.assertRaisesRegex(
                        HA.ConfigError, "strict_replay_contract_failed"
                    ):
                        HA.run_seal(
                            SimpleNamespace(state_dir=str(state_dir)),
                            config,
                        )
                    if validator:
                        validation = validate_state("failure")
                        self.assertNotEqual(validation.returncode, 0)
                    replay_result.write_bytes(original_replay_result)
                    state_path.write_bytes(original_replayed_state)

            replay_result.write_text('{"schema":"tampered"}')
            with self.assertRaisesRegex(
                HA.ConfigError, "replay_state_evidence_changed"
            ):
                HA.run_replay(replay_arguments, config)
            with self.assertRaisesRegex(
                HA.ConfigError, "strict_replay_result_changed"
            ):
                HA.run_seal(SimpleNamespace(state_dir=str(state_dir)), config)
            replay_result.write_bytes(original_replay_result)
            state_path.write_bytes(original_replayed_state)

            with HA.workflow_lock(state_dir):
                with self.assertRaisesRegex(
                    HA.ConfigError, "workflow_already_running"
                ):
                    HA.run_replay(replay_arguments, config)
                with self.assertRaisesRegex(
                    HA.ConfigError, "workflow_already_running"
                ):
                    HA.run_seal(
                        SimpleNamespace(state_dir=str(state_dir)),
                        config,
                    )

            unrelated_replay_output = self.root / "unrelated-replay"
            unrelated_replay_output.mkdir()
            mismatched_replay_arguments = SimpleNamespace(
                **{
                    **vars(replay_arguments),
                    "replay_output": str(unrelated_replay_output),
                }
            )
            with self.assertRaisesRegex(
                HA.ConfigError, "replay_state_output_mismatch"
            ):
                HA.run_replay(mismatched_replay_arguments, config)
            seal = HA.run_seal(SimpleNamespace(state_dir=str(state_dir)), config)
            sealed_paths = [
                state_dir / "resume-state-before-seal.json",
                state_dir / "evidence-seal.json",
                state_dir / "resume-state.json",
            ]
            sealed_bytes = {path: path.read_bytes() for path in sealed_paths}
            repeated_seal = HA.run_seal(
                SimpleNamespace(state_dir=str(state_dir)),
                config,
            )
            self.assertEqual(repeated_seal, seal)
            self.assertEqual(
                {path: path.read_bytes() for path in sealed_paths},
                sealed_bytes,
            )
            if validator:
                validation = validate_state("verified")
                self.assertEqual(validation.returncode, 0, validation.stdout)
                sealed_state_path = state_dir / "resume-state.json"
                original_sealed_state = sealed_state_path.read_bytes()

                state_link = self.root / "state-link"
                state_link.symlink_to(state_dir, target_is_directory=True)
                with self.assertRaisesRegex(
                    HA.ConfigError, "state_directory_missing_or_symlinked"
                ):
                    HA.run_seal(
                        SimpleNamespace(state_dir=str(state_link)),
                        config,
                    )
                validation = validate_state("failure", state_link)
                self.assertNotEqual(validation.returncode, 0)

                alias_target = self.root / "state-alias-target"
                alias_target.mkdir()
                state_alias = self.root / "state-alias"
                state_alias.symlink_to(alias_target, target_is_directory=True)
                with self.assertRaisesRegex(
                    HA.ConfigError, "state_directory_missing_or_symlinked"
                ):
                    HA.run_seal(
                        SimpleNamespace(
                            state_dir=str(state_alias / "new-state")
                        ),
                        config,
                    )
                self.assertFalse((alias_target / "new-state").exists())

                changed_continuity = json.loads(original_sealed_state)
                changed_continuity["publishedAtUtc"] = "tampered"
                sealed_state_path.write_text(json.dumps(changed_continuity))
                with self.assertRaisesRegex(
                    HA.ConfigError, "preseal_state_continuity_failed"
                ):
                    HA.run_seal(
                        SimpleNamespace(state_dir=str(state_dir)),
                        config,
                    )
                validation = validate_state("failure")
                self.assertNotEqual(validation.returncode, 0)
                sealed_state_path.write_bytes(original_sealed_state)

                seal_path = state_dir / "evidence-seal.json"
                original_seal = seal_path.read_bytes()
                for field, value in (
                    ("status", "tampered"),
                    ("frozenPin", {"number": 0}),
                ):
                    with self.subTest(seal_field=field):
                        changed_seal = json.loads(original_seal)
                        changed_seal[field] = value
                        os.chmod(seal_path, 0o600)
                        seal_path.write_text(json.dumps(changed_seal))
                        changed_state = json.loads(original_sealed_state)
                        changed_state["evidenceSealSha256"] = HA.sha256_file(
                            seal_path
                        )
                        sealed_state_path.write_text(json.dumps(changed_state))
                        with self.assertRaisesRegex(
                            HA.ConfigError,
                            "sealed_evidence_contract_failed",
                        ):
                            HA.run_seal(
                                SimpleNamespace(state_dir=str(state_dir)),
                                config,
                            )
                        validation = validate_state("failure")
                        self.assertNotEqual(validation.returncode, 0)
                        seal_path.write_bytes(original_seal)
                        os.chmod(seal_path, 0o400)
                        sealed_state_path.write_bytes(original_sealed_state)

                sealed_state = json.loads(original_sealed_state)
                sealed_state["strictReplayCompleted"] = False
                sealed_state_path.write_text(json.dumps(sealed_state))
                validation = validate_state("failure")
                self.assertNotEqual(validation.returncode, 0)
                sealed_state_path.write_bytes(original_sealed_state)

        self.assertEqual(result["phase"], "published")
        self.assertTrue(replay_state["strictReplayCompleted"])
        self.assertEqual(seal["status"], "sealed")
        self.assertTrue(seal["networkExcludedFromReplay"])
        self.assertEqual(first_hits, 1)
        self.assertEqual(second_hits, 1)
        self.assertEqual(recovery_hits, 1)
        self.assertTrue((output / "BUNDLE_SHA256SUMS").is_file())
        self.assertTrue((output / "bundle-checksums.json").is_file())
        persisted = b""
        for path in [*state_dir.rglob("*"), *output.rglob("*")]:
            if path.is_file():
                persisted += path.read_bytes()
        self.assertNotIn(SECRET.encode(), persisted)
        state = json.loads((state_dir / "resume-state.json").read_text())
        self.assertEqual(state["captureAttempts"], 2)
        self.assertFalse(state["credentialsRecorded"])


if __name__ == "__main__":
    unittest.main(verbosity=2)
