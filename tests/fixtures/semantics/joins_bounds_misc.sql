CREATE TABLE t1(a INTEGER, d NUMERIC);
CREATE TABLE t2(id INTEGER PRIMARY KEY, g INTEGER, h TEXT, i REAL);
CREATE INDEX t2_g ON t2(g);
INSERT INTO t1 VALUES (NULL, 5), (1, 2), (2, 3), (NULL, 7);
INSERT INTO t2(g, h, i) VALUES (NULL, 'a', 1.0), (1, 'b', 2.0), (NULL, 'c', 3.0), (2, '0', 4.0);
SELECT t1.a, t2.g FROM t1 JOIN t2 ON t1.a = t2.g;
SELECT t1.a, t2.g FROM t1 INNER JOIN t2 ON t1.a = t2.g WHERE (NOT (d & x'3132'));
SELECT count(*) FROM t1, t2 WHERE t1.a = t2.g;
SELECT t1.a FROM t1 WHERE a IN (SELECT g FROM t2);
SELECT t1.a FROM t1 WHERE a NOT IN (SELECT g FROM t2);
SELECT b, (- b) = b, typeof(-b) FROM (SELECT '0' AS b UNION ALL SELECT '0.0');
CREATE TABLE tb(b TEXT);
INSERT INTO tb VALUES ('0'), ('0.0'), ('1');
SELECT b, (- b) = b FROM tb;
SELECT id FROM t2 WHERE (('Abc' & h) < id);
UPDATE t2 SET i = 99 WHERE (('Abc' & h) < id);
SELECT * FROM t2;
CREATE TABLE d1(id INTEGER PRIMARY KEY, g INTEGER);
INSERT INTO d1(g) VALUES (1), (2), (3), (4), (5);
DELETE FROM d1 WHERE (SELECT max(g) FROM d1) = g;
SELECT * FROM d1;
DELETE FROM d1 WHERE g > (SELECT avg(g) FROM d1);
SELECT * FROM d1;
INSERT INTO d1(g) VALUES (10), (20), (30);
UPDATE d1 SET g = g + 100 WHERE g < (SELECT max(g) FROM d1);
SELECT * FROM d1;
DELETE FROM d1 WHERE (SELECT count(*) FROM d1 x WHERE x.g < d1.g) > 1;
SELECT * FROM d1;
SELECT DISTINCT (EXISTS (SELECT 1 FROM tb) < b) FROM tb;
SELECT 1 FROM tb WHERE ((EXISTS (SELECT 1 FROM tb WHERE b = b) < b) >= 0);
SELECT unicode(char(0) || 'a'), quote('a' || char(0) || 'b'), length(quote('a' || char(0)));
SELECT 1 IN (2 COLLATE BINARY, 1), 'a' COLLATE BINARY IN ('A');
SELECT 5 FROM tb GROUP BY 4294967296;
SELECT b FROM tb ORDER BY 2147483648;
-- IN / NOT IN over an EMPTY right-hand side is FALSE / TRUE even for NULL.
CREATE TABLE empty_t(g INTEGER);
CREATE TABLE nn(e);
INSERT INTO nn VALUES (NULL), (1), (x'c3a9');
SELECT e IN (SELECT g FROM empty_t), e NOT IN (SELECT g FROM empty_t) FROM nn;
SELECT count(*) FROM nn WHERE e NOT IN (SELECT g FROM empty_t);
SELECT count(*) FROM nn WHERE e IN (SELECT g FROM empty_t);
SELECT NULL IN (SELECT g FROM empty_t), NULL NOT IN (SELECT g FROM empty_t);
-- IN (SELECT …) combines both sides' affinities (exprINAffinity).
CREATE TABLE sub_i(g INTEGER);
INSERT INTO sub_i VALUES (7), (3), (NULL), (9223372036854775807);
CREATE TABLE lhs(j TEXT, f, n INTEGER);
INSERT INTO lhs VALUES ('+7', '3', 3), (' 3 ', 3.0, 7), ('x', NULL, 8), ('9223372036854775807', '7.0', NULL);
SELECT j, j IN (SELECT g FROM sub_i), f IN (SELECT g FROM sub_i), n IN (SELECT g FROM sub_i) FROM lhs;
SELECT j, j NOT IN (SELECT g FROM sub_i), f NOT IN (SELECT g FROM sub_i) FROM lhs;
SELECT count(*) FROM lhs WHERE j IN (SELECT g FROM sub_i);
SELECT count(*) FROM lhs WHERE n IN (SELECT g FROM sub_i);
SELECT count(*) FROM lhs WHERE f NOT IN (SELECT g FROM sub_i WHERE g IS NOT NULL);
SELECT '9223372036854775807' IN (SELECT g FROM sub_i), '3' IN (SELECT g FROM sub_i), 5 IN (SELECT g FROM sub_i), NULL IN (SELECT g FROM sub_i);
-- Collation precedence: a LEFT column's collation (BINARY included) wins.
CREATE TABLE cb(b TEXT);
CREATE TABLE cn(h TEXT COLLATE NOCASE);
INSERT INTO cb VALUES ('abc'), ('X');
INSERT INTO cn VALUES ('ABC'), ('x'), ('abc');
SELECT cb.b, cn.h FROM cb JOIN cn ON cb.b = cn.h;
SELECT cb.b, cn.h FROM cb JOIN cn ON cn.h = cb.b;
SELECT cb.b FROM cb, cn WHERE cb.b = cn.h;
SELECT count(*) FROM cn WHERE h = 'ABC';
SELECT count(*) FROM cn WHERE 'ABC' = h;
SELECT b FROM cb WHERE b IN (SELECT h FROM cn);
SELECT h FROM cn WHERE h IN (SELECT b FROM cb);
SELECT h FROM cn WHERE h IN ('ABC', 'X');
SELECT h FROM cn WHERE h BETWEEN 'A' AND 'B';

