# Plan: Transparent Service Mesh Proxy

## Context

The platform has a process-wrapper proxy (`platform-proxy --wrap`) that wraps deployed app containers, capturing logs and exporting OTLP telemetry. It also has mTLS inbound/outbound code (`inbound.rs`, `outbound.rs`, `tcp_proxy.rs`) and cert bootstrap (`tls.rs`), but none of the mesh networking is wired up — the proxy is currently a log forwarder only.

We need a fully transparent service mesh where:
- **All inbound traffic** to any pod port is intercepted, mTLS-terminated, and forwarded to the app
- **All outbound traffic** from any app is intercepted, wrapped in mTLS, and sent to the remote pod's proxy
- **No application changes** — apps keep their original ports, connection strings, and behavior
- **No Service changes** — `targetPort` stays the same
- **No health probe changes** — kubelet probes work unchanged

### Design: iptables REDIRECT + SO_ORIGINAL_DST + Source IP Exclusion

**Inbound**: An init container with `NET_ADMIN` sets iptables `PREROUTING REDIRECT` rules that send all incoming TCP to the proxy's inbound listener (port 15006). The proxy calls `getsockopt(SO_ORIGINAL_DST)` to learn the original destination port, then forwards to `localhost:{original_port}`.

**Outbound**: iptables `OUTPUT REDIRECT` rules send all app-originated TCP to the proxy's outbound listener (port 15001). The proxy calls `SO_ORIGINAL_DST` to learn the real destination, establishes mTLS to the remote, and tunnels the data.

**Loop prevention**: The proxy binds its own upstream sockets to source address `127.0.0.6` (valid loopback, no capabilities needed). The iptables rule `-s 127.0.0.6 -j RETURN` skips redirection for proxy-originated traffic. This avoids the `SO_MARK`/`NET_ADMIN` requirement on the proxy container.

**Permissive mTLS**: The inbound listener peeks the first byte of each connection — `0x16` (TLS ClientHello) triggers mTLS termination, anything else is plaintext passthrough. This lets kubelet health probes and gradual migration work without probe rewriting.

**Protocol detection**: The proxy sniffs the first bytes of each plaintext connection — HTTP verbs (`GET `, `POST`, `PUT `, `DELETE`, `PATCH`, `HEAD`, `OPTIONS`, `HTTP/`) trigger HTTP-layer processing (traceparent injection, SERVER/CLIENT spans). Everything else gets raw TCP bidirectional copy + CONNECTION spans. This is more robust than port-based guessing and works for apps on any port.

**External egress passthrough**: Outbound traffic destined for IPs outside the cluster's Pod/Service CIDRs is passed through as raw TCP (no mTLS origination). The proxy checks the resolved destination IP against configured internal CIDRs — anything outside is external and gets a direct connection (still via `127.0.0.6` source bind to skip iptables, but no TLS wrapping). This prevents breaking connections to external APIs (Stripe, RDS, etc.).

**Graceful shutdown**: On SIGTERM, the proxy enters drain mode — stops accepting new connections on inbound/outbound listeners but keeps existing connections alive until the child process exits or a grace period expires (default 30s). Since the proxy is PID 1 and wraps the child, it always outlives the app. iptables rules remain active for the pod's lifetime, so no traffic is lost to dead ports.

**SO_ORIGINAL_DST resilience**: The `get_original_dst()` call includes a short retry (3 attempts, 2ms backoff) for the rare case where the conntrack entry isn't fully established at call time under high connection churn.

### Capability Model

| Component | Capabilities | Runs as |
|---|---|---|
| `proxy-iptables` init container | `NET_ADMIN`, `NET_RAW` | Exits before app starts |
| `platform-proxy` (app container) | **None** | Same UID as app |

`SO_ORIGINAL_DST` requires no capabilities — it reads conntrack state for sockets the process owns. `bind("127.0.0.6:0")` requires no capabilities — any process can bind loopback addresses. PSA `baseline` allows `NET_ADMIN` on init containers.

### Port Allocation

