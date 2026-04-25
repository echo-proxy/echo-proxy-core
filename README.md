# echo-proxy-core

Echo Proxy is a secure, transparent HTTP/HTTPS proxy tunneled over WebSocket.

> **Breaking change (mux protocol):** This version introduces a single persistent
> WebSocket connection with frame-level multiplexing. Client and server must be
> upgraded together — the old per-request WebSocket protocol is no longer supported.

## Architecture

```
Browser → echo-proxy-client (local :9002) ──wss──▶ Nginx ──▶ echo-proxy-server (:9001) → Internet
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

Both `echo-proxy-client` and `echo-proxy-server` load `config.toml` from the current directory by default. Create a single file for both:

```toml
[client]
endpoint = "wss://example.org/proxy/"
user = "user1"
# host = "127.0.0.1"
# port = 9002

[server]
users = ["user1", "user2"]
# host = "127.0.0.1"
# port = 9001
```

Parameter precedence: **CLI flags > config file > built-in defaults**

### Client

```bash
echo-proxy-client
```

Override individual values at runtime:

```bash
echo-proxy-client --user user2 --port 9003
```

Use a custom config path:

```bash
echo-proxy-client --config /etc/echo-proxy/config.toml
```

```
Usage: echo-proxy-client [OPTIONS]

Options:
      --config <CONFIG>      Path to config file (TOML) [default: config.toml]
      --endpoint <ENDPOINT>  Remote server address
      --user <USER>          User id
      --host <HOST>          Local server address [default: 127.0.0.1]
      --port <PORT>          Local server port [default: 9002]
  -h, --help                 Print help
  -V, --version              Print version
```

### Server

```bash
echo-proxy-server
```

Configure your reverse proxy to forward WebSocket traffic to echo-proxy-server.

```nginx
# example nginx config
server {
    listen                      443 ssl http2;
    ssl_certificate             /root/cert/cert.pem;
    ssl_certificate_key         /root/cert/key.pem;
    server_name                 www.example.org;

    location /proxy/ {
        proxy_redirect off;
        proxy_pass http://127.0.0.1:9001;
        proxy_http_version 1.1;
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection "upgrade";
        proxy_set_header Host $http_host;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
    }
}
```

```bash
echo-proxy-server
```

```
Usage: echo-proxy-server [OPTIONS]

Options:
      --config <CONFIG>  Path to config file (TOML) [default: config.toml]
      --users <USERS>    Allowed user IDs
      --host <HOST>      Local server address [default: 127.0.0.1]
      --port <PORT>      Local server port [default: 9001]
  -h, --help             Print help
  -V, --version          Print version
```
