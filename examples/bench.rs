//! 性能基准测试 — Rust 版本
//!
//! 运行: cargo run --release --example bench

use agent_memory_server::models::Memory;
use agent_memory_server::storage::SQLiteStorage;
use std::time::Instant;

fn main() -> anyhow::Result<()> {
    let db_path = std::env::temp_dir().join("bench_rs_memory.db");
    let _ = std::fs::remove_file(&db_path);

    let storage = SQLiteStorage::open(&db_path, 128)?;
    let n = 10_000;

    // 1. 批量插入
    let start = Instant::now();
    for i in 0..n {
        let content = format!(
            "这是第 {} 条测试记忆，内容包含医疗信息化、医保接口、HIS系统、温州医院等关键词 {}",
            i, i
        );
        let mem = Memory::new(content, "default".to_string(), "fact".to_string());
        storage.store(&mem, None)?;
    }
    let insert_elapsed = start.elapsed();
    println!(
        "插入 {} 条: {:.2?} ({:.0} 条/秒)",
        n,
        insert_elapsed,
        n as f64 / insert_elapsed.as_secs_f64()
    );

    // 2. 单条查询（get）
    let mem = storage.list("default", 1, 0, None)?;
    let test_id = mem[0].id.clone();
    let start = Instant::now();
    let iterations = 1000;
    for _ in 0..iterations {
        storage.get(&test_id)?;
    }
    let get_elapsed = start.elapsed();
    println!(
        "单条查询 x{}: {:.2?} ({:.0} 查询/秒, {:.3} ms/次)",
        iterations,
        get_elapsed,
        iterations as f64 / get_elapsed.as_secs_f64(),
        get_elapsed.as_secs_f64() * 1000.0 / iterations as f64
    );

    // 3. 列表查询（list 20条）
    let start = Instant::now();
    let iterations = 1000;
    for _ in 0..iterations {
        storage.list("default", 20, 0, None)?;
    }
    let list_elapsed = start.elapsed();
    println!(
        "列表查询(20条) x{}: {:.2?} ({:.0} 查询/秒, {:.3} ms/次)",
        iterations,
        list_elapsed,
        iterations as f64 / list_elapsed.as_secs_f64(),
        list_elapsed.as_secs_f64() * 1000.0 / iterations as f64
    );

    // 4. 统计查询
    let start = Instant::now();
    let iterations = 1000;
    for _ in 0..iterations {
        storage.get_stats(None)?;
    }
    let stats_elapsed = start.elapsed();
    println!(
        "统计查询 x{}: {:.2?} ({:.0} 查询/秒, {:.3} ms/次)",
        iterations,
        stats_elapsed,
        iterations as f64 / stats_elapsed.as_secs_f64(),
        stats_elapsed.as_secs_f64() * 1000.0 / iterations as f64
    );

    // 5. 向量检索（内存余弦相似度，128维）
    let query_embedding: Vec<f32> = (0..128).map(|i| (i as f32 * 0.01).sin()).collect();
    // 先给部分记忆加向量
    let all = storage.list("default", 2000, 0, None)?;
    for (i, m) in all.iter().enumerate() {
        let emb: Vec<f32> = (0..128).map(|j| ((i + j) as f32 * 0.01).cos()).collect();
        storage.update_embedding(&m.id, &emb)?;
    }

    let start = Instant::now();
    let iterations = 500;
    for _ in 0..iterations {
        storage.search(&query_embedding, None, 10, None)?;
    }
    let search_elapsed = start.elapsed();
    println!(
        "向量检索(top10, 2000条) x{}: {:.2?} ({:.0} 查询/秒, {:.3} ms/次)",
        iterations,
        search_elapsed,
        iterations as f64 / search_elapsed.as_secs_f64(),
        search_elapsed.as_secs_f64() * 1000.0 / iterations as f64
    );

    let _ = std::fs::remove_file(&db_path);
    Ok(())
}
