#!/usr/bin/env bash
# =============================================================================
# build.sh —— 跨平台构建 agent-memory-server（含 Kùzu 图谱 feature）
#
# 各操作系统适配要点（源自 AGENTS.md 约束）：
#   macOS  (darwin/arm64, darwin/amd64):
#     - 使用系统 Apple clang 即可编译 kuzu 0.11.x（无需 GCC 12）
#     - 需 Xcode Command Line Tools（xcode-select --install）
#     - kuzu 官方预编译产物对 apple Silicon 支持良好
#   Linux  (x86_64/aarch64):
#     - kuzu C++ 依赖使用 AVX-512 FP16 指令，必须 GCC >= 12（clang >= 16 亦可）
#     - 本脚本自动检测编译器版本，不满足则报错并给出安装建议
#     - glibc >= 2.28（CentOS 7 等老系统请用 musl 静态编译：--musl）
#   Windows (msvc):
#     - 请用 scripts/build.ps1（PowerShell），依赖 VS Build Tools 2022
#
# 用法:
#   ./scripts/build.sh              # 构建图谱版 release
#   ./scripts/build.sh --no-graph   # 仅基础版（无 kuzu，最小依赖）
#   ./scripts/build.sh --musl       # Linux musl 静态链接（便于分发老系统）
#   ./scripts/build.sh --target aarch64-unknown-linux-gnu   # 交叉编译
# =============================================================================
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$PROJECT_ROOT"

FEATURES="graph"
TARGET_ARGS=()
EXTRA_ENV=()

for arg in "$@"; do
  case "$arg" in
    --no-graph) FEATURES="" ;;
    --musl)
      TARGET_ARGS+=(--target "$(rustc -vV | sed -n 's/^host: //p' | sed 's/-gnu/-musl/')")
      ;;
    --target) : ;; # 占位，实际 target 由用户以 --target <triple> 传入
    *) TARGET_ARGS+=("$arg") ;;
  esac
done

# ---- 工具链检查 ----
if ! command -v cargo >/dev/null 2>&1; then
  if [ -x "$HOME/.cargo/bin/cargo" ]; then
    export PATH="$HOME/.cargo/bin:$PATH"
  else
    echo "[ERROR] 未找到 cargo，请先安装 Rust: https://rustup.rs" >&2
    exit 1
  fi
fi

OS="$(uname -s)"
ARCH="$(uname -m)"

# ---- 平台特殊适配 ----
case "$OS" in
  Darwin)
    echo "[INFO] macOS ($ARCH)：使用 Apple clang 编译 kuzu"
    if ! xcode-select -p >/dev/null 2>&1; then
      echo "[ERROR] 缺少 Xcode Command Line Tools，请执行: xcode-select --install" >&2
      exit 1
    fi
    # macOS 上链接器默认即 clang，无需额外配置
    ;;
  Linux)
    echo "[INFO] Linux ($ARCH)：检查编译器版本（kuzu 需要 GCC>=12 或 clang>=16）"
    if command -v gcc >/dev/null 2>&1; then
      GCC_MAJOR=$(gcc -dumpversion | cut -d. -f1)
      if [ "${GCC_MAJOR:-0}" -lt 12 ]; then
        echo "[ERROR] GCC 版本过低 ($(gcc -dumpversion))，kuzu 的 C++ 依赖需要 GCC>=12" >&2
        echo "  Debian/Ubuntu: sudo apt install gcc-12 g++-12 && export CC=gcc-12 CXX=g++-12" >&2
        echo "  RHEL/CentOS:   sudo dnf install gcc-toolset-12 && scl enable gcc-toolset-12 bash" >&2
        exit 1
      fi
    elif command -v clang >/dev/null 2>&1; then
      CLANG_MAJOR=$(clang -version | head -1 | grep -oE '[0-9]+' | head -1)
      if [ "${CLANG_MAJOR:-0}" -lt 16 ]; then
        echo "[ERROR] clang 版本过低，kuzu 需要 clang>=16" >&2
        exit 1
      fi
    else
      echo "[ERROR] 未找到 gcc/clang" >&2
      exit 1
    fi
    # 交叉/静态编译时 kuzu 的 cmake 构建需要显式 CC/CXX
    if [ ${#TARGET_ARGS[@]} -gt 0 ]; then
      export CC="${CC:-gcc}"; export CXX="${CXX:-g++}"
    fi
    ;;
  *)
    echo "[ERROR] 不支持的类 Unix 平台: $OS（Windows 请使用 scripts/build.ps1）" >&2
    exit 1
    ;;
esac

# ---- 构建 ----
CMD=(cargo build --release)
if [ -n "$FEATURES" ]; then
  CMD+=(--features "$FEATURES")
fi
if [ ${#TARGET_ARGS[@]} -gt 0 ]; then
  CMD+=("${TARGET_ARGS[@]}")
fi

echo "[INFO] ${CMD[*]}"
"${CMD[@]}"

BINARY="target/release/agent-memory-server"
if [ ${#TARGET_ARGS[@]} -gt 0 ]; then
  TRIPLE="${TARGET_ARGS[-1]}"
  BINARY="target/$TRIPLE/release/agent-memory-server"
fi
echo "[OK] 构建完成: $PROJECT_ROOT/$BINARY"
file "$BINARY"
