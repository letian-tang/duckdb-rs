# OwnedRows：版本化 fork 扩展

## 基线与范围

- 官方仓库：`https://github.com/duckdb/duckdb-rs`。
- 官方基线：`v1.10505.0`，提交 `88f673c5cdf09e287c010a910db53b97e523a9fa`。
- 对应引擎：DuckDB 1.5.5；沿用基线的源码、绑定、依赖及版本号。
- 当前工作分支：`codex/owned-rows-v1.10505.0`，不是 main 的 2.0 开发分支。
- `OwnedRows` 是本 fork 增加的 API，不是官方同名版本已经发布的功能。
- fork 发布标签：`v1.10505.0-0930.1`（09 月 30 日第 1 次发布）。
  沿用官方 crate 版本号，只增加 fork 标签，不覆盖官方标签，不发布到 crates.io。
  由本 fork 自行维护，不向上游提交 PR；本轮不修改 NimbusDB 正式依赖。

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

按仓库现有下载机制使用对应版本的官方库，而不是本机 2.0 库：

```sh
DUCKDB_DOWNLOAD_LIB=1 cargo test --locked -p duckdb --lib --no-default-features
DUCKDB_DOWNLOAD_LIB=1 cargo test --locked -p duckdb --doc --no-default-features
DUCKDB_DOWNLOAD_LIB=1 cargo clippy --locked -p duckdb --all-targets --no-default-features -- -D warnings
cargo fmt --all -- --check
git diff --check
```

也可按 CONTRIBUTING.md 配置同版本外部库，或使用 `--features bundled` / `bundled-cmake`。
不可用 2.0 动态库通过测试后声称 1.5.5 兼容。

回归包括：新旧结果的元数据和值一致性、NULL/DECIMAL/UHUGEINT/时间单位、
跨 chunk 顺序读取、多结果并存、跨线程移动、一次执行与结果快照、错误后停止读取、
提前关闭/失败/无人接收结果后的连接释放；编译失败示例约束非 Sync 与行借用。
注入 fetch 错误只验证 wrapper 错误状态机，不等于复现全部引擎故障。
连接释放测试使用 InterruptHandle 的 Weak 检查，不代替原生内存泄漏检测。

### 本次验证（2026-09-29）

- macOS aarch64，仓库指定 Rust 1.97.1，下载并链接官方 DuckDB 1.5.5 动态库。
- 无默认特性：296 个库测试通过；doctest 52 个通过、6 个原有忽略项。
- `chrono,rust_decimal,uuid`：321 个库测试通过。
- 无默认特性 all-targets 严格 Clippy（`-D warnings`）、fmt 和 diff 检查通过。
  libduckdb-sys 会打印下载/复用动态库的 build-script 提示，并非 Rust lint。
- 本次没有编译 bundled/bundled-cmake 引擎，没有运行原生泄漏检测或跨平台测试；
  没有切换 NimbusDB，因此不能把历史 JDBC 通过结果记为此次 fork 的联调结果。
- Cargo.toml、Cargo.lock、原生绑定及 DuckDB submodule 相对官方基线均未修改。
- 本地日志：`/tmp/duckdb-ownedrows-all.log`、`/tmp/duckdb-ownedrows-features.log`、
  `/tmp/duckdb-ownedrows-clippy.log`。这些临时日志不作为版本化交付文件。

## 跟随官方版本的移植流程

1. 核对使用方准备采用的官方发布 tag 和完整 SHA，从该 tag 建立
   `codex/owned-rows-<官方版本>` 分支，保留已有版本分支；不从 main 混入开发中功能。
2. 从已有 fork 发布标签提取本扩展补丁，再逐项移植到新分支。
3. 重新审查 `ExecutedResult`、`RawStatement`、连接及 Arrow chunk 的所有权、Drop 顺序、
   Send/Sync 和线程约束；不能仅凭 cherry-pick 无冲突判定正确。
4. 核对上游是否已提供等价公开 API；若已满足需求，优先删除 fork 扩展而非长期重复维护。
5. 跑上述 Rust 回归及使用方真实 JDBC 回归，验证不同 fetchSize、异常释放、
   大行、类型边界及 500 线程场景。对比首批延迟、内存和文件 I/O 后再替换文件路径。
6. 更新本文基线和验证记录。发布时使用 fork 专用且不可变的标签或固定提交，
   不移动官方标签；使用方 Cargo.toml/Cargo.lock 必须能在新 checkout 中直接构建，
   不依赖本机 patch。提交、推送、打标签及使用方切换需明确授权。

### NimbusDB 联调补验（2026-09-30）

隔离副本使用当前 OwnedRows 补丁和 bundled-cmake，实际编译 DuckDB 1.5.5。
552 个库测试、2 个入口测试及 debug/release 构建通过；Connector/J 8.4.0、9.7.0
各 10/10 通过，包括高精度 DECIMAL、不同 fetchSize、大行及 500 线程 × 10 轮。
NimbusDB 共有 DECIMAL 元数据已修复，但正式依赖和临时文件后端尚未切换。
Clippy 保留使用方既有告警及实验 spool dead-code；不等同于零告警、原生泄漏
检测或完整跨平台兼容。具体记录在 NimbusDB 的 docs/OWNED_ROWS_VALIDATION.md。
