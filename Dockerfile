# =============================================================================
# agent-memory-server Docker 镜像（Podman / Docker 通用）
# 多阶段构建：builder 编译，runtime 仅含二进制，镜像约 50MB
# 不含 graph feature（纯 Rust，无 C++ 工具链依赖）
# =============================================================================
FROM rust:1-bookworm AS builder

WORKDIR /build

# 先拷贝依赖清单，单独编译依赖层（利用 Docker 缓存）
COPY Cargo.toml Cargo.lock* ./
COPY vendor/ vendor/
# 创建 stub src 让 cargo 解析依赖（后续覆盖真实源码）
RUN mkdir -p src && echo 'fn main() {}' > src/main.rs \
    && cargo build --release --bin agent-memory-server 2>/dev/null || true

# 拷贝完整源码，正式编译
COPY src/ src/
COPY build.rs build.rs
RUN touch src/main.rs && cargo build --release --bin agent-memory-server

# ---- Runtime stage ----
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# 创建非 root 用户
RUN groupadd -r agentmem && useradd -r -g agentmem -d /data agentmem

COPY --from=builder /build/target/release/agent-memory-server /usr/local/bin/agent-memory-server

# 数据目录（SQLite 数据库）+ 打包本地数据
RUN mkdir -p /data /etc/agent-memory
COPY data/ /data/
RUN chown -R agentmem:agentmem /data

# 默认配置（可被环境变量或挂载配置文件覆盖）
COPY deployment/config.yaml /etc/agent-memory/config.yaml

USER agentmem
WORKDIR /data

# 环境变量默认值：HTTP 传输，绑定 0.0.0.0 供容器外访问
ENV AGENT_MEMORY_SERVER__TRANSPORT=http \
    AGENT_MEMORY_SERVER__HTTP_HOST=0.0.0.0 \
    AGENT_MEMORY_SERVER__HTTP_PORT=8888 \
    RUST_LOG=INFO

EXPOSE 8888

VOLUME ["/data"]

ENTRYPOINT ["agent-memory-server", "--config", "/etc/agent-memory/config.yaml"]
