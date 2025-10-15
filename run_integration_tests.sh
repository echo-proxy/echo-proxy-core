#!/bin/bash

# 集成测试运行脚本
# 这个脚本用于运行项目的集成测试

echo "=== 运行基础代理集成测试 ==="
cd tests
rustc --test basic_proxy_integration.rs --extern core_lib=../target/debug/deps/libcore_lib.rlib --extern semver=../target/debug/deps/libsemver-*.rlib -L dependency=../target/debug/deps
./basic_proxy_integration

echo "=== 运行代理工作流集成测试 ==="
rustc --test proxy_workflow_integration.rs --extern core_lib=../target/debug/deps/libcore_lib.rlib --extern semver=../target/debug/deps/libsemver-*.rlib -L dependency=../target/debug/deps
./proxy_workflow_integration

echo "=== 集成测试完成 ==="