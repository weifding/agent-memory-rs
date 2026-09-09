//! 索引修复效果对照实验（临时，非交付物）
//!
//! 运行: cargo run --release --example bench_index_fix
//! 验证：为 list 查询补 (namespace, created_at) 复合索引后，50k 数据下的提升

use agent_memory_server::models::Memory;
use agent_memory_server::storage::SQLiteStorage;
use rusqlite::Connection;
use std::time::Instant;

fn time(label: &str, iters: u32, f: impl Fn()) {
    let start = Instant::now();
    for _ in 0..iters {
        f();
    }
    let d = start.elapsed();
    println!(
        "  {label:<46} {:>8.3} ms/次  ({:>9.0}/s)",
        d.as_secs_f64() * 1000.0 / iters as f64,
        iters as f64 / d.as_secs_f64()
    );
}

fn plan(conn: &Connection, sql: &str) {
    let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {}", sql)).unwrap();
    for r in stmt
        .query_map([], |r| r.get::<_, String>(3))
        .unwrap()
        .flatten()
    {
        println!("      {r}");
    }
}

fn main() -> anyhow::Result<()> {
    let db_path = std::env::temp_dir().join("bench_idx_fix.db");
    let _ = std::fs::remove_file(&db_path);

    let storage = SQLiteStorage::open(&db_path, 128)?;
    let n = 50_000usize;
    for i in 0..n {
        let mem = Memory::new(
            format!("测试记忆 {} 医疗信息化 医保接口 HIS 系统 温州医院 {}", i, i),
            "default".to_string(),
            "fact".to_string(),
        );
        storage.store(&mem, None)?;
    }
    drop(storage); // 关闭 storage 连接，避免与实验连接互锁
    println!("== 已插入 {n} 条，开始索引对照 ==");

    let conn = Connection::open(&db_path)?;
    let sql = "SELECT * FROM memories WHERE namespace = 'default' ORDER BY created_at DESC LIMIT 20 OFFSET 0";
    let sql_off5k = "SELECT * FROM memories WHERE namespace = 'default' ORDER BY created_at DESC LIMIT 20 OFFSET 5000";

    println!("[基线 - 无索引]");
    plan(&conn, sql);
    time("list(20) 无索引", 200, || {
        let _ = conn.query_row(&sql, [], |r| r.get::<_, i64>(0)).unwrap_or(0);
    });
    time("list(20, offset 5000) 无索引", 100, || {
        let _ = conn.query_row(&sql_off5k, [], |r| r.get::<_, i64>(0)).unwrap_or(0);
    });

    println!("[方案1 - 仅 created_at 索引]");
    conn.execute_batch("CREATE INDEX idx_created_at ON memories(created_at)")?;
    plan(&conn, sql);
    time("list(20) created_at 索引", 200, || {
        let _ = conn.query_row(&sql, [], |r| r.get::<_, i64>(0)).unwrap_or(0);
    });

    println!("[方案2 - (namespace, created_at) 复合索引]");
    conn.execute_batch("DROP INDEX idx_created_at; CREATE INDEX idx_ns_created ON memories(namespace, created_at)")?;
    plan(&conn, sql);
    time("list(20) 复合索引", 200, || {
        let _ = conn.query_row(&sql, [], |r| r.get::<_, i64>(0)).unwrap_or(0);
    });
    time("list(20, offset 5000) 复合索引", 100, || {
        let _ = conn.query_row(&sql_off5k, [], |r| r.get::<_, i64>(0)).unwrap_or(0);
    });

    let _ = std::fs::remove_file(&db_path);
    Ok(())
}
