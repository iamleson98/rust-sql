-- (1) An aggregate argument's collation is found through CAST and unary
-- `+` (sqlite3ExprCollSeq continues to the operand): min / max and
-- DISTINCT aggregates over `CAST(h AS TEXT)` of a NOCASE column compare
-- NOCASE (strict-fuzz seed 780000+).
-- (2) havingToWhere moves constant HAVING terms into the WHERE except
-- an ALWAYS-FALSE one (EP_IsFalse: an integer 0 literal, FALSE, a folded
-- `<literal> IS NULL`): the groups are still built, so their aggregate
-- arguments still evaluate — and can raise — before the HAVING rejects
-- every group (strict-fuzz seed 788000+).
CREATE TABLE t2(id INTEGER PRIMARY KEY, g INTEGER, h TEXT COLLATE NOCASE, i REAL);
CREATE INDEX t2_h ON t2(h);
INSERT INTO t2(g, h, i) VALUES (1, 'a%b', 1.5), (2, 'ABC', 2.5), (3, 'abd', 1.5), (4, 'Abc', NULL), (5, NULL, 2.5), (6, 'b', 3.0);
SELECT max(CAST(h AS TEXT)) FROM t2;
SELECT min(CAST(h AS TEXT)) FROM t2;
SELECT max(+h), min(+h) FROM t2;
SELECT max(CAST(+h AS TEXT)) FROM t2;
SELECT count(DISTINCT CAST(h AS TEXT)) FROM t2;
SELECT i, max(CAST(h AS TEXT)), min(CAST(h AS TEXT)) FROM t2 GROUP BY i;
SELECT group_concat(h, ',' ORDER BY CAST(h AS TEXT)) FROM t2;
SELECT max(CAST(h AS BLOB)) FROM t2;
SELECT max(h || '') FROM t2;
SELECT i, max(abs(-9223372036854775808)) FROM t2 GROUP BY i HAVING 0;
SELECT i, max(abs(-9223372036854775808)) FROM t2 GROUP BY i HAVING FALSE;
SELECT i, max(abs(-9223372036854775808)) FROM t2 GROUP BY i HAVING 5 IS NULL;
SELECT i, max(abs(-9223372036854775808)) FROM t2 GROUP BY i HAVING count(*) AND 0;
SELECT i, max(abs(-9223372036854775808)) FROM t2 GROUP BY i HAVING 0 AND count(*);
SELECT i, max(abs(-9223372036854775808)) FROM t2 GROUP BY i HAVING 1 = 0;
SELECT i, max(abs(-9223372036854775808)) FROM t2 GROUP BY i HAVING NULL;
SELECT i, max(abs(-9223372036854775808)) FROM t2 GROUP BY i HAVING 0.0;
SELECT i, max(abs(-9223372036854775808)) FROM t2 GROUP BY i HAVING '0';
SELECT i, max(abs(-9223372036854775808)) FROM t2 GROUP BY i HAVING 4294967296 - 4294967296;
SELECT i, count(*) FROM t2 GROUP BY i HAVING 0;
SELECT i, count(*) FROM t2 GROUP BY i HAVING 1;
SELECT i, max(((NULL || 33.9) IN (h, abs(-9223372036854775808)))) FROM t2 GROUP BY i HAVING octet_length(group_concat(id)) AND (-9223372036854775808 IS NULL);
