#!/usr/bin/env python3
"""Secret-safe HA JSON-RPC transport and resumable Reth capture orchestration."""

from __future__ import annotations

import argparse
import base64
import contextlib
import ctypes
import dataclasses
import email.utils
import errno
import fcntl
import hashlib
import http.client
import http.server
import ipaddress
import json
import os
import queue
import random
import re
import secrets
import socket
import ssl
import subprocess
import tempfile
import threading
import time
import urllib.parse
from collections import Counter, defaultdict
from pathlib import Path
from typing import Any, Callable, Iterable


SCHEMA_CONFIG = "reth-dtvm.rpc-ha-config.v1"
SCHEMA_READINESS = "reth-dtvm.rpc-ha-readiness.v1"
SCHEMA_METRICS = "reth-dtvm.rpc-ha-metrics.v1"
SCHEMA_STATE = "reth-dtvm.rpc-ha-resume-state.v1"
SCHEMA_SEAL = "reth-dtvm.rpc-ha-evidence-seal.v1"
MAINNET_CHAIN_ID = "0x1"
MAINNET_GENESIS_HASH = (
    "0xd4e56740f876aef8c010b86a40d5f56745a118d0906a34e69aec8c0db1cb8fa3"
)
HASH_RE = re.compile(r"^0x[0-9a-fA-F]{64}$")
QUANTITY_RE = re.compile(r"^0x(?:0|[1-9a-fA-F][0-9a-fA-F]*)$")
HEX_BYTES_RE = re.compile(r"^0x(?:[0-9a-fA-F]{2})+$")
SECRET_NAME_RE = re.compile(
    r"(?:url|token|secret|password|authorization|cookie|header)", re.IGNORECASE
)
HEADER_NAME_RE = re.compile(r"^[!#$%&'*+\-.^_`|~0-9A-Za-z]+$")
FORBIDDEN_FORWARD_HEADERS = {
    "connection",
    "content-length",
    "host",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
}
RETRIABLE = {"rate_limited", "upstream_5xx", "timeout", "transport_error"}
WITNESS_ROLES = {"witness-primary", "witness-standby"}
CANONICAL_ROLES = WITNESS_ROLES | {"canonical-aux"}


class ConfigError(Exception):
    """A public, secret-free configuration error."""


class RpcFailure(Exception):
    """A classified RPC failure whose text never includes endpoint material."""

    def __init__(
        self,
        category: str,
        endpoint: str | None = None,
        *,
        retry_after: float | None = None,
    ) -> None:
        super().__init__(category)
        self.category = category
        self.endpoint = endpoint
        self.retry_after = retry_after


class JsonObjectPairs(list[tuple[Any, Any]]):
    """JSON object pairs preserved so duplicate header names remain visible."""


def canonical_json(value: Any) -> bytes:
    return json.dumps(
        value, sort_keys=True, separators=(",", ":"), ensure_ascii=True
    ).encode("utf-8")


def sha256_bytes(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def lexical_absolute(path: Path) -> Path:
    return Path(os.path.abspath(path))


def require_regular_no_symlink(path: Path, category: str) -> None:
    try:
        if (
            not path.is_file()
            or path.is_symlink()
            or path.resolve(strict=True) != lexical_absolute(path)
        ):
            raise ConfigError(category)
    except OSError as error:
        raise ConfigError(category) from error


def require_directory_no_symlink(path: Path, category: str) -> None:
    path = lexical_absolute(path)
    try:
        if (
            not path.is_dir()
            or path.is_symlink()
            or path.resolve(strict=True) != path
        ):
            raise ConfigError(category)
    except OSError as error:
        raise ConfigError(category) from error


def secure_mkdir_directory(path: Path, mode: int, category: str) -> None:
    """Create an absolute directory path without following any symlink component."""
    path = lexical_absolute(path)
    if path == Path("/"):
        raise ConfigError(category)
    flags = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC
    descriptor = -1
    try:
        descriptor = os.open("/", flags)
        components = path.parts[1:]
        for index, component in enumerate(components):
            is_final = index == len(components) - 1
            try:
                child = os.open(component, flags, dir_fd=descriptor)
            except FileNotFoundError:
                try:
                    os.mkdir(
                        component,
                        mode if is_final else 0o700,
                        dir_fd=descriptor,
                    )
                except FileExistsError:
                    pass
                child = os.open(component, flags, dir_fd=descriptor)
            os.close(descriptor)
            descriptor = child
        os.fchmod(descriptor, mode)
    except OSError as error:
        raise ConfigError(category) from error
    finally:
        if descriptor >= 0:
            os.close(descriptor)
    require_directory_no_symlink(path, category)


def atomic_publish_directory_noreplace(source: Path, target: Path) -> None:
    """Atomically publish a directory while refusing to replace any target."""
    source = lexical_absolute(source)
    target = lexical_absolute(target)
    try:
        renameat2 = ctypes.CDLL(None, use_errno=True).renameat2
    except AttributeError as error:
        raise ConfigError("atomic_noreplace_unavailable") from error
    renameat2.argtypes = [
        ctypes.c_int,
        ctypes.c_char_p,
        ctypes.c_int,
        ctypes.c_char_p,
        ctypes.c_uint,
    ]
    renameat2.restype = ctypes.c_int
    result = renameat2(
        -100,
        os.fsencode(source),
        -100,
        os.fsencode(target),
        1,
    )
    if result == 0:
        return
    failure = ctypes.get_errno()
    if failure in {errno.EEXIST, errno.ENOTEMPTY}:
        raise ConfigError("publish_target_exists")
    if failure in {errno.ENOSYS, errno.EINVAL, errno.EOPNOTSUPP}:
        raise ConfigError("atomic_noreplace_unavailable")
    raise ConfigError("atomic_publish_failed")


def utc_now() -> str:
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())


def atomic_write_bytes(path: Path, value: bytes, mode: int = 0o600) -> None:
    path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    descriptor, temporary = tempfile.mkstemp(
        prefix=f".{path.name}.tmp.", dir=str(path.parent)
    )
    try:
        os.fchmod(descriptor, mode)
        with os.fdopen(descriptor, "wb") as stream:
            stream.write(value)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, path)
        directory_fd = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY)
        try:
            os.fsync(directory_fd)
        finally:
            os.close(directory_fd)
    finally:
        with contextlib.suppress(FileNotFoundError):
            os.unlink(temporary)


def atomic_write_json(path: Path, value: Any, mode: int = 0o600) -> None:
    atomic_write_bytes(path, canonical_json(value) + b"\n", mode)


def safe_json_load(path: Path) -> Any:
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        raise ConfigError(f"invalid_json:{path.name}") from error


def require_keys(value: dict[str, Any], allowed: set[str], context: str) -> None:
    unknown = sorted(set(value) - allowed)
    if unknown:
        raise ConfigError(f"unknown_{context}_keys")


@dataclasses.dataclass(frozen=True)
class Policy:
    canonical_quorum: int
    witness_ready: int
    request_timeout: float
    witness_timeout: float
    max_attempts: int
    backoff_initial_ms: int
    backoff_max_ms: int
    jitter_ratio: float
    requests_per_second: float
    burst: int
    pool_size: int
    cache_max_entries: int
    max_response_bytes: int


@dataclasses.dataclass(frozen=True)
class EndpointConfig:
    name: str
    role: str
    url_env: str
    headers_env: str | None


@dataclasses.dataclass(frozen=True)
class Config:
    path: Path
    chain_id: str
    genesis_hash: str
    policy: Policy
    endpoints: tuple[EndpointConfig, ...]
    public_fingerprint: str

    @property
    def secret_env_names(self) -> set[str]:
        names = {endpoint.url_env for endpoint in self.endpoints}
        names.update(
            endpoint.headers_env
            for endpoint in self.endpoints
            if endpoint.headers_env is not None
        )
        return names


def load_config(path: Path) -> Config:
    raw = safe_json_load(path)
    if not isinstance(raw, dict):
        raise ConfigError("config_must_be_object")
    require_keys(raw, {"schema", "expectedChain", "policy", "endpoints"}, "config")
    if raw.get("schema") != SCHEMA_CONFIG:
        raise ConfigError("unsupported_config_schema")

    chain = raw.get("expectedChain")
    if not isinstance(chain, dict):
        raise ConfigError("expected_chain_must_be_object")
    require_keys(chain, {"chainId", "genesisHash"}, "expected_chain")
    chain_id = chain.get("chainId")
    genesis_hash = chain.get("genesisHash")
    if not isinstance(chain_id, str) or not QUANTITY_RE.fullmatch(chain_id):
        raise ConfigError("invalid_expected_chain_id")
    if not isinstance(genesis_hash, str) or not HASH_RE.fullmatch(genesis_hash):
        raise ConfigError("invalid_expected_genesis_hash")
    if (
        chain_id.lower() != MAINNET_CHAIN_ID
        or genesis_hash.lower() != MAINNET_GENESIS_HASH
    ):
        raise ConfigError("production_requires_mainnet_identity")

    policy_raw = raw.get("policy")
    if not isinstance(policy_raw, dict):
        raise ConfigError("policy_must_be_object")
    allowed_policy = {
        "canonicalQuorum",
        "witnessReady",
        "requestTimeoutSeconds",
        "witnessTimeoutSeconds",
        "maxAttempts",
        "backoffInitialMs",
        "backoffMaxMs",
        "jitterRatio",
        "requestsPerSecond",
        "burst",
        "poolSize",
        "cacheMaxEntries",
        "maxResponseBytes",
    }
    require_keys(policy_raw, allowed_policy, "policy")

    def integer(name: str, default: int, minimum: int, maximum: int) -> int:
        value = policy_raw.get(name, default)
        if isinstance(value, bool) or not isinstance(value, int):
            raise ConfigError(f"invalid_policy_{name}")
        if value < minimum or value > maximum:
            raise ConfigError(f"invalid_policy_{name}")
        return value

    def number(name: str, default: float, minimum: float, maximum: float) -> float:
        value = policy_raw.get(name, default)
        if isinstance(value, bool) or not isinstance(value, (int, float)):
            raise ConfigError(f"invalid_policy_{name}")
        value = float(value)
        if value < minimum or value > maximum:
            raise ConfigError(f"invalid_policy_{name}")
        return value

    policy = Policy(
        canonical_quorum=integer("canonicalQuorum", 2, 2, 16),
        witness_ready=integer("witnessReady", 2, 2, 16),
        request_timeout=number("requestTimeoutSeconds", 30.0, 0.1, 3600.0),
        witness_timeout=number("witnessTimeoutSeconds", 600.0, 1.0, 3600.0),
        max_attempts=integer("maxAttempts", 4, 1, 32),
        backoff_initial_ms=integer("backoffInitialMs", 250, 0, 60000),
        backoff_max_ms=integer("backoffMaxMs", 5000, 0, 300000),
        jitter_ratio=number("jitterRatio", 0.2, 0.0, 1.0),
        requests_per_second=number("requestsPerSecond", 8.0, 0.1, 10000.0),
        burst=integer("burst", 16, 1, 10000),
        pool_size=integer("poolSize", 4, 1, 64),
        cache_max_entries=integer("cacheMaxEntries", 4096, 1, 1000000),
        max_response_bytes=integer(
            "maxResponseBytes", 536870912, 1024, 2147483647
        ),
    )

    endpoints_raw = raw.get("endpoints")
    if not isinstance(endpoints_raw, list) or not endpoints_raw:
        raise ConfigError("endpoints_must_be_nonempty_array")
    endpoints: list[EndpointConfig] = []
    names: set[str] = set()
    url_env_names: set[str] = set()
    roles: Counter[str] = Counter()
    for item in endpoints_raw:
        if not isinstance(item, dict):
            raise ConfigError("endpoint_must_be_object")
        require_keys(item, {"name", "role", "urlEnv", "headersEnv"}, "endpoint")
        name = item.get("name")
        role = item.get("role")
        url_env = item.get("urlEnv")
        headers_env = item.get("headersEnv")
        if (
            not isinstance(name, str)
            or not re.fullmatch(r"[a-z][a-z0-9-]{0,62}", name)
            or name in names
        ):
            raise ConfigError("invalid_or_duplicate_endpoint_name")
        if role not in CANONICAL_ROLES:
            raise ConfigError("invalid_endpoint_role")
        if not isinstance(url_env, str) or not re.fullmatch(
            r"[A-Z][A-Z0-9_]{1,127}", url_env
        ):
            raise ConfigError("invalid_endpoint_url_env")
        if url_env in url_env_names:
            raise ConfigError("endpoint_url_env_must_be_unique")
        if headers_env is not None and (
            not isinstance(headers_env, str)
            or not re.fullmatch(r"[A-Z][A-Z0-9_]{1,127}", headers_env)
        ):
            raise ConfigError("invalid_endpoint_headers_env")
        names.add(name)
        url_env_names.add(url_env)
        roles[role] += 1
        endpoints.append(EndpointConfig(name, role, url_env, headers_env))

    if roles["witness-primary"] != 1 or roles["witness-standby"] < 1:
        raise ConfigError("one_primary_and_at_least_one_standby_required")
    if len(endpoints) < policy.canonical_quorum:
        raise ConfigError("canonical_quorum_exceeds_endpoint_count")
    if sum(roles[role] for role in WITNESS_ROLES) < policy.witness_ready:
        raise ConfigError("witness_ready_exceeds_witness_endpoint_count")
    return Config(
        path=path.resolve(),
        chain_id=chain_id.lower(),
        genesis_hash=genesis_hash.lower(),
        policy=policy,
        endpoints=tuple(endpoints),
        public_fingerprint=sha256_bytes(canonical_json(raw)),
    )


