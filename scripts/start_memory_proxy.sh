#!/usr/bin/env bash
# =============================================================================
# start_memory_proxy.sh — 启动 agent-memory 代理
#
# 用法:
#   ./scripts/start_memory_proxy.sh          # 前台启动（日志到 stderr）
#   ./scripts/start_memory_proxy.sh --daemon # 后台启动（日志到 logs/proxy.log）
#   ./scripts/start_memory_proxy.sh --detach # 同 --daemon，但脱离调用方进程树（在 ZCode 等 agent 会话里一律用这个）
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
DETACH=false
[ "${1:-}" = "--detach" ] && DETACH=true

if tasklist 2>/dev/null | grep -qi agent-memory-proxy; then
    echo "[OK] 代理已在运行 (8888)"
    exit 0
fi

[ -x "$PROXY_BIN" ] || { echo "[ERROR] 代理二进制不存在: $PROXY_BIN（先 cargo build --release --bin agent-memory-proxy）" >&2; exit 1; }

# --detach：经 WMI 重新拉起本脚本走 --daemon，代理不属于调用方的 job。
# 在 ZCode 等 agent 会话里直接 --daemon，代理会落在该应用的 job 里，应用一退出/更新
# 就被一并带走（2026-11-23 ZCode 更新重启，代理随之消失 31 小时无人察觉）。
if $DETACH; then
    BASH_WIN="$(cygpath -w "$BASH")"
    if ! MSYS_NO_PATHCONV=1 MSYS2_ARG_CONV_EXCL='*' powershell -NoProfile -Command \
        "Invoke-CimMethod -ClassName Win32_Process -MethodName Create -Arguments @{ CommandLine = '\"$BASH_WIN\" -lc \"cd $PROJECT_ROOT && $SCRIPT_DIR/start_memory_proxy.sh --daemon\"' } | Out-Null"; then
        echo "[ERROR] 脱离启动失败（WMI 不可用）" >&2
        exit 1
    fi
    for _ in 1 2 3 4 5 6 7 8 9 10; do
        sleep 1
        netstat -an | grep -q ":8888 .*LISTENING\|:8888.*LISTENING" && break
    done
    if netstat -an | grep -q ":8888 .*LISTENING\|:8888.*LISTENING"; then
        echo "[OK] 代理已脱离当前进程树后台启动 (8888)，日志: $PROXY_LOG"
    else
        echo "[ERROR] 脱离启动后 8888 未监听，查看日志: $PROXY_LOG" >&2
        tail -5 "$PROXY_LOG" 2>/dev/null || true
        exit 1
    fi
    exit 0
fi

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
