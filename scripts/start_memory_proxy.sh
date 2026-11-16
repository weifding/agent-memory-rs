#!/usr/bin/env bash
# =============================================================================
# start_memory_proxy.sh — 启动 agent-memory 代理
#
# 用法:
#   ./scripts/start_memory_proxy.sh          # 前台启动（日志到 stderr）
#   ./scripts/start_memory_proxy.sh --daemon # 后台启动（日志到 logs/proxy.log）
#
# 链路：代理(8888) --HTTP直连--> 104:8888（Docker 容器 agent-memory）
# 2026-10-28 起 Clash 已加内网直连规则（Merge.yaml prepend-rules），
# 不再需要 SSH 隧道；若直连异常可临时恢复隧道：
#   ssh -f -N -L 8889:192.168.25.104:8888 root@192.168.25.104
#   并把 deployment/proxy.yaml 的 target 改回 http://127.0.0.1:8889/mcp
# =============================================================================
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
PROXY_BIN="$PROJECT_ROOT/target/release/agent-memory-proxy.exe"
CONFIG="$PROJECT_ROOT/deployment/proxy.yaml"
LOG_DIR="$PROJECT_ROOT/logs"
PROXY_LOG="$LOG_DIR/proxy.log"

DAEMON=false
[ "${1:-}" = "--daemon" ] && DAEMON=true

if tasklist 2>/dev/null | grep -qi agent-memory-proxy; then
    echo "[OK] 代理已在运行 (8888)"
    exit 0
fi

[ -x "$PROXY_BIN" ] || { echo "[ERROR] 代理二进制不存在: $PROXY_BIN（先 cargo build --release --bin agent-memory-proxy）" >&2; exit 1; }

if $DAEMON; then
    mkdir -p "$LOG_DIR"
    nohup "$PROXY_BIN" --config "$CONFIG" >> "$PROXY_LOG" 2>&1 &
    echo $! > /tmp/agent-memory-proxy.pid
    sleep 1
    if netstat -an | grep -q ":8888 .*LISTENING\|:8888.*LISTENING"; then
        echo "[OK] 代理已后台启动 (8888)，日志: $PROXY_LOG"
    else
        echo "[WARN] 代理可能启动失败，查看日志: $PROXY_LOG" >&2
        tail -5 "$PROXY_LOG" || true
        exit 1
    fi
else
    echo "[INFO] 前台启动代理 (Ctrl+C 退出)"
    exec "$PROXY_BIN" --config "$CONFIG"
fi