@dataclasses.dataclass(frozen=True)
class EndpointRuntime:
    config: EndpointConfig
    parsed: urllib.parse.SplitResult
    headers: dict[str, str]

    @property
    def name(self) -> str:
        return self.config.name

    @property
    def role(self) -> str:
        return self.config.role


def resolve_endpoints(config: Config) -> tuple[EndpointRuntime, ...]:
    resolved: list[EndpointRuntime] = []
    origins: set[tuple[str, str, int]] = set()
    for endpoint in config.endpoints:
        raw_url = os.environ.get(endpoint.url_env)
        if not raw_url:
            raise ConfigError(f"missing_endpoint_secret:{endpoint.name}:url")
        parsed = urllib.parse.urlsplit(raw_url)
        if (
            any(ord(character) < 0x20 or ord(character) == 0x7F for character in raw_url)
            or
            parsed.scheme not in {"http", "https"}
            or not parsed.hostname
            or parsed.fragment
        ):
            raise ConfigError(f"invalid_endpoint_secret:{endpoint.name}:url")
        headers: dict[str, str] = {}
        if endpoint.headers_env:
            raw_headers = os.environ.get(endpoint.headers_env)
            if not raw_headers:
                raise ConfigError(
                    f"missing_endpoint_secret:{endpoint.name}:headers"
                )
            try:
                header_value = json.loads(
                    raw_headers,
                    object_pairs_hook=JsonObjectPairs,
                )
            except json.JSONDecodeError as error:
                raise ConfigError(
                    f"invalid_endpoint_secret:{endpoint.name}:headers"
                ) from error
            if not isinstance(header_value, JsonObjectPairs):
                raise ConfigError(
                    f"invalid_endpoint_secret:{endpoint.name}:headers"
                )
            for key, value in header_value:
                normalized_name = key.lower() if isinstance(key, str) else ""
                if (
                    not isinstance(key, str)
                    or not HEADER_NAME_RE.fullmatch(key)
                    or normalized_name in headers
                    or normalized_name in FORBIDDEN_FORWARD_HEADERS
                    or not isinstance(value, str)
                    or any(
                        character in value
                        for character in ("\r", "\n", "\x00")
                    )
                ):
                    raise ConfigError(
                        f"invalid_endpoint_secret:{endpoint.name}:headers"
                    )
                headers[normalized_name] = value
        if parsed.username is not None:
            if "authorization" in headers:
                raise ConfigError(
                    f"conflicting_endpoint_auth:{endpoint.name}"
                )
            username = urllib.parse.unquote(parsed.username)
            password = urllib.parse.unquote(parsed.password or "")
            token = base64.b64encode(f"{username}:{password}".encode()).decode()
            headers["authorization"] = f"Basic {token}"
        headers.setdefault("content-type", "application/json")
        default_port = 443 if parsed.scheme == "https" else 80
        hostname = parsed.hostname
        assert hostname is not None
        try:
            endpoint_port = parsed.port
        except ValueError:
            raise ConfigError(
                f"invalid_endpoint_secret:{endpoint.name}:url"
            ) from None
        try:
            host_identity = ipaddress.ip_address(hostname).compressed
        except ValueError:
            try:
                numeric_ipv4 = socket.inet_aton(hostname)
            except OSError:
                try:
                    host_identity = (
                        hostname.rstrip(".").encode("idna").decode().lower()
                    )
                except UnicodeError as error:
                    raise ConfigError(
                        f"invalid_endpoint_secret:{endpoint.name}:url"
                    ) from error
            else:
                host_identity = str(ipaddress.IPv4Address(numeric_ipv4))
        origin = (parsed.scheme, host_identity, endpoint_port or default_port)
        if origin in origins:
            raise ConfigError("endpoint_origins_must_be_distinct")
        origins.add(origin)
        resolved.append(EndpointRuntime(endpoint, parsed, headers))
    return tuple(resolved)


class TokenBucket:
    def __init__(
        self,
        rate: float,
        burst: int,
        sleeper: Callable[[float], None] = time.sleep,
    ) -> None:
        self.rate = rate
        self.capacity = float(burst)
        self.tokens = float(burst)
        self.updated = time.monotonic()
        self.sleeper = sleeper
        self.lock = threading.Lock()

    def acquire(self) -> None:
        while True:
            with self.lock:
                now = time.monotonic()
                self.tokens = min(
                    self.capacity, self.tokens + (now - self.updated) * self.rate
                )
                self.updated = now
                if self.tokens >= 1.0:
                    self.tokens -= 1.0
                    return
                delay = (1.0 - self.tokens) / self.rate
            self.sleeper(delay)


class ConnectionPool:
    def __init__(self, endpoint: EndpointRuntime, size: int) -> None:
        self.endpoint = endpoint
        self.size = size
        self.connections: queue.LifoQueue[http.client.HTTPConnection] = queue.LifoQueue(
            size
        )
        self.created = 0
        self.lock = threading.Lock()

    def _new(self, timeout: float) -> http.client.HTTPConnection:
        host = self.endpoint.parsed.hostname
        assert host is not None
        port = self.endpoint.parsed.port
        if self.endpoint.parsed.scheme == "https":
            return http.client.HTTPSConnection(
                host,
                port=port,
                timeout=timeout,
                context=ssl.create_default_context(),
            )
        return http.client.HTTPConnection(host, port=port, timeout=timeout)

    @contextlib.contextmanager
    def connection(
        self, timeout: float
    ) -> Iterable[http.client.HTTPConnection]:
        connection: http.client.HTTPConnection | None = None
        reusable = True
        try:
            try:
                connection = self.connections.get_nowait()
            except queue.Empty:
                with self.lock:
                    if self.created < self.size:
                        self.created += 1
                        connection = self._new(timeout)
                if connection is None:
                    try:
                        connection = self.connections.get(timeout=timeout)
                    except queue.Empty as error:
                        raise TimeoutError from error
            connection.timeout = timeout
            if connection.sock is not None:
                connection.sock.settimeout(timeout)
            yield connection
        except BaseException:
            reusable = False
            raise
        finally:
            if connection is not None:
                if reusable:
                    try:
                        self.connections.put_nowait(connection)
                    except queue.Full:
                        connection.close()
                else:
                    connection.close()
                    with self.lock:
                        self.created -= 1

    def close(self) -> None:
        while True:
            try:
                connection = self.connections.get_nowait()
            except queue.Empty:
                break
            connection.close()


class Metrics:
    def __init__(self, output: Path | None = None) -> None:
        self.output = output
        self.started = utc_now()
        self.lock = threading.Lock()
        self.requests = 0
        self.retries = 0
        self.failovers = 0
        self.cache_hits = 0
        self.cache_misses = 0
        self.deduplicated = 0
        self.quorum_checks = 0
        self.quorum_disagreements = 0
        self.by_method: Counter[str] = Counter()
        self.by_endpoint: Counter[str] = Counter()
        self.by_category: Counter[str] = Counter()
        self.latency_ms: defaultdict[str, list[float]] = defaultdict(list)

    def record(
        self,
        *,
        method: str | None = None,
        endpoint: str | None = None,
        category: str | None = None,
        latency_ms: float | None = None,
    ) -> None:
        with self.lock:
            if method:
                self.requests += 1
                self.by_method[method] += 1
            if endpoint:
                self.by_endpoint[endpoint] += 1
            if category:
                self.by_category[category] += 1
            if endpoint and latency_ms is not None:
                values = self.latency_ms[endpoint]
                if len(values) < 10000:
                    values.append(latency_ms)
        self.flush()

    def increment(self, name: str) -> None:
        with self.lock:
            setattr(self, name, getattr(self, name) + 1)
        self.flush()

    def snapshot(self) -> dict[str, Any]:
        with self.lock:
            latency = {}
            for endpoint, values in self.latency_ms.items():
                ordered = sorted(values)
                latency[endpoint] = {
                    "count": len(values),
                    "averageMs": round(sum(values) / len(values), 3),
                    "maxMs": round(max(values), 3),
                    "p95Ms": round(
                        ordered[min(len(ordered) - 1, int(len(ordered) * 0.95))],
                        3,
                    ),
                }
            return {
                "schema": SCHEMA_METRICS,
                "startedAtUtc": self.started,
                "updatedAtUtc": utc_now(),
                "requests": self.requests,
                "retries": self.retries,
                "failovers": self.failovers,
                "cacheHits": self.cache_hits,
                "cacheMisses": self.cache_misses,
                "deduplicatedRequests": self.deduplicated,
                "quorumChecks": self.quorum_checks,
                "quorumDisagreements": self.quorum_disagreements,
                "requestsByMethod": dict(sorted(self.by_method.items())),
                "requestsByEndpoint": dict(sorted(self.by_endpoint.items())),
                "failuresByCategory": dict(sorted(self.by_category.items())),
                "latencyByEndpoint": latency,
                "secretMaterialRecorded": False,
            }

    def flush(self) -> None:
        if self.output is not None:
            atomic_write_json(self.output, self.snapshot())


