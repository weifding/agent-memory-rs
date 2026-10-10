#!/usr/bin/env bash
# =============================================================================
# docker_build.sh — 构建 Docker 镜像（Podman）并推送到 104 服务器
#
# 用法:
#   ./scripts/docker_build.sh                    # 构建 + 推送（默认 root@192.168.25.104）
#   SSH_USER=deploy SSH_HOST=10.0.0.5 ./scripts/docker_build.sh   # 自定义目标
#   ./scripts/docker_build.sh --build-only       # 只构建不推送
# =============================================================================
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$PROJECT_ROOT"

# ---- 可配置变量 ----
IMAGE_NAME="${IMAGE_NAME:-agent-memory-server}"
IMAGE_TAG="${IMAGE_TAG:-latest}"
SSH_USER="${SSH_USER:-root}"
SSH_HOST="${SSH_HOST:-192.168.25.104}"
REMOTE_DIR="${REMOTE_DIR:-/tmp}"
BUILD_ONLY=false

for arg in "$@"; do
  case "$arg" in
    --build-only) BUILD_ONLY=true ;;
    *) echo "[WARN] 未知参数: $arg" >&2 ;;
  esac
done

FULL_IMAGE="${IMAGE_NAME}:${IMAGE_TAG}"
TAR_FILE="${IMAGE_NAME}-${IMAGE_TAG}.tar"

echo "=========================================="
echo " 构建镜像: ${FULL_IMAGE}"
echo " 目标服务器: ${SSH_USER}@${SSH_HOST}"
echo "=========================================="

# ---- 1. 构建镜像 ----
echo "[1/3] podman build ..."
podman build -t "${FULL_IMAGE}" .

echo "[OK] 镜像构建完成: ${FULL_IMAGE}"
podman images "${FULL_IMAGE}"

if [ "$BUILD_ONLY" = true ]; then
  echo "[SKIP] --build-only，跳过推送"
  exit 0
fi

# ---- 2. 导出镜像 ----
echo "[2/3] podman save ..."
podman save -o "${TAR_FILE}" "${FULL_IMAGE}"
echo "[OK] 镜像已导出: ${TAR_FILE} ($(du -h "${TAR_FILE}" | cut -f1))"

# ---- 3. 推送到远端 ----
echo "[3/3] scp 传送到 ${SSH_USER}@${SSH_HOST}:${REMOTE_DIR}/ ..."
scp "${TAR_FILE}" "${SSH_USER}@${SSH_HOST}:${REMOTE_DIR}/"
echo "[OK] 传输完成"

# 清理本地 tar
rm -f "${TAR_FILE}"
echo "[OK] 本地临时文件已清理"

echo ""
echo "=========================================="
echo " 完成！下一步运行部署脚本:"
echo "   ./scripts/docker_deploy.sh"
echo "=========================================="
