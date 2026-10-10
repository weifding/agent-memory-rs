#!/usr/bin/env bash
# =============================================================================
# run_proxy.sh — 编译并启动本地 MCP 代理服务器
#
# 用法:
#   ./scripts/run_proxy.sh                                          # 使用 deployment/proxy.yaml
#   ./scripts/run_proxy.sh --config my-config.yaml                  # 指定配置
#   TARGET=http://10.0.0.5:8888/mcp ./scripts/run_proxy.sh          # 环境变量覆盖
#   ./scripts/run_proxy.sh --build-only                             # 只编译不运行
# =============================================================================
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$PROJECT_ROOT"

CONFIG_FILE="${PROJECT_ROOT}/deployment/proxy.yaml"
BUILD_ONLY=false
EXTRA_ARGS=()

for arg in "$@"; do
  case "$arg" in
    --build-only) BUILD_ONLY=true ;;
    --config) : ;; # 占位，实际值在下一个参数
    *) EXTRA_ARGS+=("$arg") ;;
  esac
done

# 解析 --config <path>
prev=""
for arg in "$@"; do
  if [ "$prev" = "--config" ]; then
    CONFIG_FILE="$arg"
  fi
  prev="$arg"
done

# 如无配置文件则从模板创建
if [ ! -f "$CONFIG_FILE" ]; then
  if [ -f "${PROJECT_ROOT}/deployment/proxy.example.yaml" ]; then
    echo "[INFO] 配置文件不存在，从模板创建: $CONFIG_FILE"
    cp "${PROJECT_ROOT}/deployment/proxy.example.yaml" "$CONFIG_FILE"
    echo "[ACTION] 请编辑 $CONFIG_FILE 填入实际的 access_key 和 server_auth_token"
  fi
fi

# ---- 编译 ----
echo "[1/2] 编译 agent-memory-proxy ..."
cargo build --release --bin agent-memory-proxy

BINARY="${PROJECT_ROOT}/target/release/agent-memory-proxy"
if [ ! -f "$BINARY" ]; then
  BINARY="${PROJECT_ROOT}/target/release/agent-memory-proxy.exe"
fi

if [ "$BUILD_ONLY" = true ]; then
  echo "[OK] 编译完成: $BINARY"
  exit 0
fi

# ---- 运行 ----
echo "[2/2] 启动代理 ..."
echo "  配置: $CONFIG_FILE"
echo "  二进制: $BINARY"
echo ""

if [ ${#EXTRA_ARGS[@]} -gt 0 ]; then
  "$BINARY" --config "$CONFIG_FILE" "${EXTRA_ARGS[@]}"
else
  "$BINARY" --config "$CONFIG_FILE"
fi