-- Multi-key joins where only ONE key is indexed: the index nested-loop
-- join enforces that key, every other ON conjunct must still filter
-- (the second key used to be dropped silently).
CREATE TABLE mk1(a INTEGER, b INTEGER);
CREATE TABLE mk2(x INTEGER, y INTEGER, z);
CREATE INDEX mk2x ON mk2(x);
INSERT INTO mk1 VALUES(1,1),(2,2),(3,NULL);
INSERT INTO mk2 VALUES(1,1,'ok'),(1,9,'BAD'),(2,2,'ok'),(2,8,'BAD'),(3,NULL,'nullkey'),(3,3,'BAD');
SELECT z FROM mk1 JOIN mk2 ON mk1.a = mk2.x AND mk1.b = mk2.y WHERE mk1.a > 0;
SELECT z FROM mk1, mk2 WHERE mk1.a = mk2.x AND mk1.b = mk2.y AND mk1.a > 0;
SELECT z FROM mk1 JOIN mk2 ON mk1.a = mk2.x AND mk1.b + 0 = mk2.y WHERE mk1.a > 0;
SELECT mk2.x FROM mk1 JOIN mk2 ON mk2.x = mk1.a AND mk2.y = mk1.b WHERE mk1.a > 0;
SELECT mk2.x FROM mk2 JOIN mk1 ON mk2.x = mk1.a AND mk2.y = mk1.b WHERE mk1.a > 0;
SELECT z FROM mk1 JOIN mk2 ON mk1.a = mk2.x AND mk2.z <> 'BAD' WHERE mk1.a < 3;
SELECT mk1.a, z FROM mk1 LEFT JOIN mk2 ON mk1.a = mk2.x AND mk1.b = mk2.y WHERE mk1.a > 0;
SELECT count(*) FROM mk1 JOIN mk2 ON mk1.a = mk2.x AND mk1.b IS mk2.y WHERE mk1.a > 0;

-- Compiled join conditions: nested comparisons and literal operands need
-- comparison affinity (they go to the evaluator); AND / OR are Kleene.
CREATE TABLE jc1(a INTEGER, c REAL, s TEXT);
CREATE TABLE jc2(h TEXT COLLATE NOCASE, h2 TEXT, n INTEGER);
INSERT INTO jc1 VALUES(5, 5.0, '5'),(0, -2.5, 'x'),(NULL, 1, NULL);
INSERT INTO jc2 VALUES('-45.81','-45.81', 5),('','', NULL),('-9223372036854775808','-9223372036854775808', 0);
SELECT a, h FROM jc1, jc2 WHERE (('abc' > c) > (a < h));
SELECT a, h FROM jc1, jc2 WHERE (1 > (a < h));
SELECT a, h FROM jc1, jc2 WHERE (a < h) = 0;
SELECT a FROM jc1, jc2 WHERE (1 > (a < h2));
SELECT a, n FROM jc1 LEFT JOIN jc2 ON jc1.a = jc2.n AND jc1.s > '4';
SELECT a, n FROM jc1 LEFT JOIN jc2 ON jc1.a = jc2.n AND jc2.h2 = 5;
SELECT a, n FROM jc1 JOIN jc2 ON NOT (jc1.a = jc2.n AND jc2.n > 1);
SELECT a, n FROM jc1 JOIN jc2 ON NOT (jc1.a = jc2.n OR jc2.n IS NULL);
-- An index join may only use an index whose collation is the key
-- comparison's (the left column's — BINARY here — decides).
CREATE TABLE ic1(a, b TEXT);
CREATE TABLE ic2(id INTEGER PRIMARY KEY, h TEXT COLLATE NOCASE);
CREATE INDEX ic2_h ON ic2(h);
INSERT INTO ic1 VALUES(1, 'abc'),(2, 'X');
INSERT INTO ic2(h) VALUES('ABC'),('abc'),('x');
SELECT a, id FROM ic1 INNER JOIN ic2 ON ic1.b = ic2.h WHERE a > 0;
SELECT a, id FROM ic1 INNER JOIN ic2 ON ic2.h = ic1.b WHERE a > 0;
SELECT a, id FROM ic1, ic2 WHERE ic1.b = ic2.h AND a > 0;
SELECT a, id FROM ic1 JOIN ic2 ON ic1.b = ic2.h COLLATE NOCASE WHERE a > 0;
