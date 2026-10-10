#!/usr/bin/env bash
# =============================================================================
# docker_deploy.sh — 在 104 服务器上部署 agent-memory-server 容器
#
# 前置条件: 已运行 docker_build.sh 完成镜像构建 + scp 传输
#
# 用法:
#   ./scripts/docker_deploy.sh                              # 部署（默认 root@192.168.25.104）
#   SSH_USER=deploy SSH_HOST=10.0.0.5 ./scripts/docker_deploy.sh
#   AUTH_TOKEN=my-secret ./scripts/docker_deploy.sh         # 指定鉴权 token
#   ./scripts/docker_deploy.sh --remove                     # 移除已有容器
# =============================================================================
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# ---- 可配置变量 ----
IMAGE_NAME="${IMAGE_NAME:-agent-memory-server}"
IMAGE_TAG="${IMAGE_TAG:-latest}"
SSH_USER="${SSH_USER:-root}"
SSH_HOST="${SSH_HOST:-192.168.25.104}"
REMOTE_DIR="${REMOTE_DIR:-/tmp}"
CONTAINER_NAME="${CONTAINER_NAME:-agent-memory}"
HOST_PORT="${HOST_PORT:-8888}"
CONTAINER_PORT="${CONTAINER_PORT:-8888}"
VOLUME_NAME="${VOLUME_NAME:-agent-memory-data}"
AUTH_TOKEN="${AUTH_TOKEN:-$(openssl rand -hex 32 2>/dev/null || echo 'change-me-in-production')}"
REMOVE_MODE=false

for arg in "$@"; do
  case "$arg" in
    --remove) REMOVE_MODE=true ;;
    *) echo "[WARN] 未知参数: $arg" >&2 ;;
  esac
done

FULL_IMAGE="${IMAGE_NAME}:${IMAGE_TAG}"
TAR_FILE="${IMAGE_NAME}-${IMAGE_TAG}.tar"
REMOTE_CONFIG_DIR="/etc/agent-memory"

echo "=========================================="
echo " 部署 ${FULL_IMAGE} 到 ${SSH_USER}@${SSH_HOST}"
echo " 容器: ${CONTAINER_NAME} | 端口: ${HOST_PORT}->${CONTAINER_PORT}"
echo " Volume: ${VOLUME_NAME}"
echo "=========================================="

# ---- 移除模式 ----
if [ "$REMOVE_MODE" = true ]; then
  echo "[REMOVE] 停止并删除容器 ..."
  ssh "${SSH_USER}@${SSH_HOST}" "docker stop ${CONTAINER_NAME} 2>/dev/null; docker rm ${CONTAINER_NAME} 2>/dev/null; echo done"
  echo "[OK] 容器已移除"
  exit 0
fi

# ---- 1. 加载镜像 ----
echo "[1/4] 加载镜像 ..."
ssh "${SSH_USER}@${SSH_HOST}" "docker load -i ${REMOTE_DIR}/${TAR_FILE} && echo load-ok"

# ---- 2. 准备配置目录 ----
echo "[2/4] 准备配置目录 ..."
ssh "${SSH_USER}@${SSH_HOST}" "mkdir -p ${REMOTE_CONFIG_DIR}"

# 传输配置文件（如本地有 deployment/config.yaml）
if [ -f "${PROJECT_ROOT}/deployment/config.yaml" ]; then
  scp "${PROJECT_ROOT}/deployment/config.yaml" "${SSH_USER}@${SSH_HOST}:${REMOTE_CONFIG_DIR}/config.yaml"
  echo "[OK] 配置文件已传输"
fi

# ---- 3. 停止旧容器 ----
echo "[3/4] 停止旧容器 ..."
ssh "${SSH_USER}@${SSH_HOST}" "docker stop ${CONTAINER_NAME} 2>/dev/null; docker rm ${CONTAINER_NAME} 2>/dev/null; echo cleanup-ok"

# ---- 4. 启动新容器 ----
echo "[4/4] 启动容器 ..."
ssh "${SSH_USER}@${SSH_HOST}" "docker run -d \
  --name ${CONTAINER_NAME} \
  --restart unless-stopped \
  -p ${HOST_PORT}:${CONTAINER_PORT} \
  -v ${VOLUME_NAME}:/data \
  -e AGENT_MEMORY_SERVER__AUTH_TOKEN='${AUTH_TOKEN}' \
  ${FULL_IMAGE}"

# ---- 验证 ----
echo ""
echo "[VERIFY] 容器状态:"
ssh "${SSH_USER}@${SSH_HOST}" "docker ps --filter name=${CONTAINER_NAME} --format 'table {{.Names}}\t{{.Status}}\t{{.Ports}}'"

echo ""
echo "=========================================="
echo " 部署完成！"
echo ""
echo " 服务地址: http://${SSH_HOST}:${HOST_PORT}/mcp"
echo " 鉴权 token: ${AUTH_TOKEN}"
echo ""
echo " 记录此 token，本地代理配置中 server_auth_token 填此值"
echo ""
echo " 管理命令:"
echo "   查看日志: ssh ${SSH_USER}@${SSH_HOST} 'docker logs -f ${CONTAINER_NAME}'"
echo "   停止服务: ssh ${SSH_USER}@${SSH_HOST} 'docker stop ${CONTAINER_NAME}'"
echo "   移除容器: ./scripts/docker_deploy.sh --remove"
echo "=========================================="
