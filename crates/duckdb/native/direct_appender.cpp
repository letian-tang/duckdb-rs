// Direct storage append with the standard Appender's logical input encoding.
// Keep this bridge in the driver; callers never own C++ handles or catalog entries.
// Flush follows AliSQL PhysicalAppender: validate schema, bind constraints, mark
// the transaction modified, then LocalAppend. The input conversion stays LOGICAL.
#include "duckdb.hpp"
#include "duckdb.h"
#include "duckdb/main/capi/capi_internal.hpp"
#include "duckdb/catalog/catalog_entry/duck_table_entry.hpp"
#include "duckdb/planner/binder.hpp"
#include "duckdb/storage/data_table.hpp"
#include "duckdb/transaction/meta_transaction.hpp"

namespace {
class DirectAppender final : public duckdb::Appender {
public:
    DirectAppender(duckdb::Connection &con, const char *schema, const char *table)
        : Appender(con, duckdb::Identifier(schema), duckdb::Identifier(table)),
          context(con.context), description(con.TableInfo(schema, table)) {
    }
    ~DirectAppender() override { Destructor(); }

protected:
    void FlushInternal(duckdb::ColumnDataCollection &collection) override {
        auto ctx = context.lock();
        if (!ctx) throw duckdb::InvalidInputException("Direct appender connection is closed");
        ctx->RunFunctionInTransaction([&]() {
            auto &table = duckdb::Catalog::GetEntry<duckdb::TableCatalogEntry>(
                *ctx, description->qualified_name);
            auto &columns = table.GetColumns();
            if (description->columns.size() != columns.LogicalColumnCount()) {
                throw duckdb::InvalidInputException("Direct appender table columns changed");
            }
            for (duckdb::idx_t i = 0; i < description->columns.size(); ++i) {
                auto &expected = description->columns[i];
                auto &actual = columns.GetColumn(duckdb::LogicalIndex(i));
                if (expected.Name() != actual.Name() || expected.Type() != actual.Type() ||
                    expected.Generated() != actual.Generated()) {
                    throw duckdb::InvalidInputException("Direct appender table columns changed");
                }
            }
            // Reuse Appender::AddColumn/ClearColumns and their default-value semantics.
            duckdb::vector<duckdb::LogicalIndex> ids;
            for (auto &name : GetExpectedNames()) {
                for (auto &col : columns.Logical()) {
                    if (col.Name() == name) { ids.push_back(col.Logical()); break; }
                }
            }
            auto binder = duckdb::Binder::CreateBinder(*ctx);
            auto constraints = binder->BindConstraints(table);
            duckdb::MetaTransaction::Get(*ctx).ModifyDatabase(
                table.ParentCatalog().GetAttached(), duckdb::DatabaseModificationType::INSERT_DATA);
            table.GetStorage().LocalAppend(table.Cast<duckdb::DuckTableEntry>(), *ctx, collection,
                                           constraints, ids.empty() ? nullptr : &ids);
        });
    }
private:
    duckdb::weak_ptr<duckdb::ClientContext> context;
    duckdb::unique_ptr<duckdb::TableDescription> description;
};
}

extern "C" duckdb_state duckdb_rs_direct_appender_create(
    duckdb_connection connection, const char *schema, const char *table, duckdb_appender *out) noexcept {
    if (!connection || !table || !out) return DuckDBError;
    *out = nullptr;
    // Allocate inside the exception boundary. The normal C API owns/destroys the wrapper.
    duckdb::AppenderWrapper *wrapper = nullptr;
    try {
        wrapper = new duckdb::AppenderWrapper();
        *out = reinterpret_cast<duckdb_appender>(wrapper);
        auto &con = *reinterpret_cast<duckdb::Connection *>(connection);
        wrapper->appender = duckdb::make_uniq<DirectAppender>(con, schema ? schema : DEFAULT_SCHEMA, table);
        return DuckDBSuccess;
    } catch (const std::exception &ex) {
        try { if (wrapper) wrapper->error_data = duckdb::ErrorData(ex); } catch (...) {}
    } catch (...) {
        try { if (wrapper) wrapper->error_data = duckdb::ErrorData("Direct appender creation failed"); } catch (...) {}
    }
    return DuckDBError;
}
