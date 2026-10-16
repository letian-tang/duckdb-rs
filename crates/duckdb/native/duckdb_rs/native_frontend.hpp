#pragma once

#include <functional>
#include <memory>
#include <string>

namespace duckdb { class Connection; }

namespace duckdb_rs {
// A callback is borrowed only for one synchronous Rewrite invocation. Exceptions
// are contained by the host before returning to Rust. ASTs stay in the extension.
using SqlRewriteCallback = std::function<std::string(const std::string &)>;

class SqlFrontendExtension {
public:
    virtual ~SqlFrontendExtension() = default;
    virtual void Initialize(duckdb::Connection &connection) = 0;
    virtual std::string Parse(const std::string &sql, bool dialect, bool neutralize) = 0;
    virtual std::string Rewrite(const std::string &sql, bool dialect, const SqlRewriteCallback &callback) = 0;
    virtual bool IsReadOnly(duckdb::Connection &connection, const std::string &sql) = 0;
};

// Extensions are process-lived and can be called concurrently on distinct
// connections. Use immutable shared grammar and per-invocation Parser/AST state.
// Link the extension object with whole-archive so its registration is retained.
bool RegisterSqlFrontend(const std::string &name, std::shared_ptr<SqlFrontendExtension> extension);
}
