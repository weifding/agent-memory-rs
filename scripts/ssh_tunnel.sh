#!/usr/bin/env bash
# =============================================================================
# ssh_tunnel.sh — SSH 隧道管理（绕过 Clash TUN 对 TCP 8888 的劫持）
#
# 用法:
#   ./scripts/ssh_tunnel.sh start    # 启动隧道（后台）
#   ./scripts/ssh_tunnel.sh stop     # 停止隧道
#   ./scripts/ssh_tunnel.sh status   # 查看状态
#   ./scripts/ssh_tunnel.sh restart  # 重启隧道
# =============================================================================
set -euo pipefail

REMOTE_HOST="${REMOTE_HOST:-root@192.168.25.104}"
REMOTE_PORT="${REMOTE_PORT:-8888}"
LOCAL_PORT="${LOCAL_PORT:-8889}"
PID_FILE="/tmp/agent-memory-ssh-tunnel.pid"

start() {
    if [ -f "$PID_FILE" ] && kill -0 "$(cat "$PID_FILE")" 2>/dev/null; then
        echo "[OK] 隧道已在运行 (PID=$(cat "$PID_FILE"))"
        return 0
    fi
    echo "[INFO] 启动 SSH 隧道: 127.0.0.1:${LOCAL_PORT} -> ${REMOTE_HOST}:${REMOTE_PORT}"
    ssh -f -N -L "${LOCAL_PORT}:${REMOTE_HOST#*@}:${REMOTE_PORT}" "$REMOTE_HOST"
    # 获取 ssh 进程 PID
    sleep 0.5
    SSH_PID=$(netstat -ano | grep "127.0.0.1:${LOCAL_PORT}" | grep LISTENING | head -1 | awk '{print $NF}')
    if [ -n "$SSH_PID" ]; then
        echo "$SSH_PID" > "$PID_FILE"
        echo "[OK] 隧道已启动 (PID=${SSH_PID}, 端口 ${LOCAL_PORT})"
    else
        echo "[WARN] 隧道可能已启动，但未获取到 PID"
    fi
}

stop() {
    if [ -f "$PID_FILE" ]; then
        PID=$(cat "$PID_FILE")
        if kill -0 "$PID" 2>/dev/null; then
            kill "$PID" 2>/dev/null || true
            echo "[OK] 隧道已停止 (PID=${PID})"
        else
            echo "[INFO] 隧道进程已退出"
        fi
        rm -f "$PID_FILE"
    else
        echo "[INFO] 无隧道 PID 文件"
    fi
    # 清理残留的 ssh 隧道进程
    taskkill //F //FI "IMAGENAME eq ssh.exe" //FI "WINDOWTITLE eq *8889*" 2>/dev/null || true
}

status() {
    if [ -f "$PID_FILE" ] && kill -0 "$(cat "$PID_FILE")" 2>/dev/null; then
        echo "[OK] 隧道运行中 (PID=$(cat "$PID_FILE"), 端口 ${LOCAL_PORT})"
    else
        echo "[STOP] 隧道未运行"
    fi
    netstat -an | grep "${LOCAL_PORT}" | grep LISTENING | head -3 || true
}

case "${1:-status}" in
    start)   start ;;
    stop)    stop ;;
    restart) stop; sleep 1; start ;;
    status)  status ;;
    *)       echo "用法: $0 {start|stop|restart|status}"; exit 1 ;;
esac
