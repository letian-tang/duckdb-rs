#include "duckdb.hpp"
#include "duckdb.h"
#include "duckdb_rs/native_frontend.hpp"
#include "duckdb/transaction/transaction_context.hpp"
#include "duckdb/transaction/meta_transaction.hpp"
#include "duckdb/main/valid_checker.hpp"
#include <cstring>
#include <mutex>
#include <unordered_map>

using NativeConnectionHandle = duckdb_connection;
static duckdb::Connection &NativeConnection(NativeConnectionHandle handle) {
    return *reinterpret_cast<duckdb::Connection *>(handle);
}

namespace {
struct Registry {
    std::mutex mutex;
    std::unordered_map<std::string, std::shared_ptr<duckdb_rs::SqlFrontendExtension>> entries;
};
Registry &Extensions() { static Registry value; return value; }
std::shared_ptr<duckdb_rs::SqlFrontendExtension> Find(const char *name) {
    auto &registry = Extensions();
    std::lock_guard<std::mutex> guard(registry.mutex);
    auto found = registry.entries.find(name);
    if (found == registry.entries.end()) throw duckdb::InvalidInputException("unknown native SQL frontend: %s", name);
    return found->second;
}
char *Copy(const std::string &value) {
    auto result = new char[value.size() + 1];
    std::memcpy(result, value.c_str(), value.size() + 1);
    return result;
}
template <class F> char *Guard(F &&function) noexcept {
    try { return Copy(function()); }
    catch (const std::exception &error) {
        try { return Copy(std::string("ERROR:") + error.what()); } catch (...) { return nullptr; }
    } catch (...) {
        try { return Copy("ERROR:unknown native frontend failure"); } catch (...) { return nullptr; }
    }
}
}

bool duckdb_rs::RegisterSqlFrontend(const std::string &name, std::shared_ptr<SqlFrontendExtension> extension) {
    auto &registry = Extensions();
    std::lock_guard<std::mutex> guard(registry.mutex);
    return extension && registry.entries.emplace(name, std::move(extension)).second;
}

struct DuckdbRsRewriteReply { char *value; int status; };
using Callback = DuckdbRsRewriteReply (*)(void *, const char *);
using FreeReply = void (*)(char *);

extern "C" char *duckdb_rs_frontend_initialize(const char *name, NativeConnectionHandle connection) noexcept {
    return Guard([&]() -> std::string {
        if (!connection) throw duckdb::InvalidInputException("missing connection");
        Find(name)->Initialize(NativeConnection(connection));
        return "OK";
    });
}
extern "C" char *duckdb_rs_frontend_parse(const char *name, const char *sql, bool dialect, bool neutralize) noexcept {
    return Guard([&] { return Find(name)->Parse(sql, dialect, neutralize); });
}
extern "C" char *duckdb_rs_frontend_rewrite(const char *name, const char *sql, bool dialect,
                                           Callback callback, FreeReply free_reply, void *context) noexcept {
    return Guard([&] {
        duckdb_rs::SqlRewriteCallback invoke = [&](const std::string &request) {
            auto reply = callback(context, request.c_str());
            std::string value;
            if (reply.value) {
                // Release Rust memory even if copying the reply throws.
                std::unique_ptr<char, FreeReply> guard(reply.value, free_reply);
                value = reply.value;
            }
            if (reply.status) throw duckdb::InvalidInputException(value);
            return value;
        };
        return Find(name)->Rewrite(sql, dialect, invoke);
    });
}
extern "C" char *duckdb_rs_frontend_read_only(const char *name, NativeConnectionHandle connection, const char *sql) noexcept {
    return Guard([&]() -> std::string {
        if (!connection) throw duckdb::InvalidInputException("missing connection");
        return Find(name)->IsReadOnly(NativeConnection(connection), sql) ? "true" : "false";
    });
}
extern "C" void duckdb_rs_frontend_free(char *value) noexcept { delete[] value; }

// Called on the owning session thread. Reading these connection-local flags
// neither executes SQL nor starts a transaction.
extern "C" unsigned char duckdb_rs_transaction_state(NativeConnectionHandle handle) noexcept {
    auto &connection = NativeConnection(handle);
    auto &transaction = connection.context->transaction;
    unsigned char state = transaction.IsAutoCommit() ? 1 : 0;
    if (transaction.HasActiveTransaction()) {
        state |= 2;
        if (duckdb::ValidChecker::IsInvalidated(transaction.ActiveTransaction())) state |= 4;
    }
    return state;
}
