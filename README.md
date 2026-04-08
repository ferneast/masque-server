# masque-tunnel

A high-performance MASQUE CONNECT-UDP (RFC 9298) tunnel — both client and server in a single binary.

Tunnels UDP traffic through HTTP/3 (QUIC) DATAGRAM frames on port 443, making it indistinguishable from normal HTTPS traffic. Designed for use as a VPN obfuscation layer (e.g., tunneling WireGuard).

## Background

`masque-tunnel` was extracted from [Xlarva](https://xlarva.app/), a multi-platform WireGuard client for iOS, macOS, and tvOS. While building Xlarva's obfuscation features, we needed an RFC-compliant way to wrap WireGuard's UDP traffic inside an encrypted, indistinguishable-from-HTTPS transport. The popular [`wstunnel`](https://github.com/erebe/wstunnel) project covers TCP-style encapsulation over WebSockets, but it doesn't preserve UDP semantics — turning a UDP-only protocol like WireGuard into a TCP-over-TCP nightmare on lossy networks.

MASQUE CONNECT-UDP (RFC 9298) solves exactly this: it carries UDP datagrams natively over HTTP/3 / QUIC, the same protocol stack used by Apple iCloud Private Relay and Cloudflare WARP. We built this server (and a thin reference client) so any Xlarva user — or anyone else who wants a self-hosted, RFC-standard alternative to closed proxy services — can stand up their own relay in minutes.

If you're looking for a Xlarva-compatible WireGuard client that speaks this protocol out of the box, see <https://xlarva.app/>.

## Features

- **RFC 9298 compliant** — CONNECT-UDP over HTTP/3 with QUIC DATAGRAM frames
- **Client + Server** — single binary with `client` / `server` subcommands
- **High throughput** — BBR2 congestion control, batched I/O, zero-copy forwarding
- **Obfuscation** — traffic appears as standard HTTPS/QUIC on port 443
- **Authentication** — optional Bearer token for client verification
- **SNI override** — supports domain fronting via custom TLS SNI
- **Auto-reconnect** — client automatically reconnects with exponential backoff
- **Static binaries** — musl-linked, runs on any Linux (including RouterOS containers)

## Quick Start

### Server

```bash
masque-tunnel server \
  --listen [::]:443 \
  --cert cert.pem \
  --key key.pem \
  --auth-token your-secret-token
```

### Client

```bash
masque-tunnel client \
  --listen 127.0.0.1:51820 \
  --proxy-url https://your-server.com \
  --target 10.0.0.1:51820 \
  --auth-token your-secret-token
```

This creates a local UDP endpoint at `127.0.0.1:51820` that tunnels all traffic through the MASQUE proxy to `10.0.0.1:51820`.

## Usage

```
masque-tunnel <COMMAND>

Commands:
  client  Run as MASQUE CONNECT-UDP client
  server  Run as MASQUE CONNECT-UDP proxy server
```

### Client Options

| Flag | Short | Description | Required |
|------|-------|-------------|----------|
| `--listen` | `-l` | Local UDP listen address | yes |
| `--proxy-url` | `-p` | MASQUE proxy server URL | yes |
| `--target` | `-t` | Target UDP endpoint (host:port) | yes |
| `--sni` | | TLS SNI override for domain fronting | no |
| `--auth-token` | | Bearer token for authentication | no |

### Server Options

| Flag | Short | Description | Default |
|------|-------|-------------|---------|
| `--listen` | `-l` | Listen address | `[::]:443` |
| `--cert` | | TLS certificate PEM file | required |
| `--key` | | TLS private key PEM file | required |
| `--auth-token` | | Required Bearer token | none |

## Deployment

### TLS Certificate

```bash
# Self-signed (testing)
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 \
  -keyout key.pem -out cert.pem -days 365 -nodes \
  -subj '/CN=masque-proxy'

# Let's Encrypt (production)
sudo certbot certonly --standalone -d your-domain.com
```

### Firewall

```bash
# QUIC uses UDP, not TCP
sudo ufw allow 443/udp
```

### systemd Service

```ini
# /etc/systemd/system/masque-tunnel.service
[Unit]
Description=MASQUE CONNECT-UDP Tunnel
After=network.target

[Service]
ExecStart=/usr/local/bin/masque-tunnel server \
  --listen [::]:443 \
  --cert /etc/letsencrypt/live/your-domain.com/fullchain.pem \
  --key /etc/letsencrypt/live/your-domain.com/privkey.pem \
  --auth-token your-secret-token
Restart=always
User=root
AmbientCapabilities=CAP_NET_BIND_SERVICE

[Install]
WantedBy=multi-user.target
```

## Apple Platform Integration

On Apple platforms (iOS / macOS / tvOS), you can use `NWConnection` with `ProxyConfiguration` to route UDP traffic through the MASQUE proxy natively — no client binary needed.

```swift
import Network

let proxyUrl = URL(string: "https://your-server.com")!
let parameters = NWParameters.udp

let relayHop = ProxyConfiguration.RelayHop(
    http3RelayEndpoint: .url(proxyUrl),
    additionalHTTPHeaderFields: ["proxy-authorization": "Bearer your-secret-token"]
)
let proxyConfig = ProxyConfiguration(relayHops: [relayHop])
let privacyContext = NWParameters.PrivacyContext(description: "MASQUE proxy")
privacyContext.proxyConfigurations = [proxyConfig]
parameters.setPrivacyContext(privacyContext)

let connection = NWConnection(host: "10.0.0.1", port: 51820, using: parameters)
connection.stateUpdateHandler = { state in
    print("Connection state: \(state)")
}
connection.start(queue: .global(qos: .userInitiated))
```

The system's Network framework handles the HTTP/3 CONNECT-UDP handshake, QUIC transport, and DATAGRAM framing automatically. All UDP packets sent through this `NWConnection` will be tunneled via the MASQUE proxy to the specified target endpoint.

## Build from Source

```bash
cargo build --release
```

The release binary is statically linked (musl) and optimized with LTO.

## Architecture

```
src/
├── main.rs      # CLI entry point (clap subcommands)
├── client.rs    # QUIC/H3 client: local UDP ↔ MASQUE DATAGRAM
├── server.rs    # QUIC/H3 server: MASQUE DATAGRAM ↔ target UDP
└── common.rs    # Shared: varint codec, flush, constants
```

### Protocol Flow

```
WireGuard ──UDP──▶ masque-tunnel client ──QUIC/H3──▶ masque-tunnel server ──UDP──▶ WireGuard
           (local)                        (port 443)                        (target)
```

1. Client binds a local UDP socket and accepts WireGuard packets
2. Establishes QUIC connection to the proxy server (port 443)
3. Sends HTTP/3 extended CONNECT request (`:protocol: connect-udp`)
4. Path: `/.well-known/masque/udp/{target_host}/{target_port}/`
5. Server responds `200` and creates a UDP socket to the target
6. Bidirectional forwarding via QUIC DATAGRAM frames (RFC 9297)

### Performance Optimizations

- **BBR2** congestion control (vs default Reno)
- **Batched I/O** — up to 64 packets per event loop iteration via `try_recv_from`
- **Pre-allocated buffers** — zero per-packet heap allocation in the forwarding path
- **Async target readers** — spawned tokio tasks for target→client direction
- **Non-blocking forwarding** — `try_send` / `try_send_to` on data path

## License

BSD-2-Clause
