#![cfg(feature = "direct-appender")]
use duckdb::{Connection, Result, params, types::Value};

#[test]
fn direct_appender_logical_types_defaults_and_selected_columns() -> Result<()> {
    let c = Connection::open_in_memory()?;
    c.execute_batch("CREATE TABLE t (id INT, n DECIMAL(18,2), s VARCHAR DEFAULT 'default', b BLOB, g INT GENERATED ALWAYS AS (id*2))")?;
    let mut a = c.direct_appender_to_db("t", "main")?;
    a.append_row(params![1, 1234, "中\0文😀", vec![0_u8, 255]])?;
    a.append_row(params![
        2,
        Value::Decimal(duckdb::types::Decimal::new(4, 2, 1234).unwrap()),
        "next",
        vec![1_u8]
    ])?;
    a.flush()?;
    a.add_column("n")?;
    a.add_column("id")?;
    a.append_row(params![
        Value::Decimal(duckdb::types::Decimal::new(2, 2, -1).unwrap()),
        3
    ])?;
    a.flush()?;
    a.clear_columns()?;
    a.append_row(params![4, 5, "clear", Vec::<u8>::new()])?;
    a.flush()?;
    drop(a);
    let row: (String, String, i32) = c.query_row("SELECT n::VARCHAR,s,g FROM t WHERE id=1", [], |r| {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?))
    })?;
    assert_eq!(row, ("1234.00".into(), "中\0文😀".into(), 2));
    assert_eq!(
        c.query_row("SELECT n::VARCHAR FROM t WHERE id=2", [], |r| r.get::<_, String>(0))?,
        "12.34"
    );
    assert_eq!(
        c.query_row("SELECT s FROM t WHERE id=3", [], |r| r.get::<_, String>(0))?,
        "default"
    );
    assert_eq!(
        c.query_row("SELECT n::VARCHAR FROM t WHERE id=3", [], |r| r.get::<_, String>(0))?,
        "-0.01"
    );
    Ok(())
}

#[test]
fn direct_appender_flush_and_checkpoint_share_transaction() -> Result<()> {
    let c = Connection::open_in_memory()?;
    let observer = c.try_clone()?;
    c.execute_batch("CREATE TABLE t(i INT); CREATE TABLE cp(i INT); INSERT INTO cp VALUES(0)")?;
    for commit in [false, true] {
        c.execute_batch("BEGIN")?;
        let mut a = c.direct_appender_to_db("t", "main")?;
        for i in 0..5000 {
            a.append_row([i])?;
        }
        a.flush()?;
        drop(a);
        c.execute_batch("UPDATE cp SET i=5000")?;
        assert_eq!(
            observer.query_row("SELECT count(*) FROM t", [], |r| r.get::<_, i64>(0))?,
            0
        );
        c.execute_batch(if commit { "COMMIT" } else { "ROLLBACK" })?;
        let expected = if commit { 5000 } else { 0 };
        assert_eq!(
            observer.query_row("SELECT count(*) FROM t", [], |r| r.get::<_, i64>(0))?,
            expected
        );
        assert_eq!(
            observer.query_row("SELECT i FROM cp", [], |r| r.get::<_, i64>(0))?,
            expected
        );
    }
    Ok(())
}

#[test]
fn direct_appender_constraint_error_rolls_back_and_connection_recovers() -> Result<()> {
    let c = Connection::open_in_memory()?;
    c.execute_batch("CREATE TABLE t(i INT PRIMARY KEY, n INT NOT NULL CHECK(n>0))")?;
    for sql_values in [vec![(1, Some(1)), (1, Some(2))], vec![(2, None)], vec![(3, Some(-1))]] {
        c.execute_batch("BEGIN")?;
        let mut a = c.direct_appender_to_db("t", "main")?;
        for (id, n) in sql_values {
            a.append_row(params![id, n])?;
        }
        assert!(a.flush().is_err());
        drop(a);
        c.execute_batch("ROLLBACK")?;
        assert_eq!(c.query_row("SELECT count(*) FROM t", [], |r| r.get::<_, i64>(0))?, 0);
    }
    c.execute_batch("INSERT INTO t VALUES(9,9)")?;
    Ok(())
}

