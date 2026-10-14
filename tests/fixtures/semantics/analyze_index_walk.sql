-- ANALYZE walks each index in order (no table scan + sort): stat1 and
-- stat4 must match SQLite for duplicates, NULLs, collations, UNIQUE NOT
-- NULL, partial and expression indexes, multi-column keys, an INTEGER
-- and a REAL that compare equal, and the DESC shape (exact path).
CREATE TABLE t(a INTEGER, b TEXT COLLATE NOCASE, c REAL, d TEXT NOT NULL, e);
WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM n WHERE i<700)
INSERT INTO t SELECT i % 37, CASE i % 5 WHEN 0 THEN 'Abc' WHEN 1 THEN 'aBC' WHEN 2 THEN NULL ELSE 'x' || (i % 11) END, CASE WHEN i % 3 = 0 THEN 2 ELSE (i % 9) * 1.0 END, 'k' || i, CASE WHEN i % 4 = 0 THEN NULL ELSE i % 13 END FROM n;
CREATE INDEX ta ON t(a);
CREATE INDEX tb ON t(b);
CREATE INDEX tc ON t(c);
CREATE UNIQUE INDEX td ON t(d);
CREATE INDEX tab ON t(a, b);
CREATE INDEX tpart ON t(e) WHERE e > 5;
CREATE INDEX texpr ON t(a + e, lower(b));
CREATE INDEX tdesc ON t(a DESC, e);
CREATE TABLE u(x INTEGER PRIMARY KEY, y UNIQUE, z);
INSERT INTO u SELECT a, d, e FROM t WHERE rowid <= 37;
CREATE INDEX uz ON u(z);
CREATE INDEX tdd ON t(b DESC, a DESC);
CREATE INDEX tcd ON t(c DESC);
CREATE TABLE w(k TEXT PRIMARY KEY, v) WITHOUT ROWID;
INSERT INTO w SELECT d, a FROM t;
CREATE INDEX wv ON w(v);
CREATE TABLE w2(p INTEGER, q TEXT, r, PRIMARY KEY(p DESC, q)) WITHOUT ROWID;
INSERT INTO w2 SELECT a, d, e FROM t;
CREATE TABLE w3(p INTEGER, q TEXT COLLATE NOCASE, PRIMARY KEY(p, q)) WITHOUT ROWID;
INSERT OR IGNORE INTO w3 SELECT a, b FROM t WHERE b IS NOT NULL;
CREATE TABLE empty(q);
CREATE INDEX eq ON empty(q);
ANALYZE;
SELECT tbl, idx, stat FROM sqlite_stat1 ORDER BY tbl, idx;
SELECT tbl, idx, neq, nlt, ndlt, hex(sample) FROM sqlite_stat4 ORDER BY tbl, idx, nlt;
