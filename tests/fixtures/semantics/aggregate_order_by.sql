-- ORDER BY inside an aggregate's arguments (SQLite 3.44): each aggregate
-- reads its group's rows in its own order — DISTINCT, separators, NULL
-- placement, collations, FILTER and GROUP BY included; misuse on scalar /
-- window calls is SQLite's error.
CREATE TABLE t(a INTEGER, b TEXT COLLATE NOCASE, c, g INTEGER);
INSERT INTO t VALUES (3, 'x', 1.5, 1), (1, 'Y', NULL, 2), (2, 'y', 'q', 1), (5, NULL, 7, 2), (4, 'X', 2, 1), (6, 'z', 0, 2);
SELECT group_concat(b ORDER BY a DESC) FROM t;
SELECT group_concat(b ORDER BY a DESC), group_concat(b, '-' ORDER BY b) FROM t;
SELECT group_concat(b, '-' ORDER BY b COLLATE BINARY), group_concat(b ORDER BY b DESC, a) FROM t;
/*ordered*/ SELECT g, group_concat(a ORDER BY a DESC), group_concat(b ORDER BY c NULLS LAST) FROM t GROUP BY g ORDER BY g;
SELECT string_agg(b, ';' ORDER BY a DESC) FROM t;
SELECT json_group_array(a ORDER BY a DESC), json_group_array(c ORDER BY c) FROM t;
SELECT json_group_object(b, a ORDER BY a) FROM t WHERE b IS NOT NULL;
SELECT group_concat(DISTINCT b ORDER BY b DESC) FROM t;
SELECT group_concat(a ORDER BY a) FILTER (WHERE g = 1) FROM t;
SELECT sum(a ORDER BY b), max(a ORDER BY a DESC), count(a ORDER BY b) FROM t;
SELECT count(* ORDER BY a) FROM t;
SELECT group_concat(a ORDER BY c DESC NULLS FIRST, a) FROM t;
SELECT group_concat(a ORDER BY a % 2, a DESC) FROM t;
SELECT group_concat(b ORDER BY a) FROM t WHERE a > 100;
/*ordered*/ SELECT g, group_concat(b ORDER BY a DESC) AS s FROM t GROUP BY g HAVING group_concat(b ORDER BY a DESC) LIKE 'z%' OR g = 1 ORDER BY g;
SELECT (SELECT group_concat(b ORDER BY a DESC) FROM t u WHERE u.g = t.g) FROM t WHERE a = 1;
SELECT group_concat(b ORDER BY a DESC) OVER () FROM t;
SELECT abs(a ORDER BY b) FROM t;
SELECT group_concat(b ORDER BY nosuchcol) FROM t;
CREATE TABLE r(id INTEGER PRIMARY KEY, x);
INSERT INTO r(x) VALUES (5), (5.0), (2);
SELECT max(x ORDER BY id DESC), typeof(max(x ORDER BY id DESC)), min(x ORDER BY x) FROM r;
