-- An INNER join's ON terms are WHERE terms (sqlite3ProcessJoin), so the
-- constant ones are evaluated ONCE before the loop: they raise even when
-- no row pair would reach them, and a false one returns no rows without
-- reading any. A LEFT join's ON terms stay with its inner table, and any
-- RIGHT / FULL join keeps inner-join ON terms in the loop too.
CREATE TABLE t1(a INTEGER, b TEXT);
CREATE TABLE t2(id INTEGER PRIMARY KEY, g INTEGER);
CREATE TABLE t3(k);
INSERT INTO t1 VALUES (1, 'x'), (2, 'y'), (NULL, 'z');
INSERT INTO t2 VALUES (10, 1), (11, 2);
SELECT g FROM t1 JOIN t2 ON t1.a = t2.id AND (abs(-9223372036854775808) AND 1e20);
SELECT g FROM t1 JOIN t2 ON t1.a = t2.id AND abs(-9223372036854775808);
SELECT g FROM t1 JOIN t2 ON t1.a = t2.g AND 0 AND abs(-9223372036854775808);
SELECT g FROM t1 JOIN t2 ON t1.a = t2.g AND abs(-9223372036854775808) WHERE 0;
SELECT g FROM t1 JOIN t2 ON abs(-5) = 5 AND t1.a = t2.g;
SELECT g FROM t1 JOIN t2 ON '' WHERE t1.a = t2.g;
SELECT g FROM t1 CROSS JOIN t2 ON abs(-9223372036854775808) WHERE 0;
SELECT g FROM t1, t2 JOIN t3 ON abs(-9223372036854775808);
SELECT g FROM t1 JOIN t3 ON abs(-9223372036854775808) JOIN t2 ON t2.g = t1.a;
SELECT g FROM t3 JOIN t1 ON abs(-9223372036854775808) JOIN t2 ON t2.g = t1.a;
SELECT count(*) FROM t1 LEFT JOIN t2 ON t1.a = t2.g AND abs(-5) = 5;
SELECT count(*) FROM t1 LEFT JOIN t2 ON t1.a < 0 AND abs(-9223372036854775808);
SELECT count(*) FROM t3 LEFT JOIN t2 ON abs(-9223372036854775808);
SELECT count(*), sum(g) FROM t1 JOIN t2 ON t1.a = t2.g AND 2 > 1;
SELECT count(*) FROM t1 JOIN t2 ON t1.a = t2.g AND NULL;
SELECT count(*) FROM t1 JOIN t2 ON t1.a = t2.g AND 0.5 GROUP BY t1.b;
