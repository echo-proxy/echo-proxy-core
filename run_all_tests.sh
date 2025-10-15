#!/bin/bash

# 完整测试运行脚本
# 这个脚本用于运行项目的所有测试

echo "=== 运行单元测试 ==="
cargo test --workspace --lib

echo "=== 运行集成测试 ==="
cargo test -p integration-tests

echo "=== 所有测试完成 ==="