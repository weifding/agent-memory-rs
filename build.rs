//! 构建期平台适配开关 —— 针对 graph feature（Kùzu）在各操作系统上做工具链前置检查，
//! 在编译最早期给出可操作的告警，避免直接掉进 cmake 深处的报错。
//!
//! 平台差异（与 scripts/build.sh、scripts/build.ps1 的约定保持一致）：
//! - macOS：Apple clang 可直接编译 kuzu 0.11.x，需 Xcode Command Line Tools
//! - Linux：kuzu 的 C++ 依赖用 AVX-512 FP16，需 GCC>=12 或 clang>=16
//! - Windows：MSVC（VS Build Tools 2022，含 C++ 与 CMake），由 build.ps1 引导
//!
//! 纯 Rust 主体（无 graph feature）无平台特殊要求，本脚本直接放行。

use std::process::Command;

fn have(cmd: &str, args: &[&str]) -> bool {
    Command::new(cmd)
        .args(args)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn warn(msg: &str) {
    println!("cargo:warning=[agent-memory-rs] {msg}");
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    // 仅 graph feature（kuzu 原生构建）需要平台开关检查
    if std::env::var("CARGO_FEATURE_GRAPH").is_err() {
        return;
    }
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();

    // kuzu 原生构建依赖 CMake（三平台通用）
    if !have("cmake", &["--version"]) {
        warn("未检测到 cmake，kuzu 原生库编译会失败。安装：macOS `brew install cmake` / Windows 随 VS Build Tools 或 winget install Kitware.CMake / Linux `apt install cmake`");
    }

    match os.as_str() {
        "macos" => {
            let clt = Command::new("xcode-select")
                .arg("-p")
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
            if !clt {
                warn("macOS 编译 kuzu 需要 Xcode Command Line Tools：xcode-select --install");
            } else {
                println!("cargo:warning=[agent-memory-rs] macOS({arch})：Apple clang 编译 kuzu，无需 GCC 12");
            }
        }
        "linux" => {
            let gcc_ok = || -> Option<u32> {
                let out = Command::new("gcc").arg("-dumpversion").output().ok()?;
                let major = String::from_utf8_lossy(&out.stdout)
                    .split('.')
                    .next()?
                    .trim()
                    .parse()
                    .ok()?;
                Some(major)
            }();
            match gcc_ok {
                Some(major) if major >= 12 => {}
                Some(major) => warn(&format!(
                    "Linux GCC {major} < 12，kuzu 的 C++ 依赖会编译失败。Debian/Ubuntu: apt install gcc-12 g++-12 并导出 CC=gcc-12 CXX=g++-12；或 clang>=16"
                )),
                None => {
                    if !have("clang", &["--version"]) {
                        warn("Linux 未找到 gcc/clang，kuzu 无法编译");
                    } else if !have("bash", &["-c", "clang --version | grep -qE 'version (1[6-9]|[2-9][0-9])'"]) {
                        warn("Linux clang < 16，kuzu 需要 clang>=16 或 GCC>=12");
                    }
                }
            }
        }
        "windows" => {
            let msvc_env = std::env::var("VCINSTALLDIR").is_ok()
                || std::env::var("VCToolsInstallDir").is_ok();
            if !msvc_env {
                warn("Windows 编译 kuzu 需 MSVC：安装 VS Build Tools 2022（C++ 桌面开发 + CMake），建议在 x64 Native Tools Prompt 中运行，或使用 scripts/build.ps1");
            }
        }
        other => {
            warn(&format!("平台 {other} 未做 kuzu 构建验证，graph feature 可能不可用（--no-graph 可正常构建纯记忆版）"));
        }
    }
}