| Port | Purpose | Bind address |
|---|---|---|
| 15006 | Inbound listener (receives REDIRECT'd inbound) | `0.0.0.0` |
| 15001 | Outbound listener (receives REDIRECT'd outbound) | `0.0.0.0` |
| 15020 | Health/readiness server | `0.0.0.0` |
| App ports | Unchanged — app binds normally | App decides |

## Design Principles

- **Zero app changes** — iptables intercepts at the kernel level; apps are unaware
- **Zero Service changes** — Services target the original app port; iptables redirects to proxy before the app sees the packet
- **Permissive by default** — accept both mTLS and plaintext; strict mode is opt-in per namespace later
- **Safe loop prevention** — source IP `127.0.0.6` exclusion requires no special capabilities on the running proxy
- **No unsafe code** — `nix` crate provides safe Rust wrappers for `SO_ORIGINAL_DST` (via `OriginalDst` sockopt)
- **Backward compatible** — explicit port mode (`PROXY_TLS_PORT`, `PROXY_OUTBOUND_PORT`, `--tcp-ports`) keeps working when `PROXY_TRANSPARENT=false`
- **External-aware** — outbound traffic to non-cluster IPs bypasses mTLS (raw TCP passthrough); only internal Pod/Service CIDR traffic gets mTLS
- **Graceful shutdown** — proxy drains connections on SIGTERM; since proxy is PID 1 and wraps the child, it always outlives the app so iptables rules never point at a dead port

---

## PR 1: Transparent Inbound Proxy

Add transparent inbound listener that intercepts all incoming traffic via iptables REDIRECT, determines the original destination via `SO_ORIGINAL_DST`, and forwards to the local app — with permissive mTLS (auto-detect TLS vs plaintext).

- [ ] Types & errors defined
- [ ] Tests written (red phase)
- [ ] Implementation complete (green phase)
- [ ] Integration/E2E tests passing
- [ ] Quality gate passed

### Code Changes

| File | Change |
|---|---|
| `crates/proxy/Cargo.toml` | Add `"net"` to nix features: `features = ["signal", "process", "net"]` |
| `crates/proxy/src/proxy/mod.rs` | Add `pub mod transparent;` |
| `crates/proxy/src/proxy/transparent.rs` | **New file** — `get_original_dst()` (IPv4 + IPv6 fallback), `is_tls_client_hello()`, `peek_protocol()` (with fragmentation-safe retry), `is_internal_ip()` |
| `crates/proxy/src/proxy/config.rs` | Add `transparent: bool` (from `PROXY_TRANSPARENT`), `mtls_mode: MtlsMode` (from `PROXY_MTLS_MODE`, enum `Permissive`/`Strict`, default `Permissive`), `inbound_port: u16` (from `PROXY_INBOUND_PORT`, default 15006), `outbound_bind_addr: IpAddr` (from `PROXY_OUTBOUND_BIND`, default `127.0.0.6`), `node_cidrs: Vec<(IpAddr, u8)>` (from `PROXY_NODE_CIDRS`, for strict mode kubelet whitelist) to `ProxyConfig` |
| `crates/proxy/src/proxy/inbound.rs` | Add `run_transparent_inbound(port, mtls_mode, certs, span_tx, active_spans, red_metrics, shutdown)` — binds `0.0.0.0:{inbound_port}`, accepts connections, calls `get_original_dst()` to learn original dest (ip+port), peeks for TLS. In permissive mode: plaintext passes through. In strict mode: plaintext rejected (health probes never reach here — excluded at iptables level). Forwards to `original_ip:original_port` (NOT localhost — handles apps that bind to pod IP). Uses `bind("127.0.0.6:0")` on the forwarding socket so iptables OUTPUT rules skip it. Protocol sniffed via `peek_protocol()` for HTTP vs TCP handling. |
| `crates/proxy/src/main.rs` | In `start_mesh_components`: when `config.transparent && certs.is_some()`, spawn `run_transparent_inbound()` instead of the old explicit `run_inbound_listener()` |
| `crates/proxy/src/proxy/health.rs` | Change readiness check: when transparent mode, check `app_port` if set OR just return ready (the app port is dynamic per-connection, can't pre-check). Alternatively, keep `app_port` as the "primary" app port for readiness. |

### transparent.rs — New Module

```rust
//! Transparent proxy helpers: SO_ORIGINAL_DST, TLS/protocol detection, CIDR matching.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::os::fd::AsFd;
use std::time::Duration;

/// Recover the original destination address from a REDIRECT'd socket.
/// Tries IPv4 (OriginalDst) first, falls back to IPv6 (Ip6tOriginalDst)
/// for dual-stack clusters. Retries up to 3 times with 2ms backoff
/// for conntrack race conditions under high connection churn.
pub async fn get_original_dst(stream: &tokio::net::TcpStream) -> std::io::Result<SocketAddr> {
    use nix::sys::socket::{self, sockopt};
    let fd = stream.as_fd();

    for attempt in 0..3 {
        // Try IPv4 first
        match socket::getsockopt(&fd, sockopt::OriginalDst) {
            Ok(addr) => {
                let ip = Ipv4Addr::from(u32::from_be(addr.sin_addr.s_addr));
                let port = u16::from_be(addr.sin_port);
                return Ok(SocketAddr::new(ip.into(), port));
            }
            Err(nix::errno::Errno::ENOPROTOOPT) => {
                // Not an IPv4 socket or no IPv4 conntrack — try IPv6
                break;
            }
            Err(e) if attempt < 2 => {
                tokio::time::sleep(Duration::from_millis(2)).await;
                tracing::trace!(attempt, error = %e, "SO_ORIGINAL_DST retry");
            }
            Err(e) => return Err(std::io::Error::new(std::io::ErrorKind::Other, e)),
        }
    }

    // IPv6 fallback (dual-stack / IPv6-only clusters)
    match socket::getsockopt(&fd, sockopt::Ip6tOriginalDst) {
        Ok(addr6) => {
            let ip = std::net::Ipv6Addr::from(addr6.sin6_addr.s6_addr);
            let port = u16::from_be(addr6.sin6_port);
            Ok(SocketAddr::new(ip.into(), port))
        }
        Err(e) => Err(std::io::Error::new(std::io::ErrorKind::Other, e)),
    }
}

/// Peek at the first byte to determine if this is a TLS ClientHello.
/// TLS records start with content type 0x16 (Handshake).
pub async fn is_tls_client_hello(stream: &tokio::net::TcpStream) -> bool {
    let mut buf = [0u8; 1];
    match stream.peek(&mut buf).await {
        Ok(1) => buf[0] == 0x16,
        _ => false,
    }
}

/// Protocol detected from the first bytes of a connection.
pub enum DetectedProtocol {
    Http,   // HTTP/1.1 verb, HTTP/2 preface, or gRPC
    Tcp,    // Anything else (Postgres, Redis, binary protocols)
}

/// Sniff the first bytes of a plaintext connection to detect HTTP.
/// Waits up to 100ms for at least 4 bytes to arrive, handling TCP
/// fragmentation where the initial peek may return fewer bytes.
pub async fn peek_protocol(stream: &tokio::net::TcpStream) -> DetectedProtocol {
    let mut buf = [0u8; 8];
    let deadline = tokio::time::Instant::now() + Duration::from_millis(100);

    loop {
        match stream.peek(&mut buf).await {
            Ok(n) if n >= 4 => {
                let prefix = &buf[..n];
                let is_http =
                    // HTTP/1.1 methods
                    prefix.starts_with(b"GET ")
                    || prefix.starts_with(b"POST")
                    || prefix.starts_with(b"PUT ")
                    || prefix.starts_with(b"DELE")
                    || prefix.starts_with(b"PATC")
                    || prefix.starts_with(b"HEAD")
                    || prefix.starts_with(b"OPTI")
                    || prefix.starts_with(b"HTTP")
                    || prefix.starts_with(b"CONN")
                    // HTTP/2 connection preface (gRPC, h2c prior knowledge)
                    || prefix.starts_with(b"PRI ");
                return if is_http { DetectedProtocol::Http } else { DetectedProtocol::Tcp };
            }
            Ok(_) if tokio::time::Instant::now() < deadline => {
                // Fragmented — not enough bytes yet, wait briefly and retry
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            _ => {
                // Timeout or error — default to TCP (safe fallback)
                return DetectedProtocol::Tcp;
            }
        }
    }
}

/// Check if an IP is within the cluster's internal CIDRs.
/// External IPs get raw TCP passthrough (no mTLS origination).
pub fn is_internal_ip(ip: IpAddr, internal_cidrs: &[(IpAddr, u8)]) -> bool {
    internal_cidrs.iter().any(|&(network, prefix_len)| cidr_contains(network, prefix_len, ip))
}
```

### Inbound Listener Flow

**Key: forward to `original_ip:original_port`** (from `SO_ORIGINAL_DST`), NOT `localhost`. This handles apps that bind to the pod's eth0 IP (`10.42.x.x`) instead of `0.0.0.0`. The forwarding socket binds source to `127.0.0.6` so iptables OUTPUT rules skip it (no re-interception).

**No app port exclusions in iptables** — ALL inbound traffic (including health probes) goes through the proxy on port 15006. This avoids the shared-port trap where excluding an app's health port would bypass the mesh entirely for that port's traffic.

```
accept(15006)
  → get_original_dst() → original_ip:original_port (IPv4 + IPv6, with retry)
  → is_tls_client_hello()?
    YES → build_tls_acceptor(), tls_handshake()
        → extract_spiffe_id() from client cert
        → peek_protocol() on decrypted stream (fragmentation-safe, 100ms deadline)
            Http → parse HTTP, inject traceparent, forward to original_ip:original_port, record SERVER span
            Tcp  → bidirectional copy to original_ip:original_port, record CONNECTION span
    NO (plaintext) →
        → mtls_mode?
            Permissive → peek_protocol() on plaintext stream
                Http → parse HTTP, forward to original_ip:original_port (no mTLS span)
                Tcp  → bidirectional copy to original_ip:original_port
            Strict → is_node_ip(peer_addr, node_cidrs)?
                YES → plaintext passthrough (kubelet health probes)
                NO  → close connection, log warning "plaintext rejected in strict mode"
```

**Strict mode kubelet whitelist**: In strict mode, the proxy checks the source IP of plaintext connections. Node IPs (kubelet probes) are whitelisted — they get plaintext passthrough. All other plaintext is rejected. Node CIDRs are configured via `PROXY_NODE_CIDRS` (set by the injection code from the cluster's node subnet, or defaults to common node ranges). This means health probes work on any port, even the app's primary traffic port, without creating a mesh bypass.

### Test Outline — PR 1

**New behaviors to test:**
- `get_original_dst()` returns correct address (unit, mock via nix types)
- `is_tls_client_hello()` detects TLS vs plaintext (unit)
- `is_http_protocol()` sniffs HTTP verbs correctly (unit)
- `is_http_protocol()` rejects binary/non-HTTP data (unit)
- Config parsing: `PROXY_TRANSPARENT=true` enables transparent mode (unit)
- Transparent inbound: plain HTTP forwarded correctly (integration — requires iptables in test pod)
- Transparent inbound: mTLS terminated, forwarded to app (integration)
- Transparent inbound: TCP (non-HTTP) bidirectional copy works (integration)
- Permissive mode: TLS and plaintext both accepted on same port (integration)

**Existing tests affected:**
- `tests/proxy_pipeline_integration.rs` — add transparent mode injection tests
- Proxy unit tests — ensure old explicit-port mode still works when `transparent=false`

**Estimated test count:** ~8 unit + 4 integration

### Verification
- Start proxy with `PROXY_TRANSPARENT=true PROXY_INBOUND_PORT=15006` and iptables rules
- Send plain HTTP to an app port → verify it arrives at the app unchanged
- Send mTLS to an app port → verify TLS termination and forwarding
- Send TCP to a non-HTTP port → verify bidirectional copy

---

## PR 2: Transparent Outbound Proxy

Add transparent outbound listener that intercepts all app-originated TCP, determines the real destination via `SO_ORIGINAL_DST`, and either wraps in mTLS (internal cluster traffic) or passes through as raw TCP (external egress). Uses `127.0.0.6` source bind to prevent redirect loops.

- [ ] Types & errors defined
- [ ] Tests written (red phase)
- [ ] Implementation complete (green phase)
- [ ] Integration/E2E tests passing
- [ ] Quality gate passed

### Code Changes

| File | Change |
|---|---|
| `crates/proxy/src/proxy/transparent.rs` | Add `bind_outbound_socket(dest: SocketAddr, bind_addr: IpAddr) -> io::Result<TcpStream>` — creates `TcpSocket`, binds to `{bind_addr}:0`, connects to dest. Add `is_internal_ip()` CIDR check. |
| `crates/proxy/src/proxy/outbound.rs` | Add `run_transparent_outbound(outbound_bind_addr, internal_cidrs, certs, span_tx, shutdown)` — binds `0.0.0.0:15001`, accepts connections, calls `get_original_dst()`. If dest IP is internal: mTLS connect via `bind_outbound_socket(127.0.0.6)`. If external: raw TCP passthrough via `bind_outbound_socket(127.0.0.6)` (no TLS). Protocol sniffing for HTTP spans in both cases. |
| `crates/proxy/src/proxy/config.rs` | Add `outbound_port: u16` (from `PROXY_OUTBOUND_PORT`, default 15001), `internal_cidrs: Vec<(IpAddr, u8)>` (from `PROXY_INTERNAL_CIDRS`, default `10.0.0.0/8,172.16.0.0/12,192.168.0.0/16` — standard RFC1918 private ranges covering all cluster CIDRs) |
| `crates/proxy/src/main.rs` | In `start_mesh_components`: when `config.transparent && certs.is_some()`, spawn `run_transparent_outbound()` |

### Outbound Listener Flow

```
accept(15001)
  → get_original_dst() → remote_ip:remote_port (with retry)
  → is_internal_ip(remote_ip, internal_cidrs)?
    YES (cluster traffic) →
      → bind_outbound_socket(remote_ip:remote_port, 127.0.0.6)
      → build_tls_connector(), mTLS connect
      → sniff app stream: is_http_protocol()?
          YES → read HTTP, inject traceparent, forward, record CLIENT span
          NO  → bidirectional TCP copy (app plaintext ↔ upstream mTLS), record CONNECTION span
    NO (external egress) →
      → bind_outbound_socket(remote_ip:remote_port, 127.0.0.6)
      → raw TCP connect (NO mTLS — external server won't have our CA)
      → bidirectional TCP copy (app plaintext ↔ upstream plaintext)
      → record CONNECTION span with mesh.external=true attribute
```

### Key Detail: Connecting to Remote Proxy

When the app connects to `platform-demo-db:5432`, iptables redirects to the outbound listener. `SO_ORIGINAL_DST` recovers the original destination `10.42.x.x:5432`. The outbound proxy connects to **the same IP:port** (`10.42.x.x:5432`) but wraps it in mTLS.

On the remote side, iptables PREROUTING on the postgres pod redirects port 5432 to its inbound proxy (15006). The remote proxy terminates mTLS and forwards to `localhost:5432` where postgres is listening.

The connection path:
```
app → localhost:15001 (iptables) → outbound proxy
  → mTLS connect to 10.42.x.x:5432 (source: 127.0.0.6, iptables skip)
  → remote pod iptables: 5432 → 15006
  → remote inbound proxy: mTLS terminate → localhost:5432
  → postgres
```

### Test Outline — PR 2

**New behaviors to test:**
- `bind_outbound_socket()` binds to specified source addr (unit)
- `is_internal_ip()` correctly classifies RFC1918, cluster CIDRs, and public IPs (unit)
- Internal CIDR parsing from `PROXY_INTERNAL_CIDRS` env var (unit)
- Outbound proxy: internal HTTP request forwarded with mTLS (integration)
- Outbound proxy: internal TCP connection forwarded with mTLS (integration)
- Outbound proxy: external IP gets raw TCP passthrough, no mTLS (integration)
- Loop prevention: outbound connections from 127.0.0.6 not re-intercepted (integration)
- Protocol sniffing works on outbound (HTTP vs TCP detection) (unit)

**Existing tests affected:**
- Existing outbound tests in proxy crate — ensure explicit mode still works

**Estimated test count:** ~6 unit + 4 integration

### Verification
- App inside pod connects to `remote-db:5432` → verify mTLS connection to remote pod
- Check proxy logs for CLIENT/CONNECTION spans with correct destination
- Verify no redirect loop (proxy's upstream connections have source 127.0.0.6)

---

## PR 3: iptables Init Container + Injection Code

Wire the transparent proxy into the platform's deployment pipeline. Add iptables setup as a second init container and configure the proxy env vars for transparent mode.

- [ ] Types & errors defined
- [ ] Tests written (red phase)
- [ ] Implementation complete (green phase)
- [ ] Integration/E2E tests passing
- [ ] Quality gate passed

### Code Changes

| File | Change |
|---|---|
| `src/config.rs` | Add `mesh_transparent: bool` (from `PLATFORM_MESH_TRANSPARENT`, default `true` when `mesh_enabled`), `mesh_strict_mtls: bool` (from `PLATFORM_MESH_STRICT`, default `false`). |
| `src/deployer/applier.rs` | Add `build_proxy_iptables_init_container(config) -> serde_json::Value` that builds the iptables init container spec. Modify `inject_proxy_to_pod_spec()` to also inject this container when transparent mode enabled. Modify `inject_proxy_to_container()` to add transparent + strict mode env vars. No iptables exclusions for app ports — all traffic goes through the proxy. In strict mode, the proxy itself whitelists kubelet/node source IPs for plaintext passthrough. |
| `tests/helpers/mod.rs` | Add `mesh_transparent`, `mesh_strict_mtls` to test config |
| `tests/e2e_helpers/mod.rs` | Add `mesh_transparent`, `mesh_strict_mtls` to e2e config |

### iptables Init Container

The init container uses a **distroless image** built from the platform's proxy build pipeline. It contains only:
- A statically-linked `iptables-nft` binary (from Alpine's `iptables-static` package, ~2MB)
- A shell-free entrypoint binary (`platform-proxy-init`) that sets up the rules programmatically

Alternatively, for the initial implementation: compile the iptables setup as a small Rust binary in `crates/proxy/` that uses `std::process::Command` to invoke `iptables`. The binary + `iptables-nft` static binary are baked into a `FROM scratch` Docker image.

For pragmatic initial delivery, use the `platform-runner-bare` image (already seeded, has shell) with the iptables script. Migrate to distroless once the mesh is proven.

```rust
fn build_proxy_iptables_init_container(
    config: &ProxyInjectionConfig,
) -> serde_json::Value {
    // No app port exclusions — ALL inbound traffic goes through the proxy.
    // In permissive mode: plaintext passes through (kubelet probes work).
    // In strict mode: the proxy whitelists kubelet/node source IPs for
    // plaintext passthrough; all other plaintext is rejected.
    // This avoids the shared-port trap where excluding a health probe port
    // would bypass the mesh for the app's primary traffic port.

    let script = r#"set -eu
PROXY_INBOUND_PORT=${PROXY_INBOUND_PORT:-15006}
PROXY_OUTBOUND_PORT=${PROXY_OUTBOUND_PORT:-15001}
PROXY_HEALTH_PORT=${PROXY_HEALTH_PORT:-15020}
PROXY_OUTBOUND_BIND=${PROXY_OUTBOUND_BIND:-127.0.0.6}

# --- INBOUND: redirect all external TCP to proxy inbound listener ---
iptables -t nat -N PLATFORM_INBOUND
# Only exclude proxy's own ports (to avoid redirect loops)
iptables -t nat -A PLATFORM_INBOUND -p tcp --dport ${PROXY_INBOUND_PORT} -j RETURN
iptables -t nat -A PLATFORM_INBOUND -p tcp --dport ${PROXY_OUTBOUND_PORT} -j RETURN
iptables -t nat -A PLATFORM_INBOUND -p tcp --dport ${PROXY_HEALTH_PORT} -j RETURN
# ALL other inbound traffic → proxy (including health probe ports)
iptables -t nat -A PLATFORM_INBOUND -p tcp -j REDIRECT --to-ports ${PROXY_INBOUND_PORT}
iptables -t nat -A PREROUTING -p tcp -j PLATFORM_INBOUND

# --- OUTBOUND: redirect all app-originated TCP to proxy outbound listener ---
iptables -t nat -N PLATFORM_OUTPUT
iptables -t nat -A PLATFORM_OUTPUT -s ${PROXY_OUTBOUND_BIND}/32 -j RETURN
iptables -t nat -A PLATFORM_OUTPUT -o lo -d 127.0.0.1/32 -j RETURN
iptables -t nat -A PLATFORM_OUTPUT -p tcp --dport 53 -j RETURN
iptables -t nat -A PLATFORM_OUTPUT -p tcp -j REDIRECT --to-ports ${PROXY_OUTBOUND_PORT}
iptables -t nat -A OUTPUT -p tcp -j PLATFORM_OUTPUT

echo "[proxy-iptables] rules installed (inbound:${PROXY_INBOUND_PORT} outbound:${PROXY_OUTBOUND_PORT})"
"#;

    serde_json::json!({
        "name": "proxy-iptables",
        "image": config.init_image,
        "command": ["sh", "-c"],
        "args": [script],
        "securityContext": {
            "capabilities": {
                "add": ["NET_ADMIN", "NET_RAW"],
                "drop": ["ALL"]
            },
            "allowPrivilegeEscalation": false,
            "readOnlyRootFilesystem": true
        },
        "resources": {
            "requests": { "cpu": "10m", "memory": "16Mi" },
            "limits": { "cpu": "100m", "memory": "32Mi" }
        }
    })
}
```

The `health_probe_ports` are extracted from the container spec's `readinessProbe`, `livenessProbe`, and `startupProbe` port fields. This is the key mechanism that makes strict mTLS safe: kubelet probes bypass iptables entirely and hit the app's health endpoints directly, never touching the proxy's mTLS listener.

### Init Container Ordering

```yaml
initContainers:
  - name: proxy-init        # 1. Download proxy binary to /proxy
  - name: proxy-iptables    # 2. Set iptables rules (runs after proxy-init)
  - name: gen-certs          # 3. User init containers (if any, e.g. postgres certs)
containers:
  - name: app                # Wrapped: /proxy/platform-proxy --wrap -- <original cmd>
```

The `proxy-iptables` container uses a distroless image containing only the statically-linked `iptables-nft` binary and the setup script — no shell, no package manager. The binary is compiled from the platform's proxy build and included in the `platform-proxy-init` image alongside the proxy binary itself. The init container requires no volume mounts — it only modifies the pod's network namespace.

### Container Env Vars (added by inject_proxy_to_container)

When transparent mode:
```
PROXY_TRANSPARENT=true
PROXY_MTLS_MODE=permissive          # or "strict" when mesh_strict_mtls=true
PROXY_INBOUND_PORT=15006
PROXY_OUTBOUND_BIND=127.0.0.6
PROXY_INTERNAL_CIDRS=10.0.0.0/8,172.16.0.0/12,192.168.0.0/16
PROXY_HEALTH_PORT=15020
PLATFORM_API_URL=<from config>
PLATFORM_SERVICE_NAME=<workload/container>
```

### Strict mTLS Mode

When `PROXY_MTLS_MODE=strict`, the inbound listener rejects plaintext connections — **except** from kubelet/node IPs. This is handled entirely at the proxy level, not iptables:

1. **All inbound traffic** (including health probes) goes through iptables REDIRECT → proxy port 15006
2. **mTLS connections**: accepted and processed normally (mesh peers have platform CA certs)
3. **Plaintext from node IPs**: allowed through (kubelet health probes — source IP checked against `PROXY_NODE_CIDRS`)
4. **Plaintext from other IPs**: rejected with connection close + warning log

This avoids the shared-port trap: app health probes on port 8080 still work even when 8080 is the primary traffic port, because the proxy distinguishes kubelet traffic by source IP, not by destination port.

The injection code sets `PROXY_NODE_CIDRS` from the cluster's node subnet (e.g., `192.168.0.0/16` for Kind, `10.0.0.0/8` for cloud). The proxy also accepts the node's host IP from the Kubernetes downward API via `status.hostIP` if needed.

### Network Policy Updates

The current network policy in `namespace.rs` allows port 8443 for mesh traffic. With transparent mode, mesh traffic arrives on the **original app ports** (not 8443) — it's just wrapped in mTLS. The network policy needs to allow traffic on all ports between platform-managed namespaces, or we keep 8443 as an optional explicit port and let the iptables approach handle the rest.

| File | Change |
|---|---|
| `src/deployer/namespace.rs` | In `build_network_policy()`: when mesh is enabled, allow all TCP between `platform.io/managed-by: platform` namespaces (not just port 8443). The mTLS ensures authentication regardless of port. |

### validate_pod_spec() — No Changes

The iptables init container with `NET_ADMIN` is injected by the platform AFTER `validate_pod_spec()` checks the user's manifest. User manifests still can't add capabilities.

### Test Outline — PR 3

**New behaviors to test:**
- iptables init container is injected when `mesh_transparent=true` (unit)
- iptables init container has correct capabilities (unit)
- iptables init container is NOT injected when `mesh_transparent=false` (unit)
- Transparent env vars are added to wrapped containers (unit)
- Init container ordering: proxy-init before proxy-iptables (unit)
- `validate_pod_spec()` still blocks user-specified privileged containers (unit — existing test)
- Full injection roundtrip: Deployment with proxy + iptables + env vars (integration)

**Existing tests affected:**
- `tests/proxy_pipeline_integration.rs` — add transparent injection test case
- `tests/helpers/mod.rs` — add `mesh_transparent` config field
- `tests/e2e_helpers/mod.rs` — add `mesh_transparent` config field

**Estimated test count:** ~6 unit + 2 integration

### Verification
- Deploy demo project with mesh enabled → verify pod has 3 init containers (proxy-init, proxy-iptables, gen-certs)
- Check iptables rules inside running pod: `kubectl exec -- iptables -t nat -L`
- Verify app still responds on its original port
- Verify health probes pass (port 15020 excluded from redirect)

---

## PR 4: End-to-End mTLS Mesh Verification

Integration tests proving the full mesh works: inbound mTLS termination, outbound mTLS origination, cross-pod mTLS communication, and health probe passthrough.

- [ ] Types & errors defined
- [ ] Tests written (red phase)
- [ ] Implementation complete (green phase)
- [ ] Integration/E2E tests passing
- [ ] Quality gate passed

### Code Changes

| File | Change |
|---|---|
| `tests/mesh_integration.rs` | **New file** — integration tests for transparent mesh |
| `tests/helpers/mod.rs` | Add mesh test helpers: `deploy_meshed_pod()`, `verify_mtls_connection()` |

### Test Scenarios

1. **Inbound plain HTTP passthrough** — deploy a pod with mesh, send plain HTTP to its service port, verify app receives the request and responds (permissive mode).

2. **Inbound mTLS termination** — deploy a pod with mesh, connect with mTLS (using platform CA certs), verify the proxy terminates TLS and forwards to app.

3. **Outbound mTLS origination** — deploy two pods (app + db) with mesh, have app connect to db via K8s service DNS, verify the connection is wrapped in mTLS.

4. **Health probe passthrough** — deploy a pod with mesh + readiness probe on app port, verify kubelet probes succeed (port 15020 excluded from redirect, or app port in permissive mode).

5. **Cross-pod mTLS communication** — deploy demo project (app + db + cache), trigger a request that makes app→db and app→cache connections, verify OTLP spans show mTLS CLIENT/SERVER span pairs.

6. **Preview iframe works** — deploy demo project, access deploy preview URL, verify 200 response (not 504).

### Test Outline — PR 4

**Estimated test count:** 0 unit + 4 integration + 2 E2E

### Verification
- All demo project pods have proxy wrapper + iptables init container
- `kubectl exec -- iptables -t nat -L -n` shows correct rules
- Platform observe API shows mTLS SERVER spans from inbound traffic
- Platform observe API shows mTLS CLIENT spans from outbound traffic
- Deploy preview iframes load successfully
- No 504 timeouts

---

## Cascading Impact

### Files requiring updates across PRs

| File | PR | Reason |
|---|---|---|
| `crates/proxy/Cargo.toml` | 1 | nix `net` feature |
| `crates/proxy/src/proxy/mod.rs` | 1 | new `transparent` module |
| `crates/proxy/src/proxy/config.rs` | 1, 2 | new config fields |
| `crates/proxy/src/proxy/inbound.rs` | 1 | transparent inbound listener |
| `crates/proxy/src/proxy/outbound.rs` | 2 | transparent outbound listener |
| `crates/proxy/src/main.rs` | 1, 2 | wire transparent components |
| `src/config.rs` | 3 | `mesh_transparent` field |
| `src/deployer/applier.rs` | 3 | iptables init container, env vars |
| `src/deployer/namespace.rs` | 3 | network policy update |
| `tests/helpers/mod.rs` | 3 | `mesh_transparent` config |
| `tests/e2e_helpers/mod.rs` | 3 | `mesh_transparent` config |

### Backward Compatibility

- `PROXY_TRANSPARENT=false` (default when `mesh_transparent` not set) → old explicit-port mode works unchanged
- Existing `PROXY_TLS_PORT`, `PROXY_OUTBOUND_PORT`, `--tcp-ports` continue to work in explicit mode
- Gateway mode (`--gateway`) is completely unaffected
- Non-mesh deployments (`mesh_enabled=false`) are completely unaffected

### Security Considerations

- `NET_ADMIN` + `NET_RAW` only on init container (exits before app starts)
- Proxy container has NO special capabilities
- `validate_pod_spec()` still blocks user-specified privileged containers/capabilities
- Platform injects capabilities post-validation (users can't escalate)
- Permissive mTLS means unauthenticated traffic still flows during migration — strict mode enforcement is a future phase
- `127.0.0.6` source bind is a loopback address, not routable outside the pod

### Known Limitations

- **IPv6**: `get_original_dst()` tries IPv4 (`OriginalDst`) first, falls back to IPv6 (`Ip6tOriginalDst`) for dual-stack/IPv6-only clusters (EKS, GKE moving to IPv6).
- **UDP**: iptables rules only capture TCP. UDP (DNS, QUIC) is not intercepted. DNS is explicitly excluded from outbound redirection.
- **`unsafe_code = "forbid"`**: The nix crate's `getsockopt` wrappers are safe Rust. No `unsafe` needed in our code.
- **iptables binary**: The `proxy-iptables` init container needs `iptables`. Uses `platform-runner-bare` initially; future distroless image contains only static `iptables-nft`.
- **External egress detection**: Uses RFC1918 CIDR matching (`10/8`, `172.16/12`, `192.168/16`) to distinguish internal vs external. Custom cluster CIDRs can be configured via `PROXY_INTERNAL_CIDRS`. Cloud metadata IPs (`169.254.169.254`) are external by default.
- **Conntrack table limits**: Under extreme connection churn (>10K new connections/sec per pod), conntrack entries may lag. The `SO_ORIGINAL_DST` retry (3 attempts, 2ms backoff) mitigates this.
- **Graceful shutdown ordering**: The proxy (PID 1) always outlives the child app. On SIGTERM, the proxy forwards the signal to the child, then drains existing connections for up to 30s before exiting. iptables rules remain active throughout — no traffic hits a dead port.
- **Strict mTLS safe for probes** — no iptables port exclusions; in strict mode the proxy whitelists kubelet/node source IPs for plaintext passthrough, rejecting all other plaintext. This avoids the shared-port trap where excluding a health port from iptables would bypass the mesh entirely for that port.
- **Forward to original dest IP** — the proxy forwards to `original_ip:original_port` (from `SO_ORIGINAL_DST`), not `localhost`. This handles apps that bind to the pod's eth0 IP instead of `0.0.0.0`. Forwarding sockets bind source `127.0.0.6` to skip iptables OUTPUT.
- **Distroless init container** — iptables init uses `platform-runner-bare` initially (already seeded, has shell + iptables); future hardening produces a scratch image with only the static `iptables-nft` binary