class DiskCache:
    """Checksummed cache for immutable hash-addressed JSON-RPC results."""

    def __init__(self, root: Path, maximum: int, metrics: Metrics) -> None:
        self.root = root
        self.maximum = maximum
        self.metrics = metrics
        secure_mkdir_directory(
            root,
            0o700,
            "cache_directory_missing_or_symlinked",
        )
        self.lock = threading.Lock()
        self.inflight: dict[str, threading.Event] = {}

    @staticmethod
    def key(method: str, params: list[Any]) -> str:
        return sha256_bytes(canonical_json({"method": method, "params": params}))

    def path_for(self, key: str) -> Path:
        return self.root / f"{key}.json"

    def get(self, key: str) -> Any | None:
        path = self.path_for(key)
        if not path.is_file():
            self.metrics.increment("cache_misses")
            return None
        try:
            envelope = json.loads(path.read_text(encoding="utf-8"))
            result = envelope["result"]
            if (
                envelope.get("schema") != "reth-dtvm.rpc-cache-entry.v1"
                or envelope.get("requestSha256") != key
                or envelope.get("resultSha256")
                != sha256_bytes(canonical_json(result))
            ):
                raise ValueError
        except (OSError, UnicodeError, json.JSONDecodeError, KeyError, ValueError):
            self.metrics.record(category="cache_corrupt")
            self.metrics.increment("cache_misses")
            return None
        self.metrics.increment("cache_hits")
        return result

    def put(self, key: str, result: Any) -> None:
        with self.lock:
            entries = sorted(
                self.root.glob("*.json"), key=lambda item: item.stat().st_mtime
            )
            while len(entries) >= self.maximum:
                entries.pop(0).unlink(missing_ok=True)
            envelope = {
                "schema": "reth-dtvm.rpc-cache-entry.v1",
                "requestSha256": key,
                "resultSha256": sha256_bytes(canonical_json(result)),
                "result": result,
                "secretMaterialRecorded": False,
            }
            atomic_write_json(self.path_for(key), envelope)

    @contextlib.contextmanager
    def leader(self, key: str) -> Iterable[bool]:
        with self.lock:
            event = self.inflight.get(key)
            if event is None:
                event = threading.Event()
                self.inflight[key] = event
                is_leader = True
            else:
                is_leader = False
        if not is_leader:
            self.metrics.increment("deduplicated")
            event.wait()
        try:
            yield is_leader
        finally:
            if is_leader:
                with self.lock:
                    self.inflight.pop(key, None)
                    event.set()


def parse_retry_after(value: str | None) -> float | None:
    if not value:
        return None
    try:
        return max(0.0, float(value))
    except ValueError:
        try:
            parsed = email.utils.parsedate_to_datetime(value)
            return max(0.0, parsed.timestamp() - time.time())
        except (TypeError, ValueError, OverflowError):
            return None


class RpcClient:
    def __init__(
        self,
        config: Config,
        endpoints: tuple[EndpointRuntime, ...],
        metrics: Metrics,
        cache: DiskCache | None = None,
        *,
        sleeper: Callable[[float], None] = time.sleep,
        random_source: random.Random | None = None,
    ) -> None:
        self.config = config
        self.endpoints = endpoints
        self.metrics = metrics
        self.cache = cache
        self.sleeper = sleeper
        self.random = random_source or random.SystemRandom()
        self.pools = {
            endpoint.name: ConnectionPool(endpoint, config.policy.pool_size)
            for endpoint in endpoints
        }
        self.buckets = {
            endpoint.name: TokenBucket(
                config.policy.requests_per_second,
                config.policy.burst,
                sleeper,
            )
            for endpoint in endpoints
        }
        self.ident = 0
        self.ident_lock = threading.Lock()

    def close(self) -> None:
        for pool in self.pools.values():
            pool.close()

    def _next_id(self) -> int:
        with self.ident_lock:
            self.ident += 1
            return self.ident

    def _timeout_for(self, method: str) -> float:
        if method in {
            "debug_executionWitnessByBlockHash",
            "debug_executionWitness",
        }:
            return self.config.policy.witness_timeout
        return self.config.policy.request_timeout

    def _request_endpoint(
        self, endpoint: EndpointRuntime, method: str, params: list[Any]
    ) -> Any:
        self.buckets[endpoint.name].acquire()
        request_id = self._next_id()
        body = canonical_json(
            {
                "jsonrpc": "2.0",
                "id": request_id,
                "method": method,
                "params": params,
            }
        )
        parsed = endpoint.parsed
        path = parsed.path or "/"
        if parsed.query:
            path = f"{path}?{parsed.query}"
        timeout = self._timeout_for(method)
        started = time.monotonic()
        try:
            with self.pools[endpoint.name].connection(timeout) as connection:
                connection.request("POST", path, body=body, headers=endpoint.headers)
                response = connection.getresponse()
                response_body = response.read(self.config.policy.max_response_bytes + 1)
                if len(response_body) > self.config.policy.max_response_bytes:
                    raise RpcFailure("response_too_large", endpoint.name)
                status = response.status
                retry_after = parse_retry_after(response.getheader("retry-after"))
        except RpcFailure:
            raise
        except (TimeoutError, socket.timeout):
            self.metrics.record(
                method=method,
                endpoint=endpoint.name,
                category="timeout",
                latency_ms=(time.monotonic() - started) * 1000,
            )
            raise RpcFailure("timeout", endpoint.name) from None
        except ValueError:
            self.metrics.record(
                method=method,
                endpoint=endpoint.name,
                category="invalid_endpoint_configuration",
                latency_ms=(time.monotonic() - started) * 1000,
            )
            raise RpcFailure("invalid_endpoint_configuration", endpoint.name) from None
        except (OSError, http.client.HTTPException):
            self.metrics.record(
                method=method,
                endpoint=endpoint.name,
                category="transport_error",
                latency_ms=(time.monotonic() - started) * 1000,
            )
            raise RpcFailure("transport_error", endpoint.name) from None

        latency = (time.monotonic() - started) * 1000
        if status == 429:
            self.metrics.record(
                method=method,
                endpoint=endpoint.name,
                category="rate_limited",
                latency_ms=latency,
            )
            raise RpcFailure(
                "rate_limited", endpoint.name, retry_after=retry_after
            )
        if 500 <= status <= 599:
            self.metrics.record(
                method=method,
                endpoint=endpoint.name,
                category="upstream_5xx",
                latency_ms=latency,
            )
            raise RpcFailure("upstream_5xx", endpoint.name)
        if status in {401, 403}:
            self.metrics.record(
                method=method,
                endpoint=endpoint.name,
                category="authentication_failed",
                latency_ms=latency,
            )
            raise RpcFailure("authentication_failed", endpoint.name)
        if status < 200 or status >= 300:
            self.metrics.record(
                method=method,
                endpoint=endpoint.name,
                category="http_error",
                latency_ms=latency,
            )
            raise RpcFailure("http_error", endpoint.name)
        try:
            document = json.loads(response_body)
        except (UnicodeDecodeError, json.JSONDecodeError):
            self.metrics.record(
                method=method,
                endpoint=endpoint.name,
                category="malformed_response",
                latency_ms=latency,
            )
            raise RpcFailure("malformed_response", endpoint.name) from None
        if (
            not isinstance(document, dict)
            or document.get("jsonrpc") != "2.0"
            or document.get("id") != request_id
        ):
            self.metrics.record(
                method=method,
                endpoint=endpoint.name,
                category="malformed_response",
                latency_ms=latency,
            )
            raise RpcFailure("malformed_response", endpoint.name)
        if document.get("error") is not None:
            error = document.get("error")
            code = error.get("code") if isinstance(error, dict) else None
            if code == -32601:
                category = "capability_missing"
            elif code == -32602:
                category = "capability_incompatible"
            else:
                category = "rpc_error"
            self.metrics.record(
                method=method,
                endpoint=endpoint.name,
                category=category,
                latency_ms=latency,
            )
            raise RpcFailure(category, endpoint.name)
        if "result" not in document:
            self.metrics.record(
                method=method,
                endpoint=endpoint.name,
                category="malformed_response",
                latency_ms=latency,
            )
            raise RpcFailure("malformed_response", endpoint.name)
        self.metrics.record(
            method=method,
            endpoint=endpoint.name,
            category="success",
            latency_ms=latency,
        )
        return document["result"]

    def _delay(self, attempt: int, retry_after: float | None) -> None:
        initial = self.config.policy.backoff_initial_ms / 1000.0
        maximum = self.config.policy.backoff_max_ms / 1000.0
        exponential = min(maximum, initial * (2**attempt))
        if retry_after is not None:
            exponential = min(maximum, max(exponential, retry_after))
        jitter = exponential * self.config.policy.jitter_ratio
        delay = max(0.0, exponential + self.random.uniform(-jitter, jitter))
        if delay:
            self.sleeper(delay)

    def _ordered(
        self,
        witness_only: bool,
        eligible_names: set[str] | None = None,
    ) -> list[EndpointRuntime]:
        endpoints = [
            endpoint
            for endpoint in self.endpoints
            if (eligible_names is None or endpoint.name in eligible_names)
            and (not witness_only or endpoint.role in WITNESS_ROLES)
        ]
        role_order = {
            "witness-primary": 0,
            "witness-standby": 1,
            "canonical-aux": 2,
        }
        return sorted(endpoints, key=lambda item: role_order[item.role])

    def request_endpoint_retry(
        self,
        endpoint: EndpointRuntime,
        method: str,
        params: list[Any],
    ) -> Any:
        """Produce one endpoint vote after bounded retries on that endpoint."""
        last: RpcFailure | None = None
        for attempt in range(self.config.policy.max_attempts):
            try:
                return self._request_endpoint(endpoint, method, params)
            except RpcFailure as failure:
                last = failure
                if (
                    failure.category not in RETRIABLE
                    or attempt + 1 >= self.config.policy.max_attempts
                ):
                    raise
                self.metrics.increment("retries")
                self._delay(attempt, failure.retry_after)
        raise last or RpcFailure("request_failed", endpoint.name)

    @staticmethod
    def cacheable(method: str, params: list[Any]) -> bool:
        if method in {"eth_chainId", "eth_getTransactionByHash"}:
            return True
        if method in {
            "eth_getBlockByHash",
            "debug_executionWitnessByBlockHash",
        }:
            return bool(params and isinstance(params[0], str) and HASH_RE.fullmatch(params[0]))
        if method in {"debug_getRawHeader", "debug_getRawBlock"}:
            return bool(
                params
                and isinstance(params[0], dict)
                and isinstance(params[0].get("blockHash"), str)
                and HASH_RE.fullmatch(params[0]["blockHash"])
            )
        if method == "eth_getBlockByNumber":
            return bool(params and str(params[0]).lower() == "0x0")
        return False

    def call(
        self,
        method: str,
        params: list[Any],
        *,
        witness_only: bool = False,
        use_cache: bool = True,
        eligible_names: set[str] | None = None,
    ) -> Any:
        key: str | None = None
        if use_cache and self.cache is not None and self.cacheable(method, params):
            key = self.cache.key(method, params)
            cached = self.cache.get(key)
            if cached is not None:
                return cached
            with self.cache.leader(key) as leader:
                if not leader:
                    cached = self.cache.get(key)
                    if cached is not None:
                        return cached
                result = self._call_failover(
                    method,
                    params,
                    witness_only,
                    eligible_names,
                )
                self.cache.put(key, result)
                return result
        return self._call_failover(method, params, witness_only, eligible_names)

    def _call_failover(
        self,
        method: str,
        params: list[Any],
        witness_only: bool,
        eligible_names: set[str] | None,
    ) -> Any:
        endpoints = self._ordered(witness_only, eligible_names)
        if not endpoints:
            raise RpcFailure("no_eligible_endpoint")
        last: RpcFailure | None = None
        previous_endpoint: str | None = None
        for attempt in range(self.config.policy.max_attempts):
            endpoint = endpoints[attempt % len(endpoints)]
            if previous_endpoint is not None and endpoint.name != previous_endpoint:
                self.metrics.increment("failovers")
            previous_endpoint = endpoint.name
            try:
                return self._request_endpoint(endpoint, method, params)
            except RpcFailure as failure:
                last = failure
                if failure.category == "authentication_failed":
                    raise
                if failure.category in {
                    "authentication_failed",
                    "capability_missing",
                    "capability_incompatible",
                    "malformed_response",
                    "response_too_large",
                    "rpc_error",
                    "http_error",
                } and attempt + 1 >= len(endpoints):
                    break
                if failure.category not in RETRIABLE and len(endpoints) == 1:
                    break
                if attempt + 1 < self.config.policy.max_attempts:
                    self.metrics.increment("retries")
                    self._delay(attempt, failure.retry_after)
        raise last or RpcFailure("request_failed")

    def quorum_call(
        self,
        method: str,
        params: list[Any],
        *,
        expected_key: str | None = None,
        eligible_names: set[str] | None = None,
    ) -> Any:
        self.metrics.increment("quorum_checks")
        groups: defaultdict[str, list[tuple[str, Any]]] = defaultdict(list)
        failures = 0
        for endpoint in self._ordered(False, eligible_names):
            try:
                result = self.request_endpoint_retry(endpoint, method, params)
                key = self._quorum_key(method, result)
                groups[key].append((endpoint.name, result))
            except RpcFailure as failure:
                if failure.category == "authentication_failed":
                    raise
                failures += 1
        winners = [
            values
            for key, values in groups.items()
            if len(values) >= self.config.policy.canonical_quorum
            and (expected_key is None or key == expected_key)
        ]
        if len(winners) != 1:
            self.metrics.increment("quorum_disagreements")
            category = (
                "canonical_quorum_unavailable"
                if failures and not groups
                else "canonical_quorum_disagreement"
            )
            raise RpcFailure(category)
        return winners[0][0][1]

    @staticmethod
    def _quorum_key(method: str, result: Any) -> str:
        if method == "eth_chainId":
            if not isinstance(result, str):
                raise RpcFailure("malformed_response")
            return result.lower()
        if method == "eth_getBlockByNumber":
            if (
                not isinstance(result, dict)
                or not isinstance(result.get("number"), str)
                or not isinstance(result.get("hash"), str)
            ):
                raise RpcFailure("malformed_response")
            return f"{result['number'].lower()}:{result['hash'].lower()}"
        return sha256_bytes(canonical_json(result))


