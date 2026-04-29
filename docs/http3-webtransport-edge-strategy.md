# HTTP/3 And WebTransport Edge Strategy

## 背景

服务需要在同一个公网入口上同时响应普通 HTTP/3 请求和 WebTransport 请求：

- 普通 HTTP/3：例如 `/health`、`/api/*`。
- WebTransport：例如 `/wt` 或 `/proxy`，承载现有代理隧道。

这样可以让外部只看到一个 HTTPS/HTTP/3 域名和端口，同时避免把 Rust WebTransport 服务直接裸露到公网。

## 当前问题

现有服务端使用 `wtransport` 直接提供 WebTransport 能力。它适合承载当前代理隧道，但如果直接暴露公网 UDP 端口，风险集中在 Rust 服务本身：

- QUIC/WebTransport 握手流量直接到达服务。
- 未认证连接会先消耗 session 资源。
- 应用层还缺少明确的 session、stream、IP、用户维度限流。
- 普通 HTTP/3 请求无法和 WebTransport 在同一个 Rust 入口中统一处理。

曾尝试使用 Envoy 作为 HTTP/3 边缘层，但 `wtransport` 客户端报错：

```text
server rejected WebTransport session request
```

根因是 Envoy 返回的 HTTP/3 SETTINGS 没有包含 WebTransport 客户端期望的能力声明，例如 `EnableWebTransport`、`WebTransportMaxSessions`。`allow_extended_connect` 只表示允许扩展 CONNECT 语义，不等价于完整 WebTransport 入口能力。

## Envoy 实测记录

结论：当前不能把 Envoy 当作 `wtransport` 的 WebTransport 终止反向代理使用。这里的“不能用”特指 Envoy 终止 QUIC/TLS/HTTP3 后再把 WebTransport 请求转发给后端；不是指 Envoy 完全不能处理 HTTP/3，也不是指 UDP 四层转发不可用。

已验证组合：

- `envoyproxy/envoy:v1.33-latest`：失败。
- `envoyproxy/envoy:v1.38.0`：失败。
- Envoy UDP passthrough：成功。
- 客户端直连 Rust `wtransport` server：成功。

失败配置大致为：

- Envoy 使用 `QuicDownstreamTransport` 终止客户端侧 QUIC/TLS。
- `HttpConnectionManager` 使用 `codec_type: HTTP3`。
- `http3_protocol_options.allow_extended_connect: true`。
- HCM 和 route 均尝试配置 `upgrade_configs: CONNECT`。
- upstream 使用 HTTP/3 + QUIC 转发到 Rust `wtransport` server。

客户端 debug 日志显示，Envoy 返回的 HTTP/3 SETTINGS 类似：

```text
EnableConnectProtocol: 1
H3Datagram: 1
QPackMaxTableCapacity: 65536
QPackBlockedStreams: 100
MaxFieldSectionSize: 61440
```

但 `wtransport` 直连 Rust server 时，服务端 SETTINGS 包含：

```text
EnableWebTransport: 1
WebTransportMaxSessions: 1
EnableConnectProtocol: 1
H3Datagram: 1
```

`wtransport` 客户端依赖 `EnableWebTransport` / `WebTransportMaxSessions` 判断对端是否支持 WebTransport session。Envoy 缺少这两个 SETTINGS 时，客户端会直接拒绝 session，并报：

```text
server rejected WebTransport session request
```

因此，`allow_extended_connect` 只能说明 Envoy 支持 HTTP/3 extended CONNECT / CONNECT 类能力，不足以满足 `wtransport` 对 WebTransport over HTTP/3 的完整协商要求。

另外，排查中还确认了两个容易混淆的问题：

- Colima/macOS 下 `127.0.0.1:4433/udp` 转发不稳定或不可用，测试时应优先使用 Colima VM IP，例如 `192.168.64.2:4433`。
- `wtransport` 的 cert-hash pinning 要求自签证书使用 ECDSA P-256，且有效期不超过 14 天；否则会表现为 `invalid peer certificate: UnknownIssuer`，这和 Envoy WebTransport 能力无关。

当前可用的 Envoy 方案是 UDP passthrough：

```text
client --QUIC/TLS/WebTransport--> Envoy UDP proxy --UDP--> Rust wtransport server
```

这种方式下：

- TLS/HTTP3/WebTransport 仍由 Rust server 终止。
- Envoy 只做 UDP 四层转发，不处理证书、不解析 HTTP/3、不按 path 分流。
- `--cert-hash` 对应 Rust server 使用的证书。

