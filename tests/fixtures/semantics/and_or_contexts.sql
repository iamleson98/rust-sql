-- AND / OR evaluate differently by CONTEXT, as in SQLite's code generator:
-- * value context (a result column, an operand, a function argument, a SET
--   value): exprCodeTargetAndOr computes BOTH operands, skipping one only
--   when the other holds a subquery (or a literal decides — the
--   sqlite3ExprSimplifiedAndOr reduction);
-- * jump context (WHERE / ON / HAVING / aggregate FILTER / CASE WHEN tests /
--   CHECK / trigger WHEN, and through NOT, AND, OR, IS [NOT] TRUE/FALSE and
--   BETWEEN's expansion): sqlite3ExprIfTrue / IfFalse short-circuit, and a
--   NULL takes the jump where SQLITE_JUMPIFNULL is set.
-- Only a non-deciding operand that RAISES (abs() of the minimum integer
-- here) tells the two apart.
CREATE TABLE t(a INTEGER, b TEXT, d INTEGER);
INSERT INTO t VALUES (1, 'x', -9223372036854775808), (2, 'y', 5), (3, NULL, 6), (4, '0', NULL);
-- value context: both operands evaluate, so the MIN row raises
SELECT a > 0 OR abs(d) FROM t;
SELECT a, a < 0 AND abs(d) FROM t;
SELECT NULL AND abs(d) FROM t;
SELECT count(*) FROM t WHERE (a > 0 OR abs(d) > 0) = 1;
SELECT a, coalesce(a > 0 OR abs(d), 9) FROM t;
SELECT (a > 0 OR abs(d)) IS TRUE FROM t;
SELECT a BETWEEN 5 AND abs(d) FROM t;
SELECT a NOT BETWEEN 0 AND abs(d) FROM t;
SELECT sum(a > 0 OR abs(d)) FROM t;
SELECT a FROM t ORDER BY a > 0 OR abs(d);
-- jump context: short-circuit, no error
SELECT count(*) FROM t WHERE a > 0 OR abs(d) > 0;
SELECT count(*) FROM t WHERE a < 0 AND abs(d) > 0;
SELECT count(*) FROM t WHERE NOT (a <= 0 AND abs(d) > 0);
SELECT count(*) FROM t WHERE NOT (a > 0 OR abs(d) > 0);
SELECT count(*) FROM t WHERE NULL AND abs(d);
SELECT count(*) FROM t WHERE (a > 0 OR abs(d)) IS TRUE;
SELECT count(*) FROM t WHERE (a < 0 AND abs(d)) IS NOT TRUE;
SELECT count(*) FROM t WHERE (a < 0 AND abs(d)) IS FALSE;
SELECT count(*) FROM t WHERE (a > 0 OR abs(d)) IS NOT FALSE;
SELECT count(*) FROM t WHERE a BETWEEN 5 AND abs(d);
SELECT count(*) FROM t WHERE a NOT BETWEEN 0 AND abs(d);
SELECT a, CASE WHEN a > 0 OR abs(d) THEN 'y' ELSE 'n' END FROM t;
SELECT a, CASE WHEN NOT (a < 0 AND abs(d)) THEN 'y' END FROM t;
SELECT a, iif(a > 0 OR abs(d), 1, 0) FROM t;
SELECT a FROM t GROUP BY a HAVING a > 0 OR abs(min(d)) > 0;
SELECT count(*) FILTER (WHERE a > 0 OR abs(d) > 0) FROM t;
SELECT count(*) FROM t t1 JOIN t t2 ON t1.a > 0 OR abs(t2.d) > 0;
SELECT count(*) FROM t WHERE abs(d) > 0 AND 0;
SELECT count(*) FROM t WHERE a IN (SELECT a FROM t WHERE a > 0 OR abs(d) > 0);
SELECT count(*) FROM t WHERE EXISTS (SELECT 1 FROM t u WHERE u.a = t.a AND (u.a > 0 OR abs(u.d)));
-- IS TRUE / IS FALSE are truth tests (TK_TRUTH), not comparisons with 1 / 0
SELECT '5' IS TRUE, '5' IS 1, 0.5 IS TRUE, 'x' IS FALSE, NULL IS NOT TRUE, 2 IS NOT FALSE, x'31' IS TRUE;
SELECT a, b IS TRUE, b IS FALSE, b IS NOT TRUE, b IS NOT FALSE FROM t;
SELECT count(*) FROM t WHERE b IS NOT TRUE;
-- CHECK passes unless FALSE; the test is a jump (NULL short-circuits an OR)
CREATE TABLE ck(x TEXT CHECK (x));
INSERT INTO ck VALUES ('1');
INSERT INTO ck VALUES ('0');
INSERT INTO ck VALUES ('abc');
INSERT INTO ck VALUES ('0.5');
INSERT INTO ck VALUES (NULL);
SELECT * FROM ck;
CREATE TABLE ck2(x, y, CHECK (x OR abs(y)));
INSERT INTO ck2 VALUES (NULL, -9223372036854775808);
INSERT INTO ck2 VALUES (1, -9223372036854775808);
INSERT INTO ck2 VALUES (0, -9223372036854775808);
SELECT * FROM ck2;
-- trigger WHEN is a jump
CREATE TABLE log(v);
CREATE TRIGGER tr AFTER INSERT ON ck2 WHEN new.x > 0 OR abs(new.y) > 0 BEGIN INSERT INTO log VALUES (new.x); END;
INSERT INTO ck2 VALUES (5, -9223372036854775808);
SELECT * FROM log;
-- UPDATE SET is a value context; its WHERE is a jump
UPDATE t SET b = (a > 0 OR abs(d)) WHERE a = 1;
UPDATE t SET b = 'w' WHERE a > 0 OR abs(d) > 0;
SELECT * FROM t;
-- a join's inner side is never reached when its outer side is empty: its
-- terms never evaluate (SQLite's nested loop)
SELECT count(*) FROM t t1 JOIN t t2 ON t1.a < 0 AND abs(t2.d) > 0;
SELECT count(*) FROM t t1, t t2 WHERE t1.a < 0 AND abs(t2.d) > 0;
SELECT count(*) FROM t t1 JOIN t t2 ON t2.a = t1.a WHERE t1.a < 0 AND abs(t2.d) > 0;
SELECT count(*) FROM t t1 LEFT JOIN t t2 ON t2.a = t1.a AND abs(t2.d) > 0 WHERE t1.a < 0;
SELECT count(*) FROM t t1 LEFT JOIN t t2 ON abs(t2.d) > 0 AND t1.a < 0;
-- a truth test is an expression, not an ordinal
SELECT a FROM t GROUP BY 5 IS TRUE;
/*ordered*/ SELECT a FROM t ORDER BY 1 IS TRUE, a;
/*ordered*/ SELECT a, b IS NOT DISTINCT FROM TRUE, b IS DISTINCT FROM FALSE, b IS (TRUE), b IS NOT ((FALSE)) FROM t ORDER BY a;
-- partial-index predicates are jumps too; a raising one fails the statement
CREATE TABLE p(x INTEGER, y INTEGER);
INSERT INTO p VALUES (1, -9223372036854775808), (2, 3);
CREATE INDEX pi ON p(x) WHERE x > 1 AND abs(y) > 0;
CREATE INDEX pi2 ON p(x) WHERE x > 5 OR abs(y) > 0;
INSERT INTO p VALUES (0, -9223372036854775808);
INSERT INTO p VALUES (9, -9223372036854775808);
SELECT x FROM p WHERE x > 1 AND abs(y) > 0;
SELECT count(*) FROM p;
-- index maintenance that RAISES (an expression key, a partial predicate)
-- fails the statement atomically: the row it was writing comes back out
CREATE TABLE q(x INTEGER, y INTEGER);
CREATE INDEX qe ON q(abs(y));
INSERT INTO q VALUES (9, -9223372036854775808);
INSERT INTO q VALUES (0, 1);
INSERT INTO q VALUES (9, -9223372036854775808);
SELECT * FROM q;
INSERT INTO p VALUES (3, 1);
INSERT INTO p VALUES (8, -9223372036854775808);
SELECT * FROM p;
BEGIN;
INSERT INTO q VALUES (10, 1);
INSERT INTO q VALUES (11, 1), (12, -9223372036854775808);
UPDATE q SET y = y + 10;
UPDATE q SET y = CASE x WHEN 10 THEN -9223372036854775808 ELSE 7 END;
SELECT * FROM q;
COMMIT;
SELECT x FROM q WHERE abs(y) > 10;
CREATE TABLE r(a INTEGER PRIMARY KEY, b INTEGER);
CREATE INDEX re ON r(abs(b));
INSERT INTO r VALUES (1, 5);
BEGIN;
INSERT OR REPLACE INTO r VALUES (1, 6), (2, -9223372036854775808);
SELECT * FROM r;
COMMIT;
SELECT a FROM r WHERE abs(b) = 5;
PRAGMA integrity_check;