def valid_block(result: Any) -> bool:
    return bool(
        isinstance(result, dict)
        and isinstance(result.get("number"), str)
        and QUANTITY_RE.fullmatch(result["number"])
        and isinstance(result.get("hash"), str)
        and HASH_RE.fullmatch(result["hash"])
    )


def probe_endpoint(
    client: RpcClient,
    endpoint: EndpointRuntime,
    config: Config,
    selector: str,
    expected_pin: dict[str, str] | None,
    *,
    probe_witness: bool,
) -> dict[str, Any]:
    report: dict[str, Any] = {
        "name": endpoint.name,
        "role": endpoint.role,
        "configured": True,
        "chainId": False,
        "genesisHash": False,
        "syncing": False,
        "finalized": False,
        "capabilities": {
            "eth": False,
            "debugGetRawHeaderByHashCanonical": False,
            "debugExecutionWitnessByBlockHashCanonical": False,
            "debugGetRawBlockByHashCanonical": False,
        },
        "ready": False,
        "failureCategory": None,
    }
    try:
        chain_id = client.request_endpoint_retry(endpoint, "eth_chainId", [])
        report["chainId"] = (
            isinstance(chain_id, str) and chain_id.lower() == config.chain_id
        )
        if not report["chainId"]:
            raise RpcFailure("chain_mismatch", endpoint.name)
        genesis = client.request_endpoint_retry(
            endpoint, "eth_getBlockByNumber", ["0x0", False]
        )
        report["genesisHash"] = bool(
            valid_block(genesis)
            and genesis["number"].lower() == "0x0"
            and genesis["hash"].lower() == config.genesis_hash
        )
        if not report["genesisHash"]:
            raise RpcFailure("genesis_mismatch", endpoint.name)
        syncing = client.request_endpoint_retry(endpoint, "eth_syncing", [])
        report["syncing"] = syncing is False
        if not report["syncing"]:
            raise RpcFailure("endpoint_syncing", endpoint.name)
        block = client.request_endpoint_retry(
            endpoint, "eth_getBlockByNumber", [selector, False]
        )
        report["finalized"] = valid_block(block)
        if not report["finalized"]:
            raise RpcFailure("finalized_missing", endpoint.name)
        report["blockNumber"] = block["number"].lower()
        report["blockHash"] = block["hash"].lower()
        if expected_pin is not None and (
            block["number"].lower() != expected_pin["numberHex"].lower()
            or block["hash"].lower() != expected_pin["hash"].lower()
        ):
            raise RpcFailure("finalized_hash_drift", endpoint.name)
        report["capabilities"]["eth"] = True
        if probe_witness:
            block_hash = block["hash"].lower()
            raw_header = client.request_endpoint_retry(
                endpoint,
                "debug_getRawHeader",
                [{"blockHash": block_hash, "requireCanonical": True}],
            )
            if not isinstance(raw_header, str) or not HEX_BYTES_RE.fullmatch(raw_header):
                raise RpcFailure("malformed_response", endpoint.name)
            report["capabilities"]["debugGetRawHeaderByHashCanonical"] = True
            witness = client.request_endpoint_retry(
                endpoint,
                "debug_executionWitnessByBlockHash",
                [block_hash, "canonical"],
            )
            if not (
                isinstance(witness, dict)
                and isinstance(witness.get("state"), list)
                and isinstance(witness.get("codes"), list)
                and isinstance(witness.get("headers"), list)
            ):
                raise RpcFailure("malformed_response", endpoint.name)
            report["capabilities"][
                "debugExecutionWitnessByBlockHashCanonical"
            ] = True
            raw_block = client.request_endpoint_retry(
                endpoint,
                "debug_getRawBlock",
                [{"blockHash": block_hash, "requireCanonical": True}],
            )
            if not isinstance(raw_block, str) or not HEX_BYTES_RE.fullmatch(raw_block):
                raise RpcFailure("malformed_response", endpoint.name)
            report["capabilities"]["debugGetRawBlockByHashCanonical"] = True
        report["ready"] = bool(
            report["chainId"]
            and report["genesisHash"]
            and report["syncing"]
            and report["finalized"]
            and (
                endpoint.role not in WITNESS_ROLES
                or all(report["capabilities"].values())
            )
        )
    except RpcFailure as failure:
        report["failureCategory"] = failure.category
    return report


def readiness(
    config: Config,
    endpoints: tuple[EndpointRuntime, ...],
    client: RpcClient,
    *,
    frozen_pin: dict[str, Any] | None = None,
) -> dict[str, Any]:
    selector = (
        frozen_pin["numberHex"]
        if frozen_pin is not None
        else "finalized"
    )
    expected_pin = frozen_pin
    reports = [
        probe_endpoint(
            client,
            endpoint,
            config,
            selector,
            expected_pin,
            probe_witness=endpoint.role in WITNESS_ROLES,
        )
        for endpoint in endpoints
    ]
    canonical_groups: defaultdict[str, list[str]] = defaultdict(list)
    for report in reports:
        if (
            report["chainId"]
            and report["genesisHash"]
            and report["syncing"]
            and report["finalized"]
        ):
            key = f"{report['blockNumber']}:{report['blockHash']}"
            canonical_groups[key].append(report["name"])
    winners = [
        (key, names)
        for key, names in canonical_groups.items()
        if len(names) >= config.policy.canonical_quorum
    ]
    witness_ready = [
        report["name"]
        for report in reports
        if report["role"] in WITNESS_ROLES and report["ready"]
    ]
    success = len(winners) == 1 and len(witness_ready) >= config.policy.witness_ready
    pinned: dict[str, Any] | None = None
    if len(winners) == 1:
        number, block_hash = winners[0][0].split(":", 1)
        pinned = {
            "numberHex": number,
            "number": int(number, 16),
            "hash": block_hash,
            "agreedEndpoints": winners[0][1],
        }
    failures = sorted(
        {
            report["failureCategory"]
            for report in reports
            if report["failureCategory"] is not None
        }
    )
    if len(winners) != 1:
        failures.append("canonical_quorum_disagreement")
    if len(witness_ready) < config.policy.witness_ready:
        failures.append("witness_quorum_unavailable")
    return {
        "schema": SCHEMA_READINESS,
        "status": "ready" if success else "not_ready",
        "success": success,
        "checkedAtUtc": utc_now(),
        "expectedChain": {
            "chainId": config.chain_id,
            "genesisHash": config.genesis_hash,
        },
        "canonicalQuorum": {
            "required": config.policy.canonical_quorum,
            "satisfied": len(winners) == 1,
        },
        "witnessReadiness": {
            "required": config.policy.witness_ready,
            "readyEndpoints": witness_ready,
            "satisfied": len(witness_ready) >= config.policy.witness_ready,
        },
        "frozenPin": pinned,
        "endpoints": reports,
        "failureCategories": sorted(set(failures)),
        "secretMaterialRecorded": False,
    }


