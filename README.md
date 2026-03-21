# masque-server

Minimal MASQUE CONNECT-UDP (RFC 9298) proxy server powered by [tokio-quiche](https://github.com/cloudflare/quiche/tree/master/tokio-quiche).

Tunnels UDP traffic through HTTP/3 (QUIC) on port 443, making it indistinguishable from normal HTTPS traffic.

## Usage

```bash
masque-server \
  --listen 0.0.0.0:443 \
  --cert cert.pem \
  --key key.pem \
  --auth-token your-secret-token
```

### Options

| Flag | Description | Default |
|------|-------------|---------|
| `--listen` | Listen address (ip:port) | `0.0.0.0:443` |
| `--cert` | Path to TLS certificate (PEM) | required |
| `--key` | Path to TLS private key (PEM) | required |
| `--auth-token` | Optional Bearer token for authentication | none |

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
# /etc/systemd/system/masque-server.service
[Unit]
Description=MASQUE CONNECT-UDP Proxy
After=network.target

[Service]
ExecStart=/usr/local/bin/masque-server \
  --listen 0.0.0.0:443 \
  --cert /etc/letsencrypt/live/your-domain.com/fullchain.pem \
  --key /etc/letsencrypt/live/your-domain.com/privkey.pem \
  --auth-token your-secret-token
Restart=always
User=masque
AmbientCapabilities=CAP_NET_BIND_SERVICE

[Install]
WantedBy=multi-user.target
```

## Build from Source

```bash
cargo build --release
```

## Protocol

The server implements RFC 9298 CONNECT-UDP:

1. Client establishes QUIC connection to server on port 443
2. Client sends HTTP/3 `CONNECT` request with `:protocol: connect-udp`
3. Path format: `/.well-known/masque/udp/{target_host}/{target_port}/`
4. Server responds `200` and creates a UDP socket to the target
5. Bidirectional forwarding: QUIC DATAGRAM frames ↔ UDP packets

## License

BSD-2-Clause
