# echo-proxy-core

Echo Proxy is a secure proxy over WebSocket. Point your browser or system proxy to the local client, and it forwards traffic to the remote server through a single persistent WebSocket connection. 

## Architecture

```
Browser ──▶ client ──wss──▶ Nginx ──▶ server ──▶ Internet
                    one ws connection carries N logical streams
```

### Frame wire format

Each WebSocket binary message is a single frame:

```
| 1 byte type | 4 byte stream_id (big-endian) | payload |
```

| type | name     | direction        | payload                              |
|------|----------|------------------|--------------------------------------|
| 0x10 | HELLO    | client → server  | UTF-8 session token                  |
| 0x01 | OPEN     | client → server  | UTF-8 `host:port`                    |
| 0x02 | OPEN_ACK | server → client  | 1 byte (0 = ok, 1 = failed)          |
| 0x03 | DATA     | both directions  | raw TCP bytes                        |
| 0x04 | CLOSE    | both directions  | empty                                |

Connection flow:
1. Client calls `/control` → receives a session token.
2. Client opens `/mux`, sends `HELLO(token)`.
3. For each HTTP/CONNECT request, client sends `OPEN(id, host:port)`.
4. Server replies `OPEN_ACK(id, ok)` then relays `DATA` frames bidirectionally.

## Usage

### Configuration file

Both `client` and `server` load `config.toml` from the current directory by default. Create a single file for both:

```toml
[client]
endpoint = "wss://example.org/proxy/"
user = "user1"
# host = "127.0.0.1"
# port = 9002

# SOCKS5 proxy (disabled by default).
# socks_host defaults to host (127.0.0.1) when omitted.
# socks_port = 9003
# socks_host = "127.0.0.1"

# Routing rules (all optional)
#
# proxy   — always tunnel through the remote proxy, even if a bypass rule also matches.
# bypass  — connect directly, bypassing the remote proxy.
#
# Pattern syntax:
#   example.com         match any port
#   example.com:8080    match only that port
#   *.example.com       match any subdomain (not the apex domain itself)
#   127.0.0.1           plain IP, any port
#
# proxy = ["github.com", "*.google.com"]
# bypass = ["localhost", "127.0.0.1", "*.lan"]
#
# If these files exist in the current working directory, the client loads them
# automatically. Set explicit paths only when the files live elsewhere.
#
# Default GeoIP file: geoip-only-cn-private.dat
# Default GeoSite file: dlc.dat
#
# bypass_geoip_dat — path to a V2Ray GeoIP dat file (IP targets only).
# bypass_geoip_tags — which tags to extract from the dat (default: ["cn", "private"]).
#
# Download geoip-only-cn-private.dat from:
#   https://github.com/v2fly/geoip/releases/latest/download/geoip-only-cn-private.dat
#
# bypass_geoip_dat = "geoip-only-cn-private.dat"
# bypass_geoip_tags = ["cn", "private"]
#
# bypass_geosite_dat — path to a V2Ray GeoSite dat file (domain targets).
# bypass_geosite_tags — which tags to extract (default: ["cn", "private"]).
#
# Handles domain-based bypass. Complements bypass_geoip_dat which only
# matches bare IP addresses. With both configured, most CN traffic will
# be routed directly regardless of whether the target is a domain or IP.
#
# Download dlc.dat from:
#   https://github.com/v2fly/domain-list-community/releases/latest/download/dlc.dat
#
# bypass_geosite_dat = "dlc.dat"
# bypass_geosite_tags = ["cn", "private"]
#
# Matching priority (highest first):
#   1. proxy           — force proxy
#   2. bypass          — direct connect
#   3. bypass_geosite  — domain rules, direct connect
#   4. bypass_geoip    — CIDR rules (IP targets only), direct connect
#   5. default         — proxy
#
# Log level (optional, default: info):
# Supported values: off, error, warn, info, debug, trace
# At `info`: each request logs the routing result (direct or proxy).
# At `debug`: also logs detailed matching reasons and stream-level events.
#
# log_level = "info"

[server]
users = ["user1", "user2"]
# host = "127.0.0.1"
# port = 9001
# log_level = "info"
```

Parameter precedence: **CLI flags > config file > built-in defaults**

> CLI flags only cover `endpoint`, `user`, `host`, `port` (client) and `users`, `host`, `port` (server). Routing rules and `log_level` can only be set via the config file.

### Client

Run the installed binary:

```bash
client
```

Or from source:

```bash
cargo run -p client
```

Override individual values at runtime:

```bash
client --user user2 --port 9003
```

Use a custom config path:

```bash
client --config /etc/echo-proxy/config.toml
```

```
Usage: client [OPTIONS]

Options:
      --config <CONFIG>        Path to config file (TOML) [default: config.toml]
      --endpoint <ENDPOINT>    Remote server address
      --user <USER>            User id
      --host <HOST>            Local HTTP server address [default: 127.0.0.1]
      --port <PORT>            Local HTTP server port [default: 9002]
      --socks-host <SOCKS_HOST>  Local SOCKS5 server address (defaults to host)
      --socks-port <SOCKS_PORT>  Local SOCKS5 server port (SOCKS5 disabled when unset)
  -h, --help                   Print help
  -V, --version                Print version
```

### Server

Run the installed binary:

```bash
server
```

Or from source:

```bash
cargo run -p server
```

Configure your reverse proxy to forward WebSocket traffic to `server`.

```nginx
# example nginx config
server {
    listen                      443 ssl http2;
    ssl_certificate             /root/cert/cert.pem;
    ssl_certificate_key         /root/cert/key.pem;
    server_name                 www.example.org;

    location /proxy/ {
        proxy_redirect off;
        proxy_pass http://127.0.0.1:9001/;
        proxy_http_version 1.1;
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection "upgrade";
        proxy_set_header Host $http_host;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_read_timeout 3600s;
        proxy_send_timeout 3600s;
    }
}
```

```
Usage: server [OPTIONS]

Options:
      --config <CONFIG>  Path to config file (TOML) [default: config.toml]
      --users <USERS>    Allowed user IDs
      --host <HOST>      Local server address [default: 127.0.0.1]
      --port <PORT>      Local server port [default: 9001]
  -h, --help             Print help
  -V, --version          Print version
```