#[test]
fn direct_appender_schema_change_is_rejected() -> Result<()> {
    let c = Connection::open_in_memory()?;
    c.execute_batch("CREATE TABLE t(i INT)")?;
    let mut a = c.direct_appender_to_db("t", "main")?;
    a.append_row([1])?;
    c.execute_batch("ALTER TABLE t ADD n INT")?;
    assert!(a.flush().is_err());
    drop(a);
    assert_eq!(c.query_row("SELECT count(*) FROM t", [], |r| r.get::<_, i64>(0))?, 0);
    Ok(())
}

#[test]
fn direct_appender_drop_flushes_and_quoted_names_are_literal() -> Result<()> {
    let c = Connection::open_in_memory()?;
    c.execute_batch("CREATE SCHEMA \"s\"\"中\"; CREATE TABLE \"s\"\"中\".\"t\"\";--\"(i INT)")?;
    {
        let mut a = c.direct_appender_to_db("t\";--", "s\"中")?;
        a.append_row([42])?;
    }
    assert_eq!(
        c.query_row("SELECT i FROM \"s\"\"中\".\"t\"\";--\"", [], |r| r.get::<_, i32>(0))?,
        42
    );
    assert!(c.direct_appender_to_db("missing", "main").is_err());
    Ok(())
}

#[test]
#[ignore = "manual comparative benchmark; no timing assertion"]
fn direct_appender_comparative_benchmark() -> Result<()> {
    // Alternate order to reduce the effect of one-time caches; compare equal data and flush boundaries.
    for pass in 0..4 {
        for direct in if pass % 2 == 0 { [false, true] } else { [true, false] } {
            let c = Connection::open_in_memory()?;
            c.execute_batch("CREATE TABLE t(i BIGINT, s VARCHAR)")?;
            let start = std::time::Instant::now();
            for chunk in 0..50 {
                let mut a = if direct {
                    c.direct_appender_to_db("t", "main")?
                } else {
                    c.appender_to_db("t", "main")?
                };
                for i in 0..2000 {
                    a.append_row(params![chunk * 2000 + i, "payload中文"])?;
                }
                a.flush()?;
            }
            let elapsed = start.elapsed();
            assert_eq!(
                c.query_row("SELECT count(*) FROM t", [], |r| r.get::<_, i64>(0))?,
                100000
            );
            println!("pass={pass} direct={direct} elapsed_ms={}", elapsed.as_millis());
        }
    }
    Ok(())
}

#[test]
fn direct_appender_wide_decimal_temporal_blob_match_standard() -> Result<()> {
    let c = Connection::open_in_memory()?;
    for table in ["ordinary", "direct"] {
        c.execute_batch(&format!(
            "CREATE TABLE {table}(n DECIMAL(38,10), ts TIMESTAMP, d DATE, b BLOB, s VARCHAR)"
        ))?;
        let mut a = if table == "direct" {
            c.direct_appender_to_db(table, "main")?
        } else {
            c.appender_to_db(table, "main")?
        };
        a.append_row(params![
            Value::Decimal(duckdb::types::Decimal::new(38, 10, 12345678901234567890123456781234567890_i128).unwrap()),
            "2024-01-02 03:04:05.123456",
            "2024-02-29",
            vec![0_u8, 255, 128],
            "中\0😀"
        ])?;
        a.flush()?;
    }
    assert_eq!(c.query_row("SELECT count(*) FROM ((SELECT * FROM ordinary EXCEPT ALL SELECT * FROM direct) UNION ALL (SELECT * FROM direct EXCEPT ALL SELECT * FROM ordinary))",[],|r|r.get::<_,i64>(0))?,0);
    Ok(())
}

