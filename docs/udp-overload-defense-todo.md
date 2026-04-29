# UDP Overload Defense TODO

当前服务端主要依赖 `wtransport`/QUIC 默认能力，应用层还缺少明确的过载防御。后续建议补齐：

- [ ] 增加最大并发 session 限制，避免 `accept()` 后无限创建任务。
- [ ] 增加单个 session 的最大并发 stream 限制，避免无限 `spawn` proxy stream。
- [ ] 增加按来源 IP 或用户维度的限流策略。
- [ ] 增加过载时的拒绝/丢弃策略，并记录明确日志。
- [ ] 增加队列、并发数、拒绝数、限流次数等观测指标。
- [ ] 评估是否需要调整底层 UDP/QUIC socket buffer 或 transport 参数。
