/* Tiny always-available oracle: the bundled SQLite 3.53.4 amalgamation.
** Reads SQL from stdin (one statement per line or ;-terminated), prints
** rows pipe-separated, NULL as <NULL>, errors as !ERR: message.
** Build: cc -O2 -I vendor/libsqlite3-sys/sqlite3 scripts/oracle.c \
**          vendor/libsqlite3-sys/sqlite3/sqlite3.c -o /tmp/oracle -lm
*/
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "sqlite3.h"

static int cb(void *unused, int n, char **v, char **names) {
    (void)unused; (void)names;
    for (int i = 0; i < n; i++)
        printf("%s%s", v[i] ? v[i] : "<NULL>", i + 1 < n ? "|" : "");
    printf("\n");
    return 0;
}

int main(void) {
    sqlite3 *db;
    if (sqlite3_open(":memory:", &db) != SQLITE_OK) {
        fprintf(stderr, "open failed\n"); return 1;
    }
    char *line = NULL; size_t cap = 0; ssize_t n;
    char buf[65536]; buf[0] = 0;
    while ((n = getline(&line, &cap, stdin)) > 0) {
        if (strncmp(line, ".", 1) == 0) continue;
        if (strlen(buf) + (size_t)n >= sizeof buf) { fprintf(stderr, "!ERR: stmt too long\n"); buf[0]=0; continue; }
        strcat(buf, line);
        if (strchr(line, ';') || strcmp(line, "\n") == 0) {
            char *err = 0;
            int rc = sqlite3_exec(db, buf, cb, 0, &err);
            if (rc != SQLITE_OK) { printf("!ERR: %s\n", err ? err : "?"); sqlite3_free(err); }
            buf[0] = 0;
        }
    }
    return 0;
}
