#include "sqlite_adapter.h"

#include <stdlib.h>
#include <string.h>

static int read_only_select(void* context, int action, const char* first,
    const char* second, const char* database, const char* view) {
    (void)context;
    (void)first;
    (void)second;
    (void)database;
    (void)view;
    return action == SQLITE_SELECT ? SQLITE_OK : SQLITE_DENY;
}

char* example_query(const char* sql, const char* input) {
    sqlite3* db = NULL;
    sqlite3_stmt* statement = NULL;
    char* copy = NULL;
    if (sqlite3_open(":memory:", &db) != SQLITE_OK) goto done;
    if (sqlite3_set_authorizer(db, read_only_select, NULL) != SQLITE_OK) goto done;
    if (sqlite3_prepare_v2(db, sql, -1, &statement, NULL) != SQLITE_OK || !statement) goto done;
    if (sqlite3_bind_text(statement, 1, input, -1, SQLITE_TRANSIENT) != SQLITE_OK) goto done;
    if (sqlite3_step(statement) != SQLITE_ROW || sqlite3_column_count(statement) != 1) goto done;

    const unsigned char* text = sqlite3_column_text(statement, 0);
    int length = sqlite3_column_bytes(statement, 0);
    if (!text || length < 0 || length > 1000000 || memchr(text, 0, (size_t)length)) goto done;
    copy = (char*)malloc((size_t)length + 1);
    if (copy) {
        memcpy(copy, text, (size_t)length);
        copy[length] = '\0';
    }

done:
    if (statement) sqlite3_finalize(statement);
    if (db) sqlite3_close(db);
    return copy;
}

void example_release(char* value) {
    free(value);
}
