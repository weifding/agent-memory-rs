#!/usr/bin/env bash
# =============================================================================
# start_workbuddy_memory.sh
# 把 Rust 版 agent-memory-server 装配到本机 WorkBuddy，并实时观察记忆读写日志。
#
# 用法:
#   ./scripts/start_workbuddy_memory.sh           # 构建 + 装配 + tail 日志
#   ./scripts/start_workbuddy_memory.sh --no-build # 跳过构建（用已有二进制）
#   ./scripts/start_workbuddy_memory.sh --remove   # 从 WorkBuddy 移除该 MCP 配置
#
# 设计要点:
#   - 服务器由 WorkBuddy 以 stdio 方式拉起，本脚本不直接启动服务器
#     （Kùzu 单进程约束，避免两个进程抢同一个 graph.kuzu）
#   - 通过 wrapper 把服务器 stderr（tracing 日志）重定向到固定日志文件
#   - 脚本 tail 该日志文件，实时显示记忆读写
# =============================================================================
set -euo pipefail

# ---- 颜色 ----
RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'
CYAN='\033[0;36m'; BOLD='\033[1m'; RESET='\033[0m'
info()  { echo -e "${GREEN}[INFO]${RESET} $*"; }
warn()  { echo -e "${YELLOW}[WARN]${RESET} $*"; }
error() { echo -e "${RED}[ERROR]${RESET} $*" >&2; }
step()  { echo -e "\n${CYAN}${BOLD}==> $*${RESET}"; }

# ---- 路径 ----
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
BINARY="$PROJECT_ROOT/target/release/agent-memory-server"

DATA_DIR="$HOME/.agent-memory"
DB_PATH="$DATA_DIR/memory.db"
GRAPH_PATH="$DATA_DIR/graph.kuzu"
LOG_FILE="$DATA_DIR/server.log"
WRAPPER="$DATA_DIR/run-server.sh"

WORKBUDDY_MCP="$HOME/.workbuddy/connectors/default/mcp.json"
MCP_SERVER_NAME="agent-memory"
LOG_LEVEL="${RUST_LOG:-info}"

# ---- 参数 ----
NO_BUILD=0
REMOVE=0
for arg in "$@"; do
    case "$arg" in
        --no-build) NO_BUILD=1 ;;
        --remove)   REMOVE=1 ;;
        -h|--help)
            sed -n '2,20p' "$0"; exit 0 ;;
        *) error "未知参数: $arg"; exit 1 ;;
    esac
done

# =============================================================================
# 移除配置
# =============================================================================
if [[ $REMOVE -eq 1 ]]; then
    step "从 WorkBuddy 移除 agent-memory MCP 配置"
    if [[ ! -f "$WORKBUDDY_MCP" ]]; then
        error "配置文件不存在: $WORKBUDDY_MCP"; exit 1
    fi
    python3 - "$WORKBUDDY_MCP" "$MCP_SERVER_NAME" <<'PYEOF'
import json, sys
path, name = sys.argv[1], sys.argv[2]
with open(path) as f:
    cfg = json.load(f)
servers = cfg.get("mcpServers", {})
if name in servers:
    del servers[name]
    cfg["mcpServers"] = servers
    with open(path, "w") as f:
        json.dump(cfg, f, indent=2, ensure_ascii=False)
    print(f"  已移除 {name}")
else:
    print(f"  {name} 不存在，无需移除")
PYEOF
    info "完成。请重启 WorkBuddy 使配置生效。"
    exit 0
fi

# =============================================================================
# 1. 构建
# =============================================================================
step "1/5  检查并构建 Rust 二进制"
if [[ $NO_BUILD -eq 1 ]]; then
    warn "跳过构建（--no-build）"
else
    # 确保 cargo 在 PATH
    if ! command -v cargo &>/dev/null; then
        export PATH="$HOME/.cargo/bin:$PATH"
    fi
    # cmake（kuzu C++ 编译需要）
    CMAKE_CANDIDATE="/Users/dingweifeng/Library/Application Support/Doubao/sandbox_runtime/bases/98670218a5f0d8bc9b9ebf3f70881304/bin"
    if [[ -d "$CMAKE_CANDIDATE" ]]; then
        export PATH="$CMAKE_CANDIDATE:$PATH"
    fi
    info "cargo build --release --features graph（首次约 3-5 分钟）..."
    (cd "$PROJECT_ROOT" && cargo build --release --features graph 2>&1 | tail -3)
fi

if [[ ! -x "$BINARY" ]]; then
    error "二进制不存在或不可执行: $BINARY"; exit 1
fi
info "二进制就绪: $BINARY ($(du -h "$BINARY" | cut -f1))"

# =============================================================================
# 2. 数据目录 + wrapper
# =============================================================================
step "2/5  创建数据目录与日志 wrapper"
mkdir -p "$DATA_DIR"

