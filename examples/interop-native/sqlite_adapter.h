#ifndef KU_EXAMPLE_SQLITE_ADAPTER_H
#define KU_EXAMPLE_SQLITE_ADAPTER_H

#include "sqlite3.h"

/* The scalar SQLite function is exported directly through the bridge. */
int sqlite3_libversion_number(void);

/* Both C-string inputs are call-scoped. Returns a separately allocated UTF-8
 * copy on success, or NULL after all SQLite handles have been released. */
char* example_query(const char* sql, const char* input);
void example_release(char* value);

#endif
