# OwnedRows：main 分支 fork 扩展

## 基线与范围

- 官方仓库：`https://github.com/duckdb/duckdb-rs`。
- main 移植基线：`4014db76fd09d542ca321cc7396de6e63d79be47`。
- crate 版本 `1.20000.0`，Arrow 60；固定引擎 `2.0.0-alpha43678` / `d591bb1da2de3cb12329c75aff2c31bebe922b59`。
- 本次在 fork 的 `main` 为传统 `duckdb` crate 增加 `Connection::query_owned`、`OwnedRows`、`OwnedRow`；不修改 `duckdb-neo`。
- 这是 fork API，沿用官方版本号、绑定、依赖及引擎 pin，不发布到 crates.io。
- 旧的 `codex/owned-rows-v1.10505.0` 分支和 `v1.10505.0-0930.1` 标签保留，供 DuckDB 1.5.5 使用方继续使用。
- NimbusDB 的 `2.0` 实验分支接入本 main 扩展；不因此修改 NimbusDB master 的依赖或实施会话管理改造。

## API 与所有权

```rust
use duckdb::{Connection, Result, types::ValueRef};

fn example() -> Result<()> {
    let conn = Connection::open_in_memory()?;
    let mut rows = conn.query_owned("SELECT ?::BIGINT AS id", [42i64])?;
    assert_eq!(rows.column_name(0)?, "id");
    let row = rows.next()?.unwrap();
    assert_eq!(row.get_ref(0)?, ValueRef::BigInt(42));
    assert!(rows.next()?.is_none());
    Ok(())
}
```

- `Connection::query_owned(self, sql, params)` 消费连接，包括失败路径。
  完成一次原生执行后，从 statement 转移已有的 `ExecutedResult`，释放 statement，
  返回独立 owner；不延长借用生命周期，不使用自引用或泄漏。
- 如需保留调用方连接，可先 `try_clone()`；克隆是独立会话，不能假定事务、临时表
  或所有 session 配置会被复制。消费原连接则保持在该连接上执行。
- 与已有 `prepare/execute` 一样处理 SQL 和参数，不新增只读限制或游标协议。
  调用方仍负责 SQL 权限与单结果集限制；多语句 SQL 继承原有行为。
- `OwnedRows` 保存当前 Arrow chunk、原生结果、连接和位置；字段顺序确保 chunk
  先于 result、result 先于 connection 释放。原有 Statement/Rows API 不变。
- `OwnedRows` 自动继承现有成员的 `Send`，不是 `Sync`；本扩展不增加 unsafe 实现。
  顺序移交线程不需要驻留工作线程，不能并发读取同一 owner。
- `next()` 返回借用的 `OwnedRow`；`get_ref()` 返回借用值。借用仍在使用时不能
  推进或释放 owner。复用普通 Row 的解码与原生逻辑类型修正，不维护第二套类型映射。
- 原生 fetch/Arrow 转换错误通过 Result 返回，错误后清空当前 chunk 并停止迭代；
  耗尽后重复 next 返回 None。元数据、原生结果和连接保留至 owner drop。
  类型解码继承上游 Row 的支持边界，不承诺消除上游所有 panic。

## 收益与限制

目标是让 NimbusDB 可直接保留原生结果供多次 FETCH 使用，不必再完整转存自定义
临时文件，也不需要完整 Rust 行集。此 fork 本身不实现 MySQL FETCH 或 async 调度。
查询先完成物化再返回，不保证降低引擎查询内存，不保证引擎内部不使用临时文件。
资源数量、内存预算、超时和取消策略由使用方负责。取消接收方不能自动中断正在运行
的同步查询；工作完成后必须丢弃无人接收的 owner 以回收资源。

## 验证入口

main 默认按 `.duckdb-release` 下载匹配引擎，不能用其他 alpha 或 1.5.5 库替代后声称该版本兼容：

```sh
cargo test --locked -p duckdb --features chrono,rust_decimal,uuid,r2d2 --lib owned_rows
cargo test --locked -p duckdb --features chrono,rust_decimal,uuid,r2d2
cargo clippy --locked -p duckdb --all-targets --features chrono,rust_decimal,uuid,r2d2 -- -D warnings
cargo build --locked --workspace
cargo fmt --all -- --check
git diff --check
```

回归覆盖新旧结果的元数据和值一致性、NULL/DECIMAL/UHUGEINT/时间单位、跨 chunk
顺序读取、多结果并存、跨线程移动、一次执行与结果快照、临时表会话状态、DML RETURNING
只执行一次、错误后停止读取，以及提前关闭/失败/无人接收结果后的连接释放。
编译失败示例约束非 Sync 与行借用；fetch 错误注入不等于全部原生故障覆盖，
InterruptHandle 的 Weak 检查不代替原生内存泄漏检测。

### 2026-10-15 main 移植验证

- macOS aarch64，Rust 1.98.1，linked 模式使用 pin 指定的 `2.0.0-alpha43678` 官方预编译库。
- OwnedRows 专项 11 项；启用 `chrono,rust_decimal,uuid,r2d2` 的库测试 353/353，集成测试 3 通过、2 忽略。
- 文档测试 55 通过、7 忽略，包括非 Sync 与借用期间不能推进结果的编译失败示例。
- 整个 workspace 构建通过，包括 duckdb-neo。crate 版本、Cargo.lock、绑定和原生引擎 pin 未改动。
- all-targets 严格 Clippy（`-D warnings`）、fmt 与 diff 检查通过。
- NimbusDB 联调发现无类型 NULL 导出为 Arrow NullArray，其物理 null bitmap 为空；公共 Row 解码器补齐 DataType::Null，避免 panic。普通 Rows 与 OwnedRows 均覆盖无类型/有类型 NULL、嵌套 NULL 列表及跨 chunk 读取。
- NimbusDB 的 bundled-cmake / MySQL wire 验证单独记录在其 `docs/DUCKDB_2_0_UPGRADE.md`，不与本项 linked 验证混用。

## 跟随官方版本的移植流程

1. 核对准备采用的官方版本、完整 SHA 和引擎 pin；main 开发版与发布标签分别维护。
2. 提取本扩展补丁，检查上游是否已有等价 API；满足需求时优先复用上游。
3. 重新审查 ExecutedResult、RawStatement、连接和 Arrow chunk 的所有权、Drop 顺序及 Send/Sync；不能只凭补丁无冲突判断兼容。
4. 完成 Rust 回归，再验证使用方实际 bundled-cmake 构建与真实协议路径；单纯 linked 构建不证明源码后端可用。
5. 保留不可变的旧发布标签。使用方用 Cargo.lock 锁定 main 提交，升级时重新验证，不依赖本机 patch。

## 历史版本

`v1.10505.0-0930.1` 基于官方 `v1.10505.0` / `88f673c5cdf09e287c010a910db53b97e523a9fa`。
其 DuckDB 1.5.5 构建及历史 JDBC 验证不能视作本次 2.0 alpha 的验证结果。
