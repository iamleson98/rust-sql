-- An ORDER BY term the planner resolves to a result expression that folds to
-- an integer constant is a constant sort key, not an ordinal ("ORDER BY 3"
-- over `x'3132' - x'00410042'` read as "ORDER BY 12"); ordinal-shaped terms
-- (+2, 2) still resolve against star projections. Top-N (ORDER BY … LIMIT)
-- breaks ties by scan position: PK order on WITHOUT ROWID tables, where the
-- internal rowid follows insertion instead.
CREATE TABLE t1(a INTEGER, b TEXT, c REAL, d NUMERIC, e BLOB, f);
INSERT INTO t1 VALUES (3, 2, NULL, NULL, 'a_b', '.5');
SELECT a, 2, (coalesce(x'3132', e) - x'00410042') FROM t1 ORDER BY 1, 2, 3 DESC;
SELECT a, 2, coalesce(x'3132', e) FROM t1 ORDER BY 1, 2, 3 DESC;
SELECT a, 2, coalesce(1, e) FROM t1 ORDER BY 3;
SELECT a, 2, coalesce(e, 1) FROM t1 ORDER BY 3;
SELECT a, 2, ifnull(1, e) FROM t1 ORDER BY 3;
SELECT a, coalesce(1, e) FROM t1 ORDER BY 2;
SELECT coalesce(1, e) FROM t1 ORDER BY 1;
SELECT a, 2, (coalesce(x'3132', e) - x'00410042') FROM t1 ORDER BY 3;
CREATE TABLE s(a, b);
INSERT INTO s VALUES (1, 'z'), (2, 'y'), (3, 'x');
/*ordered*/ SELECT * FROM s ORDER BY +2;
/*ordered*/ SELECT * FROM s ORDER BY 2;
/*ordered*/ SELECT a, b FROM s ORDER BY +2;
/*ordered*/ SELECT * FROM s ORDER BY 1 + 1, a;
/*ordered*/ SELECT a, 1 + 1 FROM s ORDER BY 2, 1 DESC;
/*ordered*/ SELECT a, abs(-2) FROM s ORDER BY 2, a DESC;
CREATE TABLE t3(j TEXT PRIMARY KEY, k INTEGER) WITHOUT ROWID;
CREATE INDEX t3_k ON t3(k);
INSERT INTO t3 VALUES ('12abc', -9223372036854775808);
INSERT INTO t3 VALUES ('1.5', -9.2233720368547758e18);
INSERT INTO t3 VALUES ('a', 5), ('b', 5.0), ('c', 2.5);
/*ordered*/ SELECT k, typeof(k) FROM t3 ORDER BY 1 LIMIT 1;
/*ordered*/ SELECT j, k FROM t3 ORDER BY 2 LIMIT 3;
/*ordered*/ SELECT j, k + 0 FROM t3 ORDER BY k + 0 LIMIT 4;
/*ordered*/ SELECT k FROM t3 ORDER BY 1;
