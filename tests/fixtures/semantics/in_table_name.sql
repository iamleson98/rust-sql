-- `x IN [schema.]name` is `x IN (SELECT * FROM [schema.]name)` over a
-- one-column table, view or CTE (the bare form used to fail); a scalar
-- operand against a multi-column subquery is an error, not a silent
-- first-column match.
CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER);
INSERT INTO t(v) VALUES (7), (8), (9);
CREATE TABLE s (n INTEGER);
INSERT INTO s VALUES (7), (9);
SELECT id FROM t WHERE v IN s;
SELECT id FROM t WHERE v NOT IN s;
WITH x(n) AS (SELECT 7) SELECT id FROM t WHERE v IN x;
WITH x(n) AS (SELECT 7 UNION ALL SELECT 8) SELECT id FROM t WHERE v NOT IN x;
WITH x(n) AS (SELECT 7) UPDATE t SET v = v * 10 WHERE v IN x;
SELECT * FROM t;
WITH x(n) AS (SELECT 8) DELETE FROM t WHERE v IN x;
SELECT * FROM t;
SELECT 7 IN main.s, 8 IN main.s;
CREATE VIEW sv AS SELECT n FROM s;
SELECT id FROM t WHERE v IN sv;
SELECT 7 IN s, 7 NOT IN s, NULL IN s, 7 IN (SELECT n FROM s);
CREATE TABLE two (a, b);
SELECT 1 IN two;
SELECT 1 IN (SELECT a, b FROM two);
INSERT INTO two VALUES (1, 2);
SELECT 1 IN (SELECT a, b FROM two);
SELECT * FROM two WHERE a IN (SELECT a, b FROM two);
