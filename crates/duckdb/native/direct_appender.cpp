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
        if (!description) throw duckdb::CatalogException("Direct appender table disappeared during construction");
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
            // Validate the buffers themselves, not just the second metadata description.
            // Full LogicalType equality includes DECIMAL width/scale and nested types.
            duckdb::vector<duckdb::LogicalIndex> ids;
            duckdb::vector<duckdb::LogicalType> target_types;
            auto names = GetExpectedNames();
            if (names.empty()) {
                for (auto &col : columns.Physical()) target_types.push_back(col.Type());
            } else {
                for (auto &name : names) {
                    bool found = false;
                    for (auto &col : columns.Logical()) {
                        if (col.Name() == name && !col.Generated()) {
                            ids.push_back(col.Logical());
                            target_types.push_back(col.Type());
                            found = true;
                            break;
                        }
                    }
                    if (!found) throw duckdb::InvalidInputException("Direct appender active column disappeared");
                }
            }
            if (collection.Types() != GetActiveTypes() || collection.Types() != target_types) {
                throw duckdb::InvalidInputException("Direct appender buffered column types do not match the table");
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
    duckdb::Connection *con = nullptr;
    bool owns_construction_transaction = false;
    auto rollback_construction = [&]() noexcept {
        if (!owns_construction_transaction || !con) return;
        // BEGIN can fail after changing autocommit, and COMMIT/ROLLBACK can fail
        // after clearing the active transaction. Preserve the original error.
        try { if (con->HasActiveTransaction()) con->Rollback(); } catch (...) {}
        try { if (!con->HasActiveTransaction()) con->SetAutoCommit(true); } catch (...) {}
    };
    try {
        wrapper = new duckdb::AppenderWrapper();
        *out = reinterpret_cast<duckdb_appender>(wrapper);
        con = reinterpret_cast<duckdb::Connection *>(connection);
        if (!con->HasActiveTransaction()) {
            if (!con->IsAutoCommit()) {
                throw duckdb::InvalidInputException("Direct appender requires an active transaction when autocommit is off");
            }
            // Register ownership BEFORE BEGIN: even a failing begin may leave state.
            // The public transaction APIs lock the context, then release it before
            // TableInfo takes its own lock. Both constructor reads share one snapshot.
            owns_construction_transaction = true;
            con->BeginTransaction();
        }
        wrapper->appender = duckdb::make_uniq<DirectAppender>(*con, schema ? schema : DEFAULT_SCHEMA, table);
        if (owns_construction_transaction) {
            con->Commit();
            owns_construction_transaction = false;
        }
        return DuckDBSuccess;
    } catch (const std::exception &ex) {
        rollback_construction();
        try { if (wrapper) wrapper->error_data = duckdb::ErrorData(ex); } catch (...) {}
    } catch (...) {
        rollback_construction();
        try { if (wrapper) wrapper->error_data = duckdb::ErrorData("Direct appender creation failed"); } catch (...) {}
    }
    return DuckDBError;
}