cat > "$WRAPPER" <<EOF
#!/usr/bin/env bash
# 由 start_workbuddy_memory.sh 自动生成 — 勿手动编辑
# 作用: 透传 stdin/stdout 给 MCP 协议，把 stderr（tracing 日志）追加到日志文件
LOG_FILE="\${AGENT_MEMORY_LOG_FILE:-$LOG_FILE}"
mkdir -p "\$(dirname "\$LOG_FILE")"
exec "$BINARY" 2>>"\$LOG_FILE"
EOF
chmod +x "$WRAPPER"
info "wrapper: $WRAPPER"
info "日志:   $LOG_FILE"

# =============================================================================
# 3. 注入 WorkBuddy MCP 配置
# =============================================================================
step "3/5  注入 WorkBuddy MCP 配置"
if [[ ! -f "$WORKBUDDY_MCP" ]]; then
    error "WorkBuddy MCP 配置文件不存在: $WORKBUDDY_MCP"
    error "请确认 WorkBuddy 已安装并至少启动过一次。"
    exit 1
fi

# 备份
BACKUP="${WORKBUDDY_MCP}.bak.$(date +%Y%m%d_%H%M%S)"
cp "$WORKBUDDY_MCP" "$BACKUP"
info "已备份原配置: $BACKUP"

python3 - "$WORKBUDDY_MCP" "$MCP_SERVER_NAME" "$WRAPPER" "$DB_PATH" "$GRAPH_PATH" "$LOG_LEVEL" <<'PYEOF'
import json, sys

path, name, wrapper, db_path, graph_path, log_level = sys.argv[1:7]

with open(path) as f:
    cfg = json.load(f)

servers = cfg.setdefault("mcpServers", {})
servers[name] = {
    "type": "stdio",
    "command": wrapper,
    "args": [],
    "env": {
        "AGENT_MEMORY_STORAGE__DB_PATH": db_path,
        "AGENT_MEMORY_GRAPH__ENABLED": "true",
        "AGENT_MEMORY_GRAPH__DB_PATH": graph_path,
        "AGENT_MEMORY_LOG_FILE": f"{db_path.rsplit('/', 1)[0]}/server.log",
        "RUST_LOG": log_level,
    },
    "timeout": 30000,
    "disabled": False,
}

with open(path, "w") as f:
    json.dump(cfg, f, indent=2, ensure_ascii=False)

print(f"  已写入 MCP server: {name}")
print(f"  command: {wrapper}")
print(f"  graph:   enabled -> {graph_path}")
print(f"  log:     RUST_LOG={log_level}")
PYEOF

# =============================================================================
# 4. 检测 WorkBuddy 运行状态
# =============================================================================
step "4/5  检测 WorkBuddy 状态"
WB_PID=$(pgrep -f "WorkBuddy" | head -1 || true)
if [[ -n "$WB_PID" ]]; then
    warn "WorkBuddy 正在运行 (PID $WB_PID)。MCP 配置变更需要重启 WorkBuddy 才能加载。"
    echo -n "  是否现在自动重启 WorkBuddy？[y/N] "
    read -r ans
    if [[ "$ans" =~ ^[yY]$ ]]; then
        info "正在退出 WorkBuddy..."
        osascript -e 'tell application "WorkBuddy" to quit' 2>/dev/null || true
        sleep 3
        # 兜底强杀
        pkill -f "WorkBuddy" 2>/dev/null || true
        sleep 1
        info "正在启动 WorkBuddy..."
        open -a "WorkBuddy"
        info "WorkBuddy 已启动，等待 MCP 服务器被拉起..."
    else
        warn "请手动重启 WorkBuddy 后，日志才会开始输出。"
    fi
else
    info "WorkBuddy 未运行，正在启动..."
    open -a "WorkBuddy"
    info "WorkBuddy 已启动，等待 MCP 服务器被拉起..."
fi

# =============================================================================
# 5. tail 日志
# =============================================================================
step "5/5  实时记忆读写日志（Ctrl+C 退出）"
echo -e "  ${CYAN}日志文件:${RESET} $LOG_FILE"
echo -e "  ${CYAN}数据目录:${RESET} $DATA_DIR"
echo -e "  ${CYAN}过滤提示:${RESET} 日志含 memory::store / memory::search / memory::update / memory::delete / graph::* 等 target"
echo "  ----------------------------------------------------------------------"

# 如果日志文件不存在先创建
touch "$LOG_FILE"

# 清理：Ctrl+C 时不删配置，只退出 tail
cleanup() {
    echo -e "\n${YELLOW}已退出日志观察。MCP 配置仍保留，WorkBuddy 重启后会继续加载。${RESET}"
    echo -e "  移除配置: ${CYAN}$0 --remove${RESET}"
    exit 0
}
trap cleanup INT TERM

tail -n 0 -f "$LOG_FILE"
