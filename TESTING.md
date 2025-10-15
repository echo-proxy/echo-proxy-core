# 测试执行指南

本文档说明如何运行 echo-proxy-core 项目的测试。

## 测试结构

项目包含以下类型的测试：

### 1. 单元测试
- **位置**: 每个包的 `src/lib.rs` 中的 `#[cfg(test)]` 模块
- **数量**:
  - `core-lib`: 7 个测试
  - `client`: 9 个测试
  - `server`: 8 个测试

### 2. 集成测试
- **位置**: `integration-tests` 包
- **数量**: 8 个测试
- **功能**: 验证多个模块之间的交互

## 运行测试的方法

### 方法1: 运行所有测试（推荐）
```bash
./run_all_tests.sh
```

### 方法2: 分别运行单元测试和集成测试
```bash
# 运行所有单元测试
cargo test --workspace --lib

# 运行集成测试
cargo test -p integration-tests
```

### 方法3: 运行特定包的测试
```bash
# 运行 core-lib 测试
cargo test -p core-lib

# 运行 client 测试
cargo test -p client

# 运行 server 测试
cargo test -p server
```

### 方法4: 运行特定测试
```bash
# 运行包含特定名称的测试
cargo test test_encode_decode

# 运行集成测试中的特定测试
cargo test -p integration-tests test_http_header
```

## 测试覆盖的功能

### 单元测试功能
- **core-lib**: 主机地址编码/解码、随机字符生成
- **client**: HTTP请求构建、请求头解析、URL构建
- **server**: 版本检查、用户认证、消息解析

### 集成测试功能
- 跨模块的编码/解码一致性
- HTTP/HTTPS请求检测
- WebSocket URL构建
- 版本兼容性检查
- 用户认证流程

## 测试结果解读

- ✅ **通过**: 所有断言成功
- ❌ **失败**: 至少一个断言失败
- ⏸️ **忽略**: 测试被标记为忽略
- 📊 **测量**: 性能基准测试

## 故障排除

如果测试失败，请检查：

1. 依赖是否正确安装
2. 网络连接是否正常（对于需要外部资源的测试）
3. 测试环境配置是否正确
4. 代码变更是否破坏了现有功能

## 添加新测试

1. 单元测试：在对应包的 `src/lib.rs` 的 `#[cfg(test)]` 模块中添加
2. 集成测试：在 `integration-tests/src/lib.rs` 中添加
3. 确保测试使用 `#[tokio::test]` 对于异步测试
4. 运行测试验证新测试通过

## 持续集成

项目配置了 GitHub Actions 自动运行所有测试，确保代码质量。