如果未来要重新评估 Envoy 终止模式，需要确认 Envoy 是否已经能在 downstream HTTP/3 SETTINGS 中声明 `EnableWebTransport` 和 `WebTransportMaxSessions`，并且能把 WebTransport session 正确转发到 upstream。

## 可选路线

### 1. 继续寻找合适边缘层

目标是让边缘层直接支持：

- HTTP/3 downstream。
- WebTransport over HTTP/3。
- 正确返回 WebTransport 相关 HTTP/3 SETTINGS。
- 按 path 区分普通 HTTP/3 和 WebTransport。
- 将 WebTransport 请求安全转发到后端。

优点：

- Rust 服务不直接暴露公网。
- TLS、限流、日志、连接保护可以放在成熟边缘层。
- 普通 HTTP/3 和 WebTransport 可以共用同一个域名。

风险：

- Envoy、Caddy、Nginx 等对普通 HTTP/3 支持相对成熟，但 WebTransport 反向代理支持仍不稳定。
- 如果边缘层不能正确声明 WebTransport SETTINGS，浏览器或 `wtransport` 客户端会直接拒绝连接。
- L4 UDP 转发可以保留 WebTransport 能力，但无法按 HTTP path 分流普通 HTTP/3 和 WebTransport。

### 2. 换 Rust WebTransport/H3 实现库

目标是在 Rust 服务内直接实现同端口处理：

- `GET /health` 等普通 HTTP/3 请求。
- `CONNECT /wt` WebTransport session。
- 现有认证与 bidirectional stream 代理协议。

更推荐优先评估 `quinn` + `h3` / WebTransport 生态，而不是直接使用 `quiche`。

原因：

- `quiche` 偏底层，需要应用自己管理 UDP socket、connection id 路由、定时器、pacing、HTTP/3 事件循环和 WebTransport 细节。
- `quinn` + `h3` 更接近当前 Rust async 生态，抽象层级更适合服务端应用。
- 如果 `h3` 生态能同时处理普通 HTTP/3 和 WebTransport，可以避免承担过多底层协议维护成本。

风险：

- 需要验证库是否真正支持当前浏览器和 `wtransport` 客户端期望的 WebTransport SETTINGS。
- 需要迁移当前 `wtransport` 的 session、stream、认证和重连语义。
- 需要补齐大量集成测试，避免协议兼容性回归。

### 3. 继续使用 `wtransport`，先做防护加固

这是短期风险最低的方案。

可以先保持当前 WebTransport 服务实现不变，同时补齐：

- 最大并发 session 限制。
- 单 session 最大并发 stream 限制。
- 未认证 session 超时。
- 每 IP / 每 user 限流。
- 认证失败次数限制。
- 连接数、stream 数、拒绝数、限流数等观测指标。
- 认证前移到 session request 阶段，尽量避免未认证客户端进入后续 stream 循环。

## 推荐决策

短期不要直接用 `quiche` 重写主服务。

推荐推进顺序：

1. 保留当前 `wtransport` 实现，优先补应用层过载防御和认证前置。
2. 继续调研可用边缘层，但不依赖 Envoy 当前 WebTransport 反代能力。
3. 新建最小 POC，验证 `quinn` + `h3` / WebTransport 生态能否在同一 UDP 端口同时处理：
   - `GET /health`
   - `CONNECT /wt`
   - 一个 bidirectional stream 的 `Hello` 认证
4. POC 成功后，再迁移现有代理 stream 协议。
5. 如果高层 Rust 生态无法满足需求，再考虑 `quiche`。

## 目标架构

理想状态：

```text
Internet
  ↓
example.com:443 UDP/TCP
  ↓
HTTP/3 capable edge or Rust H3/WT service
  ├── /health, /api/*  → 普通 HTTP/3
  └── /wt, /proxy      → WebTransport proxy tunnel
```

如果使用边缘层：

```text
Internet
  ↓
Edge:443 UDP
  ├── normal HTTP/3 route → app-server
  └── WebTransport route → echo-proxy server
```

如果使用 Rust 单服务：

```text
Internet
  ↓
Rust H3/WebTransport service:443 UDP
  ├── normal HTTP/3 handler
  └── WebTransport session handler
```

## 当前结论

问题不在于“是否应该隐藏 WebTransport 服务”，这个方向是对的。关键在于实现方式：

- Envoy 当前表现不满足 `wtransport` 客户端对 WebTransport SETTINGS 的要求。
- `quiche` 能提供底层能力，但会显著增加协议实现和维护成本。
- 更稳妥的中期方向是验证 `quinn` + `h3` / WebTransport 生态。
- 在任何替换方案落地前，当前 `wtransport` 服务需要先补齐限流、认证前置和观测能力。