#[test]
fn direct_appender_foreign_key_and_read_only_transaction_are_enforced() -> Result<()> {
    let c = Connection::open_in_memory()?;
    c.execute_batch("CREATE TABLE parent(i INT PRIMARY KEY); CREATE TABLE child(i INT REFERENCES parent(i)); INSERT INTO parent VALUES(1)")?;
    c.execute_batch("BEGIN")?;
    let mut a = c.direct_appender_to_db("child", "main")?;
    a.append_row([99])?;
    assert!(a.flush().is_err());
    drop(a);
    c.execute_batch("ROLLBACK; BEGIN TRANSACTION READ ONLY")?;
    let mut a = c.direct_appender_to_db("child", "main")?;
    a.append_row([1])?;
    assert!(a.flush().is_err());
    drop(a);
    c.execute_batch("ROLLBACK")?;
    assert_eq!(
        c.query_row("SELECT count(*) FROM child", [], |r| r.get::<_, i64>(0))?,
        0
    );
    Ok(())
}

#[test]
fn direct_appender_automatic_flush_still_rolls_back_whole_transaction() -> Result<()> {
    let c = Connection::open_in_memory()?;
    let observer = c.try_clone()?;
    c.execute_batch("CREATE TABLE t(i INT CHECK(i>=0)); BEGIN")?;
    let mut a = c.direct_appender_to_db("t", "main")?;
    // Cross the default 100-vector flush watermark before the final failing row.
    for i in 0..210000 {
        a.append_row([i])?;
    }
    a.append_row([-1])?;
    assert!(a.flush().is_err());
    drop(a);
    c.execute_batch("ROLLBACK")?;
    assert_eq!(
        observer.query_row("SELECT count(*) FROM t", [], |r| r.get::<_, i64>(0))?,
        0
    );
    Ok(())
}

#[test]
fn direct_appender_selected_columns_evaluate_dynamic_defaults() -> Result<()> {
    let c = Connection::open_in_memory()?;
    c.execute_batch("CREATE SEQUENCE seq; CREATE TABLE t(i INT, ts TIMESTAMP DEFAULT CURRENT_TIMESTAMP, n BIGINT DEFAULT nextval('seq'))")?;
    let mut a = c.direct_appender_to_db("t", "main")?;
    a.add_column("i")?;
    a.append_row([1])?;
    a.append_row([2])?;
    a.flush()?;
    drop(a);
    assert_eq!(
        c.query_row("SELECT count(*) FROM t WHERE ts IS NOT NULL", [], |r| r
            .get::<_, i64>(0))?,
        2
    );
    assert_eq!(c.query_row("SELECT sum(n) FROM t", [], |r| r.get::<_, i64>(0))?, 3);
    Ok(())
}

#[test]
#[ignore = "manual independent-process resource measurement; no timing or RSS assertion"]
fn direct_appender_resource_workload() -> Result<()> {
    let direct = match std::env::var("DIRECT_APPENDER_PROFILE_MODE").as_deref() {
        Ok("direct") => true,
        Ok("standard") => false,
        _ => panic!("set DIRECT_APPENDER_PROFILE_MODE=direct or standard"),
    };
    let batch: i64 = std::env::var("DIRECT_APPENDER_PROFILE_BATCH")
        .unwrap_or("1000".into())
        .parse()
        .unwrap();
    assert!(batch > 0);
    let rows = 500000_i64;
    let c = Connection::open_in_memory()?;
    c.execute_batch("SET threads=1; SET memory_limit='512MB'; CREATE TABLE t(i BIGINT,s VARCHAR,n DECIMAL(18,2))")?;
    let payload = "中文😀payload".repeat(8);
    let decimal = duckdb::types::Decimal::new(18, 2, 1234).unwrap();
    for start in (0..rows).step_by(batch as usize) {
        c.execute_batch("BEGIN")?;
        let mut a = if direct {
            c.direct_appender_to_db("t", "main")?
        } else {
            c.appender_to_db("t", "main")?
        };
        for i in start..(start + batch).min(rows) {
            a.append_row(params![i, payload, Value::Decimal(decimal)])?;
        }
        a.flush()?;
        drop(a);
        c.execute_batch("COMMIT")?;
    }
    let (count, sum): (i64, i64) = c.query_row("SELECT count(*),sum(i)::BIGINT FROM t", [], |r| {
        Ok((r.get(0)?, r.get(1)?))
    })?;
    assert_eq!((count, sum), (rows, rows * (rows - 1) / 2));
    println!("mode={direct} rows={rows} batch={batch}");
    Ok(())
}