class Gateway:
    def __init__(
        self,
        client: RpcClient,
        readiness_report: dict[str, Any],
        metrics: Metrics,
    ) -> None:
        self.client = client
        self.readiness_report = readiness_report
        self.metrics = metrics
        self.server: http.server.ThreadingHTTPServer | None = None
        self.thread: threading.Thread | None = None
        self.path_token = secrets.token_urlsafe(32)
        self.eligible_names = {
            endpoint["name"]
            for endpoint in readiness_report.get("endpoints", [])
            if endpoint.get("ready") is True
        }

    def start(self) -> str:
        gateway = self

        class Handler(http.server.BaseHTTPRequestHandler):
            server_version = "reth-dtvm-ha"

            def log_message(self, _format: str, *_args: Any) -> None:
                return

            def _json(self, status: int, value: Any) -> None:
                body = canonical_json(value) + b"\n"
                self.send_response(status)
                self.send_header("content-type", "application/json")
                self.send_header("content-length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def do_GET(self) -> None:
                prefix = f"/{gateway.path_token}/"
                if self.path == prefix + "livez":
                    self._json(200, {"status": "live"})
                elif self.path == prefix + "readyz":
                    status = 200 if gateway.readiness_report["success"] else 503
                    self._json(status, gateway.readiness_report)
                elif self.path == prefix + "metrics":
                    self._json(200, gateway.metrics.snapshot())
                else:
                    self._json(404, {"status": "not_found"})

            def do_POST(self) -> None:
                request: Any = None
                try:
                    if self.path != f"/{gateway.path_token}/":
                        self._json(404, {"status": "not_found"})
                        return
                    length = int(self.headers.get("content-length", "0"))
                    if length <= 0 or length > 16 * 1024 * 1024:
                        raise ValueError
                    request = json.loads(self.rfile.read(length))
                    if (
                        not isinstance(request, dict)
                        or request.get("jsonrpc") != "2.0"
                        or not isinstance(request.get("method"), str)
                        or not isinstance(request.get("params", []), list)
                    ):
                        raise ValueError
                    request_id = request.get("id")
                    method = request["method"]
                    params = request.get("params", [])
                    allowed_methods = {
                        "eth_chainId",
                        "eth_getBlockByNumber",
                        "eth_getBlockByHash",
                        "eth_getTransactionByHash",
                        "debug_getRawHeader",
                        "debug_executionWitnessByBlockHash",
                        "debug_getRawBlock",
                    }
                    if method not in allowed_methods:
                        raise RpcFailure("method_not_allowed")
                    valid_params = (
                        (method == "eth_chainId" and params == [])
                        or (
                            method == "eth_getBlockByNumber"
                            and len(params) == 2
                            and isinstance(params[0], str)
                            and (
                                params[0] == "finalized"
                                or QUANTITY_RE.fullmatch(params[0])
                            )
                            and isinstance(params[1], bool)
                        )
                        or (
                            method == "eth_getBlockByHash"
                            and len(params) == 2
                            and isinstance(params[0], str)
                            and HASH_RE.fullmatch(params[0])
                            and isinstance(params[1], bool)
                        )
                        or (
                            method == "eth_getTransactionByHash"
                            and len(params) == 1
                            and isinstance(params[0], str)
                            and HASH_RE.fullmatch(params[0])
                        )
                        or (
                            method
                            in {
                                "debug_getRawHeader",
                                "debug_getRawBlock",
                            }
                            and len(params) == 1
                            and isinstance(params[0], dict)
                            and set(params[0]) == {"blockHash", "requireCanonical"}
                            and isinstance(params[0]["blockHash"], str)
                            and HASH_RE.fullmatch(params[0]["blockHash"])
                            and params[0]["requireCanonical"] is True
                        )
                        or (
                            method == "debug_executionWitnessByBlockHash"
                            and len(params) == 2
                            and isinstance(params[0], str)
                            and HASH_RE.fullmatch(params[0])
                            and params[1] == "canonical"
                        )
                    )
                    if not valid_params:
                        raise RpcFailure("method_parameters_not_allowed")
                    frozen = gateway.readiness_report.get("frozenPin")
                    if (
                        method == "eth_getBlockByNumber"
                        and params
                        and params[0] == "finalized"
                        and frozen is not None
                    ):
                        result = gateway.client.quorum_call(
                            "eth_getBlockByNumber",
                            [frozen["numberHex"], bool(params[1]) if len(params) > 1 else False],
                            expected_key=f"{frozen['numberHex']}:{frozen['hash']}",
                            eligible_names=gateway.eligible_names,
                        )
                    elif method in {"eth_chainId", "eth_getBlockByNumber"}:
                        result = gateway.client.quorum_call(
                            method,
                            params,
                            eligible_names=gateway.eligible_names,
                        )
                    else:
                        result = gateway.client.call(
                            method,
                            params,
                            witness_only=method.startswith("debug_"),
                            eligible_names=gateway.eligible_names,
                        )
                    self._json(
                        200,
                        {
                            "jsonrpc": "2.0",
                            "id": request_id,
                            "result": result,
                        },
                    )
                except (ValueError, json.JSONDecodeError):
                    self._json(
                        400,
                        {
                            "jsonrpc": "2.0",
                            "id": None,
                            "error": {"code": -32600, "message": "invalid request"},
                        },
                    )
                except RpcFailure as failure:
                    self._json(
                        503,
                        {
                            "jsonrpc": "2.0",
                            "id": request.get("id") if isinstance(request, dict) else None,
                            "error": {
                                "code": -32098,
                                "message": failure.category,
                            },
                        },
                    )

        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        host, port = self.server.server_address
        return f"http://{host}:{port}/{self.path_token}/"

    def stop(self) -> None:
        if self.server is not None:
            self.server.shutdown()
            self.server.server_close()
        if self.thread is not None:
            self.thread.join(timeout=5)


@contextlib.contextmanager
def workflow_lock(state_dir: Path) -> Iterable[None]:
    state_dir = lexical_absolute(state_dir)
    secure_mkdir_directory(
        state_dir,
        0o700,
        "state_directory_missing_or_symlinked",
    )
    lock_path = state_dir / "workflow.lock"
    with lock_path.open("a+b") as stream:
        os.chmod(lock_path, 0o600)
        try:
            fcntl.flock(stream, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError as error:
            raise ConfigError("workflow_already_running") from error
        yield


def sanitized_environment(config: Config) -> dict[str, str]:
    environment = dict(os.environ)
    for name in config.secret_env_names:
        environment.pop(name, None)
    for name in list(environment):
        if SECRET_NAME_RE.search(name) and name.startswith(("RETH_RPC_", "RPC_")):
            environment.pop(name, None)
    return environment


def offline_environment(config: Config) -> dict[str, str]:
    environment = sanitized_environment(config)
    proxy_names = {
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "NO_PROXY",
    }
    for name in list(environment):
        upper = name.upper()
        if (
            upper in proxy_names
            or (
                any(prefix in upper for prefix in ("RPC", "RETH", "WEB3"))
                and any(
                    suffix in upper
                    for suffix in (
                        "URL",
                        "TOKEN",
                        "HEADER",
                        "SECRET",
                        "AUTH",
                        "COOKIE",
                        "PASSWORD",
                    )
                )
            )
        ):
            environment.pop(name, None)
    environment["CARGO_NET_OFFLINE"] = "true"
    environment["PYTHONDONTWRITEBYTECODE"] = "1"
    return environment


def validate_replayer_approval(path: Path) -> dict[str, Any]:
    manifest_path = lexical_absolute(path)
    require_regular_no_symlink(
        manifest_path,
        "approved_replayer_manifest_missing_or_symlinked",
    )
    value = safe_json_load(manifest_path)
    replayer = value.get("replayer") if isinstance(value, dict) else None
    approval = value.get("approval") if isinstance(value, dict) else None
    correctness = value.get("correctness") if isinstance(value, dict) else None
    if (
        not isinstance(value, dict)
        or value.get("schema") != "reth-dtvm.approved-replayer.v1"
        or not isinstance(replayer, dict)
        or not isinstance(approval, dict)
        or not isinstance(correctness, dict)
        or correctness.get("sealed") is not True
        or correctness.get("passed") is not True
        or approval.get("replayerIdentityApproved") is not True
        or approval.get("mustMatchRealpathAndSha256") is not True
        or not isinstance(replayer.get("realpath"), str)
        or not Path(replayer["realpath"]).is_absolute()
        or not isinstance(replayer.get("sha256"), str)
        or not re.fullmatch(r"[0-9a-fA-F]{64}", replayer["sha256"])
    ):
        raise ConfigError("approved_replayer_manifest_contract_failed")
    binary = Path(replayer["realpath"])
    require_regular_no_symlink(binary, "approved_replayer_binary_missing_or_symlinked")
    expected_binary_sha = replayer["sha256"].lower()
    if sha256_file(binary) != expected_binary_sha:
        raise ConfigError("approved_replayer_binary_checksum_mismatch")
    return {
        "role": "downstream_replayer_identity",
        "manifestRealpath": str(manifest_path),
        "manifestSha256": sha256_file(manifest_path),
        "replayer": {
            "realpath": str(binary),
            "sha256": expected_binary_sha,
        },
    }


def initialize_or_load_state(
    state_path: Path,
    config: Config,
    output: Path,
    count: int,
) -> dict[str, Any]:
    if state_path.is_file():
        state = safe_json_load(state_path)
        if (
            not isinstance(state, dict)
            or state.get("schema") != SCHEMA_STATE
            or state.get("configFingerprint") != config.public_fingerprint
            or state.get("requestedCount") != count
            or state.get("output") != str(output)
        ):
            raise ConfigError("resume_state_mismatch")
        return state
    return {
        "schema": SCHEMA_STATE,
        "status": "in_progress",
        "phase": "initializing",
        "createdAtUtc": utc_now(),
        "updatedAtUtc": utc_now(),
        "configFingerprint": config.public_fingerprint,
        "requestedTag": "finalized",
        "requestedCount": count,
        "output": str(output),
        "frozenPin": None,
        "lastFailureCategory": None,
        "captureAttempts": 0,
        "networkCaptureCompleted": False,
        "strictReplayCompleted": False,
        "evidenceSealed": False,
        "credentialsRecorded": False,
    }


def update_state(state_path: Path, state: dict[str, Any], **changes: Any) -> None:
    state.update(changes)
    state["updatedAtUtc"] = utc_now()
    atomic_write_json(state_path, state)


def validate_and_checksum_corpus(
    stage: Path,
    expected_count: int,
    expected_pin: dict[str, Any] | None = None,
    *,
    expected_replayer: dict[str, Any],
    write_checksums: bool = True,
) -> dict[str, Any]:
    stage = lexical_absolute(stage)
    if (
        not stage.is_dir()
        or stage.is_symlink()
        or stage.resolve(strict=True) != stage
    ):
        raise ConfigError("capture_output_missing_or_symlinked")
    manifest_path = stage / "manifest.json"
    require_regular_no_symlink(manifest_path, "capture_manifest_missing_or_symlinked")
    manifest = safe_json_load(manifest_path)
    if (
        not isinstance(manifest, dict)
        or manifest.get("schema") != "reth-dtvm.atomic-capture-window.v1"
        or manifest.get("success") is not True
        or manifest.get("count") != expected_count
        or not isinstance(manifest.get("blocks"), list)
        or len(manifest["blocks"]) != expected_count
        or manifest.get("rpcUrlRecorded") is not False
    ):
        raise ConfigError("capture_manifest_contract_failed")
    replayer_identity = manifest.get("replayerIdentity")
    replayer = (
        replayer_identity.get("replayer")
        if isinstance(replayer_identity, dict)
        else None
    )
    if (
        not isinstance(replayer_identity, dict)
        or replayer_identity.get("role") != expected_replayer["role"]
        or replayer_identity.get("manifestRealpath")
        != expected_replayer["manifestRealpath"]
        or replayer_identity.get("manifestSha256")
        != expected_replayer["manifestSha256"]
        or not isinstance(replayer, dict)
        or replayer.get("realpath") != expected_replayer["replayer"]["realpath"]
        or str(replayer.get("sha256", "")).lower()
        != expected_replayer["replayer"]["sha256"]
    ):
        raise ConfigError("capture_replayer_identity_mismatch")
    if expected_pin is not None:
        pinned = manifest.get("pinnedHead")
        if (
            manifest.get("requestedTag") != "finalized"
            or not isinstance(pinned, dict)
            or pinned.get("numberHex", "").lower()
            != expected_pin["numberHex"].lower()
            or pinned.get("hash", "").lower() != expected_pin["hash"].lower()
        ):
            raise ConfigError("capture_manifest_pin_mismatch")
        expected_first = expected_pin["number"] - expected_count + 1
        if expected_first < 0:
            raise ConfigError("capture_manifest_range_invalid")
    else:
        expected_first = None
    if (
        manifest.get("canonicalRecheck", {}).get("checkedCount") != expected_count
        or manifest.get("canonicalRecheck", {}).get("allPinnedHashesUnchanged")
        is not True
        or manifest.get("witness", {}).get("method")
        != "debug_executionWitnessByBlockHash"
        or manifest.get("witness", {}).get("mode") != "canonical"
        or manifest.get("witness", {}).get("policy") != "production"
    ):
        raise ConfigError("capture_manifest_safety_contract_failed")
    entries: list[dict[str, Any]] = []
    bundle_paths: set[str] = set()
    previous_hash: str | None = None
    for index, block in enumerate(manifest["blocks"]):
        if not isinstance(block, dict):
            raise ConfigError("invalid_bundle_manifest_entry")
        relative = block.get("bundle")
        expected = block.get("bundleSha256")
        if (
            not isinstance(block.get("number"), int)
            or isinstance(block.get("number"), bool)
            or not isinstance(block.get("numberHex"), str)
            or not QUANTITY_RE.fullmatch(block["numberHex"])
            or int(block["numberHex"], 16) != block["number"]
            or not isinstance(block.get("hash"), str)
            or not HASH_RE.fullmatch(block["hash"])
            or not isinstance(block.get("parentHash"), str)
            or not HASH_RE.fullmatch(block["parentHash"])
            or (
                expected_first is not None
                and block["number"] != expected_first + index
            )
            or (previous_hash is not None and block["parentHash"].lower() != previous_hash)
            or not isinstance(relative, str)
            or Path(relative).is_absolute()
            or ".." in Path(relative).parts
            or relative in bundle_paths
            or not isinstance(expected, str)
            or not re.fullmatch(r"[0-9a-f]{64}", expected)
        ):
            raise ConfigError("invalid_bundle_manifest_entry")
        bundle_paths.add(relative)
        previous_hash = block["hash"].lower()
        bundle = stage / relative
        require_regular_no_symlink(bundle, "bundle_missing_or_symlinked")
        actual = sha256_file(bundle)
        if actual != expected:
            raise ConfigError("bundle_checksum_mismatch")
        entries.append(
            {
                "number": block["number"],
                "hash": block["hash"],
                "path": relative,
                "sha256": actual,
            }
        )
    if expected_pin is not None and previous_hash != expected_pin["hash"].lower():
        raise ConfigError("capture_manifest_pin_mismatch")
    checksum_lines = [
        f"{entry['sha256']}  {entry['path']}" for entry in sorted(entries, key=lambda x: x["path"])
    ]
    checksum_bytes = ("\n".join(checksum_lines) + "\n").encode()
    summary = {
        "schema": "reth-dtvm.bundle-checksums.v1",
        "blockCount": len(entries),
        "bundles": entries,
        "bundleSetSha256": sha256_bytes(checksum_bytes),
        "manifestSha256": sha256_file(manifest_path),
    }
    if write_checksums:
        atomic_write_bytes(stage / "BUNDLE_SHA256SUMS", checksum_bytes, 0o400)
        atomic_write_json(stage / "bundle-checksums.json", summary, 0o400)
    else:
        checksum_path = stage / "BUNDLE_SHA256SUMS"
        summary_path = stage / "bundle-checksums.json"
        if (
            not checksum_path.is_file()
            or checksum_path.is_symlink()
            or checksum_path.resolve(strict=True) != checksum_path
            or checksum_path.read_bytes() != checksum_bytes
            or not summary_path.is_file()
            or summary_path.is_symlink()
            or summary_path.resolve(strict=True) != summary_path
            or safe_json_load(summary_path) != summary
        ):
            raise ConfigError("published_checksum_evidence_mismatch")
    return summary


def validate_replay_report(
    report: Any,
    expected_count: int,
    expected_library_sha256: str,
    expected_replayer_realpath: str,
    expected_replayer_sha256: str,
    expected_manifest_sha256: str,
    expected_blocks: list[dict[str, Any]],
) -> bool:
    if not isinstance(report, dict):
        return False
    corpus = report.get("corpus")
    correctness = report.get("correctness")
    dtvm = report.get("dtvm")
    replayer = report.get("replayer")
    timing = report.get("timingQualification")
    if not (
        report.get("schema") == "reth-dtvm.corpus-correctness.v1"
        and isinstance(corpus, dict)
        and isinstance(corpus.get("manifestSha256"), str)
        and corpus["manifestSha256"].lower()
        == expected_manifest_sha256
        and corpus.get("blockCount") == expected_count
        and isinstance(correctness, dict)
        and correctness.get("passed") is True
        and isinstance(dtvm, dict)
        and isinstance(dtvm.get("librarySha256"), str)
        and dtvm["librarySha256"].lower()
        == expected_library_sha256
        and dtvm.get("loadedFromVerifiedSealedMemfd") is True
        and isinstance(replayer, dict)
        and replayer.get("realpath") == expected_replayer_realpath
        and isinstance(replayer.get("sha256"), str)
        and replayer["sha256"].lower()
        == expected_replayer_sha256
        and isinstance(timing, dict)
        and timing.get(
            "excludesFromFormalPr577PerformanceConclusion"
        )
        is True
    ):
        return False
    blocks = correctness.get("blockResults")
    if (
        not isinstance(blocks, list)
        or len(blocks) != expected_count
        or len(expected_blocks) != expected_count
    ):
        return False
    for block, expected in zip(blocks, expected_blocks, strict=True):
        if not (
            isinstance(block, dict)
            and block.get("blockNumber") == expected.get("number")
            and isinstance(block.get("blockHash"), str)
            and block["blockHash"].lower() == str(expected.get("hash", "")).lower()
            and block.get("bundle") == expected.get("path")
            and isinstance(block.get("bundleSha256"), str)
            and block["bundleSha256"].lower() == expected.get("sha256")
            and block.get("correctnessPassed") is True
            and block.get("differentialMatch") is True
            and block.get("rawBound") is True
            and block.get("preExecutionCommitments") is True
            and block.get("preStateRootVerified") is True
            and block.get("postStateRootVerified") is True
            and all(
                block.get("postExecutionCommitments", {}).get(name) is True
                for name in (
                    "gasUsed",
                    "receiptsRoot",
                    "logsBloom",
                    "requestsHash",
                    "blobGasUsed",
                )
            )
        ):
            return False
    return True


def run_capture_workflow(args: argparse.Namespace, config: Config) -> dict[str, Any]:
    output = lexical_absolute(Path(args.output))
    state_dir = lexical_absolute(Path(args.state_dir))
    state_path = state_dir / "resume-state.json"
    approved_replayer = validate_replayer_approval(Path(args.replayer_manifest))
    with workflow_lock(state_dir):
        metrics = Metrics(state_dir / "metrics.json")
        cache = DiskCache(
            state_dir / "rpc-cache",
            config.policy.cache_max_entries,
            metrics,
        )
        endpoints = resolve_endpoints(config)
        client = RpcClient(config, endpoints, metrics, cache)
        state = initialize_or_load_state(state_path, config, output, args.count)
        if state.get("approvedReplayer") is None:
            update_state(
                state_path,
                state,
                approvedReplayer=approved_replayer,
            )
        elif state.get("approvedReplayer") != approved_replayer:
            raise ConfigError("resume_replayer_identity_mismatch")
        frozen = state.get("frozenPin")
        if output.exists():
            if frozen is None:
                raise ConfigError("published_output_without_frozen_pin")
            checksums = validate_and_checksum_corpus(
                output,
                args.count,
                frozen,
                expected_replayer=approved_replayer,
                write_checksums=False,
            )
            if (
                checksums["bundleSetSha256"] != state.get("bundleSetSha256")
                or checksums["manifestSha256"] != state.get("manifestSha256")
            ):
                raise ConfigError("published_capture_state_mismatch")
            if state.get("phase") == "checksummed":
                directory_fd = os.open(output.parent, os.O_RDONLY | os.O_DIRECTORY)
                try:
                    os.fsync(directory_fd)
                finally:
                    os.close(directory_fd)
                update_state(
                    state_path,
                    state,
                    phase="published",
                    status="success",
                    publishedAtUtc=utc_now(),
                    outputPublished=True,
                    atomicPublication=True,
                    publicationRecovered=True,
                )
                client.close()
                return state
            if state.get("phase") in {"published", "replayed", "sealed"}:
                client.close()
                return state
            raise ConfigError("output_exists")
        report = readiness(config, endpoints, client, frozen_pin=frozen)
        atomic_write_json(state_dir / "readiness.json", report)
        if not report["success"]:
            update_state(
                state_path,
                state,
                phase="not_ready",
                status="blocked",
                lastFailureCategory="endpoint_not_ready",
            )
            client.close()
            raise RpcFailure("endpoint_not_ready")
        if frozen is None:
            frozen = report["frozenPin"]
            update_state(
                state_path,
                state,
                phase="ready",
                status="in_progress",
                frozenPin=frozen,
                capabilityMatrix=report["endpoints"],
                lastFailureCategory=None,
            )

        stage = output.parent / f".{output.name}.reth-ha-stage"
        if stage.exists() and state.get("phase") not in {"captured", "checksummed"}:
            try:
                validate_and_checksum_corpus(
                    stage,
                    args.count,
                    frozen,
                    expected_replayer=approved_replayer,
                )
            except ConfigError as error:
                raise ConfigError("unowned_or_partial_capture_stage") from error
            update_state(
                state_path,
                state,
                phase="captured",
                status="in_progress",
                networkCaptureCompleted=True,
                stagingRecovered=True,
                lastFailureCategory=None,
            )
        if state.get("phase") not in {"captured", "checksummed"}:
            gateway = Gateway(client, report, metrics)
            local_url = gateway.start()
            environment = sanitized_environment(config)
            environment["RETH_RPC_URL"] = local_url
            if args.verify_witness:
                environment["CAPTURE_WINDOW_VERIFY_WITNESS"] = str(
                    Path(args.verify_witness).resolve()
                )
            command = [
                str(Path(args.capture_script).resolve()),
                "--tag",
                "finalized",
                "--count",
                str(args.count),
                "--max-attempts",
                str(args.capture_attempts),
                "--output",
                str(stage),
                "--dtvm-identity-manifest",
                str(Path(args.dtvm_identity_manifest).resolve()),
            ]
            command.extend(
                [
                    "--replayer-manifest",
                    approved_replayer["manifestRealpath"],
                ]
            )
            update_state(
                state_path,
                state,
                phase="capturing",
                captureAttempts=state["captureAttempts"] + 1,
            )
            try:
                process = subprocess.run(
                    command,
                    env=environment,
                    capture_output=True,
                    text=True,
                    timeout=args.capture_timeout,
                    check=False,
                )
            except subprocess.TimeoutExpired:
                process = None
            finally:
                gateway.stop()
                metrics.flush()
            if process is None or process.returncode != 0:
                category = "capture_timeout" if process is None else "capture_failed"
                if process is not None:
                    with contextlib.suppress(json.JSONDecodeError):
                        document = json.loads(process.stdout)
                        if isinstance(document, dict) and isinstance(
                            document.get("failureCategory"), str
                        ):
                            category = document["failureCategory"]
                update_state(
                    state_path,
                    state,
                    phase="capture_failed",
                    status="in_progress",
                    lastFailureCategory=category,
                    networkCaptureCompleted=False,
                )
                client.close()
                raise RpcFailure(category)
            if not stage.is_dir():
                client.close()
                raise RpcFailure("capture_stage_missing")
            update_state(
                state_path,
                state,
                phase="captured",
                status="in_progress",
                networkCaptureCompleted=True,
                lastFailureCategory=None,
            )

        checksums = validate_and_checksum_corpus(
            stage,
            args.count,
            frozen,
            expected_replayer=approved_replayer,
        )
        update_state(
            state_path,
            state,
            phase="checksummed",
            bundleSetSha256=checksums["bundleSetSha256"],
            manifestSha256=checksums["manifestSha256"],
        )
        atomic_publish_directory_noreplace(stage, output)
        directory_fd = os.open(output.parent, os.O_RDONLY | os.O_DIRECTORY)
        try:
            os.fsync(directory_fd)
        finally:
            os.close(directory_fd)
        update_state(
            state_path,
            state,
            phase="published",
            status="success",
            publishedAtUtc=utc_now(),
            outputPublished=True,
            atomicPublication=True,
        )
    client.close()
    return state


def run_replay(args: argparse.Namespace, config: Config) -> dict[str, Any]:
    state_dir = lexical_absolute(Path(args.state_dir))
    with workflow_lock(state_dir):
        return run_replay_locked(args, config, state_dir)


def run_replay_locked(
    args: argparse.Namespace,
    config: Config,
    state_dir: Path,
) -> dict[str, Any]:
    state_path = state_dir / "resume-state.json"
    state = safe_json_load(state_path)
    if (
        not isinstance(state, dict)
        or state.get("schema") != SCHEMA_STATE
        or state.get("phase") not in {"published", "replayed", "sealed"}
    ):
        raise ConfigError("capture_not_published")
    if state.get("configFingerprint") != config.public_fingerprint:
        raise ConfigError("replay_config_fingerprint_mismatch")
    output = Path(state["output"])
    replay_output = lexical_absolute(Path(args.replay_output))
    approved_replayer = state.get("approvedReplayer")
    if (
        not isinstance(approved_replayer, dict)
        or validate_replayer_approval(
            Path(approved_replayer.get("manifestRealpath", ""))
        )
        != approved_replayer
    ):
        raise ConfigError("approved_replayer_identity_changed")
    expected_script_sha = args.verify_corpus_sha256.lower()
    expected_library_sha = args.dtvm_library_sha256.lower()
    if not re.fullmatch(r"[0-9a-f]{64}", expected_script_sha) or not re.fullmatch(
        r"[0-9a-f]{64}", expected_library_sha
    ):
        raise ConfigError("invalid_replay_identity_sha256")
    verify_script = lexical_absolute(Path(args.verify_corpus_script))
    dtvm_library = lexical_absolute(Path(args.dtvm_library))
    require_regular_no_symlink(verify_script, "replay_identity_mismatch")
    require_regular_no_symlink(dtvm_library, "replay_identity_mismatch")
    if (
        sha256_file(verify_script) != expected_script_sha
        or sha256_file(dtvm_library) != expected_library_sha
    ):
        raise ConfigError("replay_identity_mismatch")
    checksums = validate_and_checksum_corpus(
        output,
        state["requestedCount"],
        state["frozenPin"],
        expected_replayer=approved_replayer,
        write_checksums=False,
    )
    if (
        checksums["manifestSha256"] != state.get("manifestSha256")
        or checksums["bundleSetSha256"] != state.get("bundleSetSha256")
    ):
        raise ConfigError("capture_evidence_changed_before_replay")
    if state.get("strictReplayCompleted") is True:
        if (
            replay_output != Path(state.get("replayOutput", ""))
            or not replay_output.is_dir()
            or replay_output.is_symlink()
        ):
            raise ConfigError("replay_state_output_mismatch")
        result = replay_output / "result.json"
        require_regular_no_symlink(result, "replay_state_output_missing")
        if (
            sha256_file(result) != state.get("replayResultSha256")
            or not validate_replay_report(
                safe_json_load(result),
                state["requestedCount"],
                expected_library_sha,
                approved_replayer["replayer"]["realpath"],
                approved_replayer["replayer"]["sha256"],
                checksums["manifestSha256"],
                checksums["bundles"],
            )
        ):
            raise ConfigError("replay_state_evidence_changed")
        return state
    environment = offline_environment(config)
    environment["DTVM_APPROVED_REPLAYER"] = approved_replayer["replayer"][
        "realpath"
    ]
    environment["DTVM_APPROVED_REPLAYER_SHA256"] = approved_replayer["replayer"][
        "sha256"
    ]
    command = [
        str(verify_script),
        str(output / "manifest.json"),
        args.label,
        str(dtvm_library),
        str(replay_output),
    ]
    process = subprocess.run(
        command,
        env=environment,
        capture_output=True,
        text=True,
        timeout=args.replay_timeout,
        check=False,
    )
    if process.returncode != 0:
        update_state(
            state_path,
            state,
            status="in_progress",
            lastFailureCategory="strict_replay_failed",
        )
        raise RpcFailure("strict_replay_failed")
    result = replay_output / "result.json"
    require_regular_no_symlink(result, "strict_replay_result_missing")
    report = safe_json_load(result)
    if not validate_replay_report(
        report,
        state["requestedCount"],
        expected_library_sha,
        approved_replayer["replayer"]["realpath"],
        approved_replayer["replayer"]["sha256"],
        checksums["manifestSha256"],
        checksums["bundles"],
    ):
        raise RpcFailure("strict_replay_contract_failed")
    update_state(
        state_path,
        state,
        phase="replayed",
        strictReplayCompleted=True,
        replayOutput=str(replay_output),
        replayResultSha256=sha256_file(result),
        verifyCorpusScript=str(verify_script),
        verifyCorpusSha256=expected_script_sha,
        dtvmLibrary=str(dtvm_library),
        dtvmLibrarySha256=expected_library_sha,
        networkExcludedFromReplay=True,
        lastFailureCategory=None,
    )
    return state


def run_seal(args: argparse.Namespace, config: Config) -> dict[str, Any]:
    state_dir = lexical_absolute(Path(args.state_dir))
    with workflow_lock(state_dir):
        return run_seal_locked(config, state_dir)


def seal_inputs(
    manifest: Path,
    checksums: Path,
    preseal_state: Path,
    verify_script: Path,
    dtvm_library: Path,
    approved_replayer: dict[str, Any],
    metrics_path: Path,
    replay_result: Path,
    state: dict[str, Any],
) -> list[dict[str, str]]:
    return [
        {
            "role": "capture_manifest",
            "path": str(manifest),
            "sha256": sha256_file(manifest),
        },
        {
            "role": "bundle_checksums",
            "path": str(checksums),
            "sha256": sha256_file(checksums),
        },
        {
            "role": "resume_state_before_seal",
            "path": str(preseal_state),
            "sha256": sha256_file(preseal_state),
        },
        {
            "role": "strict_replay_runner",
            "path": str(verify_script),
            "sha256": state["verifyCorpusSha256"],
        },
        {
            "role": "dtvm_library",
            "path": str(dtvm_library),
            "sha256": state["dtvmLibrarySha256"],
        },
        {
            "role": "approved_replayer",
            "path": approved_replayer["replayer"]["realpath"],
            "sha256": approved_replayer["replayer"]["sha256"],
        },
        {
            "role": "rpc_metrics",
            "path": str(metrics_path),
            "sha256": sha256_file(metrics_path),
        },
        {
            "role": "strict_replay_result",
            "path": str(replay_result),
            "sha256": sha256_file(replay_result),
        },
    ]


def validate_preseal_continuity(
    preseal: Any,
    sealed_state: dict[str, Any],
) -> bool:
    if not isinstance(preseal, dict):
        return False
    ignored = {"updatedAtUtc", "evidenceSeal", "evidenceSealSha256"}
    expected = {
        key: value
        for key, value in sealed_state.items()
        if key not in ignored
    }
    expected["phase"] = "replayed"
    expected["evidenceSealed"] = False
    actual = {
        key: value
        for key, value in preseal.items()
        if key not in ignored
    }
    return actual == expected


def run_seal_locked(config: Config, state_dir: Path) -> dict[str, Any]:
    state_path = state_dir / "resume-state.json"
    state = safe_json_load(state_path)
    if not isinstance(state, dict) or state.get("schema") != SCHEMA_STATE:
        raise ConfigError("resume_state_missing")
    if state.get("configFingerprint") != config.public_fingerprint:
        raise ConfigError("seal_config_fingerprint_mismatch")
    if (
        state.get("networkCaptureCompleted") is not True
        or state.get("strictReplayCompleted") is not True
        or state.get("networkExcludedFromReplay") is not True
    ):
        raise ConfigError("strict_replay_not_completed")
    approved_replayer = state.get("approvedReplayer")
    if (
        not isinstance(approved_replayer, dict)
        or validate_replayer_approval(
            Path(approved_replayer.get("manifestRealpath", ""))
        )
        != approved_replayer
    ):
        raise ConfigError("approved_replayer_identity_changed")
    output = Path(state["output"])
    manifest = output / "manifest.json"
    checksums = output / "bundle-checksums.json"
    if not manifest.is_file() or not checksums.is_file():
        raise ConfigError("capture_evidence_missing")
    corpus = validate_and_checksum_corpus(
        output,
        state["requestedCount"],
        state["frozenPin"],
        expected_replayer=approved_replayer,
        write_checksums=False,
    )
    if (
        corpus["manifestSha256"] != state.get("manifestSha256")
        or corpus["bundleSetSha256"] != state.get("bundleSetSha256")
    ):
        raise ConfigError("capture_evidence_changed_before_seal")
    replay_result = Path(state["replayOutput"]) / "result.json"
    require_regular_no_symlink(
        replay_result,
        "strict_replay_result_changed",
    )
    if sha256_file(replay_result) != state.get("replayResultSha256"):
        raise ConfigError("strict_replay_result_changed")
    replay_report = safe_json_load(replay_result)
    if not validate_replay_report(
        replay_report,
        state["requestedCount"],
        state["dtvmLibrarySha256"],
        approved_replayer["replayer"]["realpath"],
        approved_replayer["replayer"]["sha256"],
        corpus["manifestSha256"],
        corpus["bundles"],
    ):
        raise ConfigError("strict_replay_contract_failed")
    verify_script = Path(state["verifyCorpusScript"])
    dtvm_library = Path(state["dtvmLibrary"])
    require_regular_no_symlink(
        verify_script,
        "replay_identity_changed_before_seal",
    )
    require_regular_no_symlink(
        dtvm_library,
        "replay_identity_changed_before_seal",
    )
    if (
        sha256_file(verify_script) != state.get("verifyCorpusSha256")
        or sha256_file(dtvm_library) != state.get("dtvmLibrarySha256")
    ):
        raise ConfigError("replay_identity_changed_before_seal")
    metrics_path = state_dir / "metrics.json"
    require_regular_no_symlink(metrics_path, "metrics_contract_failed")
    metrics = safe_json_load(metrics_path)
    if (
        not isinstance(metrics, dict)
        or metrics.get("schema") != SCHEMA_METRICS
        or metrics.get("secretMaterialRecorded") is not False
    ):
        raise ConfigError("metrics_contract_failed")
    preseal_state = state_dir / "resume-state-before-seal.json"
    already_sealed = (
        state.get("phase") == "sealed"
        or state.get("evidenceSealed") is True
    )
    if already_sealed:
        if (
            state.get("phase") != "sealed"
            or state.get("status") != "success"
            or state.get("evidenceSealed") is not True
        ):
            raise ConfigError("sealed_state_flags_inconsistent")
        require_regular_no_symlink(
            preseal_state,
            "preseal_state_missing_or_changed",
        )
        if not validate_preseal_continuity(
            safe_json_load(preseal_state),
            state,
        ):
            raise ConfigError("preseal_state_continuity_failed")
        inputs = seal_inputs(
            manifest,
            checksums,
            preseal_state,
            verify_script,
            dtvm_library,
            approved_replayer,
            metrics_path,
            replay_result,
            state,
        )
        seal_path = state_dir / "evidence-seal.json"
        if (
            state.get("evidenceSeal") != str(seal_path)
            or not isinstance(state.get("evidenceSealSha256"), str)
        ):
            raise ConfigError("sealed_state_evidence_mismatch")
        require_regular_no_symlink(seal_path, "sealed_evidence_missing")
        if sha256_file(seal_path) != state["evidenceSealSha256"]:
            raise ConfigError("sealed_evidence_changed")
        existing_seal = safe_json_load(seal_path)
        if not (
            isinstance(existing_seal, dict)
            and existing_seal.get("schema") == SCHEMA_SEAL
            and existing_seal.get("status") == "sealed"
            and isinstance(existing_seal.get("sealedAtUtc"), str)
            and existing_seal.get("configFingerprint")
            == config.public_fingerprint
            and existing_seal.get("frozenPin") == state.get("frozenPin")
            and existing_seal.get("networkCaptureCompleted") is True
            and existing_seal.get("strictReplayCompleted") is True
            and existing_seal.get("networkExcludedFromReplay") is True
            and existing_seal.get("inputs") == inputs
            and existing_seal.get("metrics") == metrics
            and existing_seal.get("credentialsRecorded") is False
        ):
            raise ConfigError("sealed_evidence_contract_failed")
        return existing_seal
    if state.get("phase") != "replayed" or state.get("evidenceSealed") is not False:
        raise ConfigError("replayed_state_required_before_seal")
    atomic_write_json(preseal_state, state, 0o400)
    inputs = seal_inputs(
        manifest,
        checksums,
        preseal_state,
        verify_script,
        dtvm_library,
        approved_replayer,
        metrics_path,
        replay_result,
        state,
    )
    seal = {
        "schema": SCHEMA_SEAL,
        "status": "sealed",
        "sealedAtUtc": utc_now(),
        "configFingerprint": config.public_fingerprint,
        "frozenPin": state.get("frozenPin"),
        "networkCaptureCompleted": state.get("networkCaptureCompleted") is True,
        "strictReplayCompleted": state.get("strictReplayCompleted") is True,
        "networkExcludedFromReplay": state.get("networkExcludedFromReplay") is True,
        "inputs": inputs,
        "metrics": metrics,
        "credentialsRecorded": False,
    }
    seal_path = state_dir / "evidence-seal.json"
    atomic_write_json(seal_path, seal, 0o400)
    update_state(
        state_path,
        state,
        phase="sealed",
        status="success",
        evidenceSealed=True,
        evidenceSeal=str(seal_path),
        evidenceSealSha256=sha256_file(seal_path),
    )
    return seal


def command_health(config: Config, full: bool) -> dict[str, Any]:
    metrics = Metrics()
    endpoints = resolve_endpoints(config)
    client = RpcClient(config, endpoints, metrics)
    if full:
        result = readiness(config, endpoints, client)
    else:
        endpoint_reports = []
        for endpoint in endpoints:
            try:
                chain = client._request_endpoint(endpoint, "eth_chainId", [])
                success = isinstance(chain, str) and chain.lower() == config.chain_id
                category = None if success else "chain_mismatch"
            except RpcFailure as failure:
                success = False
                category = failure.category
            endpoint_reports.append(
                {
                    "name": endpoint.name,
                    "role": endpoint.role,
                    "live": success,
                    "failureCategory": category,
                }
            )
        result = {
            "schema": "reth-dtvm.rpc-ha-health.v1",
            "status": "live"
            if any(report["live"] for report in endpoint_reports)
            else "unavailable",
            "success": any(report["live"] for report in endpoint_reports),
            "endpoints": endpoint_reports,
            "secretMaterialRecorded": False,
        }
    client.close()
    return result


def command_fetch(args: argparse.Namespace, config: Config) -> dict[str, Any]:
    metrics = Metrics()
    endpoints = resolve_endpoints(config)
    client = RpcClient(config, endpoints, metrics)
    report = readiness(config, endpoints, client)
    if not report["success"]:
        client.close()
        raise RpcFailure("endpoint_not_ready")
    output = Path(args.output).resolve()
    if output.exists():
        client.close()
        raise ConfigError("output_exists")
    if args.kind == "block":
        if not HASH_RE.fullmatch(args.identifier):
            raise ConfigError("identifier_must_be_block_hash")
        result = client.call(
            "eth_getBlockByHash", [args.identifier.lower(), True], use_cache=False
        )
        payload = {
            "schema": "reth-dtvm.rpc-fetch.v1",
            "kind": "block",
            "blockHash": args.identifier.lower(),
            "result": result,
        }
        atomic_write_json(output, payload)
    elif args.kind == "transaction":
        if not HASH_RE.fullmatch(args.identifier):
            raise ConfigError("identifier_must_be_transaction_hash")
        result = client.call(
            "eth_getTransactionByHash", [args.identifier.lower()], use_cache=False
        )
        payload = {
            "schema": "reth-dtvm.rpc-fetch.v1",
            "kind": "transaction",
            "transactionHash": args.identifier.lower(),
            "result": result,
        }
        atomic_write_json(output, payload)
    else:
        if not HASH_RE.fullmatch(args.identifier):
            raise ConfigError("identifier_must_be_block_hash")
        gateway = Gateway(client, report, metrics)
        local_url = gateway.start()
        environment = sanitized_environment(config)
        process = subprocess.run(
            [
                str(Path(args.fetch_witness_script).resolve()),
                "--policy",
                "production",
                local_url,
                args.identifier.lower(),
                str(output),
                "canonical",
            ],
            env=environment,
            capture_output=True,
            text=True,
            timeout=args.fetch_timeout,
            check=False,
        )
        gateway.stop()
        if process.returncode != 0:
            client.close()
            raise RpcFailure("witness_fetch_failed")
        payload = json.loads(process.stdout)
    client.close()
    return {
        "schema": "reth-dtvm.rpc-fetch-result.v1",
        "status": "success",
        "kind": args.kind,
        "output": str(output),
        "sha256": sha256_file(output),
        "fetch": payload,
        "credentialsRecorded": False,
    }


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Reth HA readiness, capture, replay, and evidence sealing"
    )
    parser.add_argument("--config", required=True, help="secret-free JSON config")
    subparsers = parser.add_subparsers(dest="command", required=True)
    subparsers.add_parser("health", help="cheap chain liveness")
    subparsers.add_parser("readiness", help="full exact capability matrix")

    fetch = subparsers.add_parser("fetch", help="fetch a block, transaction, or witness")
    fetch.add_argument("--kind", choices=("block", "transaction", "witness"), required=True)
    fetch.add_argument("--identifier", required=True)
    fetch.add_argument("--output", required=True)
    fetch.add_argument(
        "--fetch-witness-script",
        default=str(Path(__file__).with_name("fetch-witness.sh")),
    )
    fetch.add_argument("--fetch-timeout", type=float, default=1800)

    capture = subparsers.add_parser(
        "capture", help="resumable finalized capture through the HA gateway"
    )
    capture.add_argument("--output", required=True)
    capture.add_argument("--state-dir", required=True)
    capture.add_argument("--count", type=int, default=16)
    capture.add_argument("--capture-attempts", type=int, default=3)
    capture.add_argument("--capture-timeout", type=float, default=86400)
    capture.add_argument("--dtvm-identity-manifest", required=True)
    capture.add_argument("--replayer-manifest", required=True)
    capture.add_argument(
        "--capture-script",
        default=str(Path(__file__).with_name("capture-window.sh")),
    )
    capture.add_argument("--verify-witness")

    replay = subparsers.add_parser("replay", help="strictly replay a captured corpus offline")
    replay.add_argument("--state-dir", required=True)
    replay.add_argument("--verify-corpus-script", required=True)
    replay.add_argument("--verify-corpus-sha256", required=True)
    replay.add_argument("--dtvm-library", required=True)
    replay.add_argument("--dtvm-library-sha256", required=True)
    replay.add_argument("--replay-output", required=True)
    replay.add_argument("--label", required=True)
    replay.add_argument("--replay-timeout", type=float, default=86400)

    seal = subparsers.add_parser("seal", help="seal capture and replay evidence")
    seal.add_argument("--state-dir", required=True)
    return parser


def emit_failure(category: str, status: int = 1) -> int:
    print(
        json.dumps(
            {
                "schema": "reth-dtvm.rpc-ha-failure.v1",
                "status": "failure",
                "success": False,
                "failureCategory": category,
                "credentialsRecorded": False,
            },
            sort_keys=True,
        )
    )
    return status


def main() -> int:
    parser = build_parser()
    args = parser.parse_args()
    try:
        config = load_config(Path(args.config))
        if args.command == "health":
            result = command_health(config, False)
        elif args.command == "readiness":
            result = command_health(config, True)
        elif args.command == "fetch":
            result = command_fetch(args, config)
        elif args.command == "capture":
            if args.count < 1 or args.count > 256:
                raise ConfigError("count_out_of_range")
            if args.capture_attempts < 1 or args.capture_attempts > 10:
                raise ConfigError("capture_attempts_out_of_range")
            result = run_capture_workflow(args, config)
        elif args.command == "replay":
            result = run_replay(args, config)
        elif args.command == "seal":
            result = run_seal(args, config)
        else:
            parser.error("unknown command")
            return 2
        print(json.dumps(result, sort_keys=True))
        return 0 if result.get("success", True) else 1
    except ConfigError as error:
        return emit_failure(str(error), 2)
    except RpcFailure as error:
        return emit_failure(error.category, 1)
    except KeyboardInterrupt:
        return emit_failure("interrupted", 130)
    except Exception:
        return emit_failure("internal_error", 1)


if __name__ == "__main__":
    raise SystemExit(main())
