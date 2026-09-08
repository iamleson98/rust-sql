#!/usr/bin/env bash
# CLI-level SQLite file-format interop check.
#
# Uses the REAL sqlite3 CLI (not rusqlite) in both directions:
#   1. sqlite3 creates a database -> rustqlite-cli reads AND writes it.
#   2. rustqlite-cli --sqlite-format creates a database -> sqlite3 reads it.
#   3. A second rustqlite-cli session re-reads what it wrote.
#
# Requires: sqlite3 on PATH, a built rustqlite-cli binary.
set -uo pipefail

SQLITE3="${SQLITE3:-sqlite3}"
CLI="${CLI:-./target/debug/rustqlite-cli}"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

fail() {
    echo "FAIL: $1" >&2
    exit 1
}

# Fallback: environments without the sqlite3 binary (sandboxed dev
# containers) use a python3-backed shim that speaks the same subset
# (`sqlite3 DB [SQL|-]`). CI installs the real CLI.
if ! command -v "$SQLITE3" >/dev/null 2>&1; then
    if command -v python3 >/dev/null 2>&1; then
        SHIM_PY="$WORK/sqlite3_shim.py"
        cat > "$SHIM_PY" <<'PY'
import sqlite3, sys
db = sys.argv[1]
con = sqlite3.connect(db)
if len(sys.argv) > 2 and sys.argv[2] != "-":
    for row in con.execute(sys.argv[2]).fetchall():
        print("|".join("NULL" if v is None else str(v) for v in row))
else:
    for line in sys.stdin:
        s = line.strip()
        if not s or s.startswith("."):
            continue
        for row in con.execute(s).fetchall():
            print("|".join("NULL" if v is None else str(v) for v in row))
con.commit()
PY
        SHIM="$WORK/sqlite3_shim.sh"
        printf '#!/usr/bin/env bash\nexec python3 "$1" "$2" "${3:-}"\n' > "$SHIM"
        # shellcheck disable=SC2016
        cat > "$SHIM" <<'OUTER'
#!/usr/bin/env bash
exec python3 "$(dirname "$0")/sqlite3_shim.py" "$@"
OUTER
        chmod +x "$SHIM"
        SQLITE3="$SHIM"
        echo "note: sqlite3 binary not found, using python3 shim"
    else
        fail "sqlite3 CLI not found on PATH and python3 unavailable"
    fi
fi

command -v "$SQLITE3" >/dev/null 2>&1 || fail "sqlite3 CLI not found on PATH"
[ -x "$CLI" ] || fail "rustqlite-cli not built at $CLI (cargo build --bin rustqlite-cli)"

DB_A="$WORK/sqlite_created.db"
DB_B="$WORK/engine_created.db"

echo "== 1. sqlite3 creates, rustqlite reads+writes =="
"$SQLITE3" "$DB_A" <<'EOF'
CREATE TABLE t(id INTEGER PRIMARY KEY, name TEXT, score REAL);
INSERT INTO t VALUES (1, 'alice', 1.5), (2, 'bob', 2.5), (3, 'carol', 3.5);
CREATE TABLE notes(n);
INSERT INTO notes VALUES ('hello');
CREATE INDEX idx_name ON t(name);
EOF
[ -s "$DB_A" ] || fail "sqlite3 did not create the file"

printf "SELECT name FROM t WHERE id = 2;\n.quit\n" | "$CLI" "$DB_A" | grep -q "bob" \
    || fail "engine could not read sqlite3-created data"

printf "INSERT INTO t VALUES (4, 'dave', 4.5);\nDELETE FROM t WHERE id = 1;\n.quit\n" | "$CLI" "$DB_A" >/dev/null \
    || fail "engine write to sqlite3-created file failed"

"$SQLITE3" "$DB_A" "SELECT count(*) FROM t;" | grep -q 3 \
    || fail "sqlite3 cannot see the engine's writes"
IC_A="$("$SQLITE3" "$DB_A" "PRAGMA integrity_check;")"
[ "$IC_A" = "ok" ] || fail "integrity_check after engine writes: $IC_A"

echo "== 2. rustqlite --sqlite-format creates, sqlite3 reads =="
rm -f "$DB_B"
printf "CREATE TABLE u(a INTEGER PRIMARY KEY, b TEXT UNIQUE, c BLOB);\nBEGIN;\nINSERT INTO u VALUES (1, 'x', x'0102');\nINSERT INTO u VALUES (2, 'y', NULL);\nCOMMIT;\n.quit\n" | "$CLI" --sqlite-format "$DB_B" >/dev/null \
    || fail "engine sqlite-format session failed"
[ -s "$DB_B" ] || fail "engine did not write the sqlite-format file"

"$SQLITE3" "$DB_B" "SELECT a, b FROM u ORDER BY a;" | tr '\n' ' ' | grep -q "1|x 2|y" \
    || fail "sqlite3 cannot read engine-created file"
IC_B="$("$SQLITE3" "$DB_B" "PRAGMA integrity_check;")"
[ "$IC_B" = "ok" ] || fail "integrity_check on engine-created file: $IC_B"
# UNIQUE constraint still enforced by SQLite itself on the engine's file.
if "$SQLITE3" "$DB_B" "INSERT INTO u(b) VALUES ('x');" 2>/dev/null; then
    fail "UNIQUE constraint lost in the engine's file"
fi

echo "== 3. engine re-reads its own file =="
printf "SELECT count(*) FROM u;\n.quit\n" | "$CLI" "$DB_B" | grep -q 2 \
    || fail "engine cannot reopen its own sqlite-format file"

echo "PASS: CLI-level SQLite interop verified in both directions."
