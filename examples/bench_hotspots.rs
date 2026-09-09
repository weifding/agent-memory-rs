//! 性能热点探测（临时基准，用于定位瓶颈，非项目交付物）
//!
//! 运行: cargo run --release --example bench_hotspots
//!
//! 验证三个假设：
//!   A. list() 的 ORDER BY created_at DESC 无索引 → 全表读取+排序，随 N 线性/超线性增长
//!   B. search() 全量向量余弦 O(N)，随 N 线性增长且内存占用增长
//!   C. get_stats() 的 consolidated=0 计数无索引 → 全表扫描

use agent_memory_server::models::Memory;
use agent_memory_server::storage::SQLiteStorage;
use rusqlite::Connection;
use std::time::Instant;

fn time<F: FnMut()>(label: &str, iters: u32, mut f: F) {
    let start = Instant::now();
    for _ in 0..iters {
        f();
    }
    let d = start.elapsed();
    println!(
        "  {label:<42} {:>8.3} ms/次  ({:>9.0}/s)",
        d.as_secs_f64() * 1000.0 / iters as f64,
        iters as f64 / d.as_secs_f64()
    );
}

fn explain(db_path: &std::path::Path, label: &str, sql: &str) {
    let conn = Connection::open(db_path).expect("open for explain");
    let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {}", sql)).unwrap();
    let rows: Vec<(i32, i32, i32, String)> = stmt
        .query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    println!("  [查询计划] {label}");
    for (a, b, c, detail) in rows {
        println!("    {a} {b} {c}  {detail}");
    }
}

fn run_size(n: usize) -> anyhow::Result<()> {
    let db_path = std::env::temp_dir().join(format!("bench_hot_{n}.db"));
    let _ = std::fs::remove_file(&db_path);

    let storage = SQLiteStorage::open(&db_path, 128)?;
    let start = Instant::now();
    for i in 0..n {
        let content = format!(
            "测试记忆 {} 内容包含 医疗信息化 医保接口 HIS 系统 温州医院 关键词 {}",
            i, i
        );
        let mem = Memory::new(content, "default".to_string(), "fact".to_string());
        storage.store(&mem, None)?;
    }
    println!(
        "== N={} 批量插入: {:.0} 条/s ({:.2}s)",
        n,
        n as f64 / start.elapsed().as_secs_f64(),
        start.elapsed().as_secs_f64()
    );

    // 查询计划证据
    explain(
        &db_path,
        "list 查询 (ORDER BY created_at DESC)",
        "SELECT * FROM memories WHERE namespace = 'default' ORDER BY created_at DESC LIMIT 20 OFFSET 0",
    );
    explain(
        &db_path,
        "unconsolidated 计数 (consolidated = 0)",
        "SELECT COUNT(*) FROM memories WHERE consolidated = 0",
    );
    explain(
        &db_path,
        "按 namespace 计数",
        "SELECT COUNT(*) FROM memories WHERE namespace = 'default'",
    );

    // 测量
    time(&format!("list(20, offset 0) N={}", n), 500, || {
        let _ = storage.list("default", 20, 0, None);
    });
    time(&format!("list(20, offset 5000) N={}", n), 200, || {
        let _ = storage.list("default", 20, 5000, None);
    });
    time(&format!("get_stats(全局) N={}", n), 300, || {
        let _ = storage.get_stats(None);
    });
    time(&format!("get_stats(default) N={}", n), 300, || {
        let _ = storage.get_stats(Some("default"));
    });
    time(&format!("get(单条) N={}", n), 2000, || {
        let _ = storage.get("nonexistent-id");
    });

    // 向量检索：固定 2000 条带向量，测搜索扩展性
    let all = storage.list("default", 2000, 0, None)?;
    let q: Vec<f32> = (0..128).map(|i| (i as f32 * 0.01).sin()).collect();
    for (i, m) in all.iter().enumerate() {
        let emb: Vec<f32> = (0..128).map(|j| ((i + j) as f32 * 0.01).cos()).collect();
        storage.update_embedding(&m.id, &emb)?;
    }
    time(&format!("search(top10, 2000 向量) N={}", n), 200, || {
        let _ = storage.search(&q, None, 10, None);
    });

    let _ = std::fs::remove_file(&db_path);
    Ok(())
}

fn main() -> anyhow::Result<()> {
    for n in [10_000usize, 50_000] {
        run_size(n)?;
    }
    Ok(())
}
