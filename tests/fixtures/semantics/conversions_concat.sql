SELECT typeof(x'41' || x'42'), typeof('a' || x'42'), typeof(1 || 2), x'41' || x'42';
SELECT typeof(CAST('3' || x'00410042' AS INTEGER)), CAST('3' || x'00410042' AS INTEGER), '3' || x'00410042' + 0;
CREATE TABLE ti(g INTEGER, b TEXT, e BLOB);
INSERT INTO ti VALUES ('3' || x'00410042', '1e3', x'c3a9');
SELECT typeof(g), g, length(g) FROM ti;
CREATE TABLE s(g INTEGER);
INSERT INTO s VALUES (1), (NULL), (9223372036854775807), (3);
SELECT 1 FROM ti WHERE b;
CREATE TABLE tb(b TEXT);
INSERT INTO tb VALUES ('-1.0e+308'), ('1e400'), (' 12'), ('abc'), ('0.0'), ('-0'), ('5.'), ('.5'), ('0x1F');
SELECT b FROM tb WHERE b;
SELECT b, b AND 1, NOT b, CASE WHEN b THEN 1 ELSE 0 END FROM tb;
SELECT -21 % '9.2233720368547758e+18', -21 % ' 1e20', 7 % '2.5x', 5.5 % '2';
SELECT quote(1e999), quote(-1e999), CAST(1e999 AS TEXT), printf('%!0.15g', 1e999);
CREATE TABLE o(g);
INSERT INTO o VALUES (1), (5), (NULL), ('.5'), (-3), (12);
SELECT (-3 IS NOT NULL), (ltrim(g) - '.5') FROM o ORDER BY 1 DESC, 2 LIMIT 4 OFFSET 1;
SELECT (-3 IS NOT NULL), (ltrim(g) - '.5') FROM o ORDER BY 1 DESC, 2;
SELECT x FROM (SELECT 1 AS x UNION SELECT 2) ORDER BY 1 LIMIT 1 OFFSET 1;

-- concat_ws: the separator precedes every non-NULL item but the first,
-- empty strings included.
SELECT concat_ws(',', '', 'a', '', NULL, 'b'), concat_ws(',', NULL, NULL), concat_ws(NULL, 'a');
SELECT concat_ws(',', 1, 2.5, x'41'), concat_ws('', 'a', 'b'), concat_ws(',', ''), concat_ws(',', '', '');
-- substr() over TEXT stops at the first NUL byte (BLOBs do not).
SELECT substr('ab' || char(0) || 'cd', 2), length(substr('ab' || char(0) || 'cd', 2)), substr('ab' || char(0) || 'cd', 4);
SELECT hex(substr('ab' || char(0) || 'cd', 1, 4)), hex(substr(x'0102000304', 2, 3)), substr('ab' || char(0) || 'cd', -2);

-- Parse-time folds SQLite performs: `<literal> IS NULL` / NOTNULL /
-- `NOT NULL` become integer constants, `X AND 0` is the literal 0 (the
-- other side never evaluates) — both then read as ORDINALS in GROUP BY /
-- ORDER BY.
-- (Known residual, not pinned here: `SELECT (abs(-9223372036854775808) AND 0)`
-- is 0 in SQLite — its value-context AND skips an erroring left operand
-- next to a constant 0 — while rustqlite evaluates left-to-right and
-- raises; see README "Remaining gaps".)
SELECT 'É' IS NULL, 5 IS NOT NULL, -3 ISNULL, x'00' NOTNULL, (1 AND 0), (0 AND (1/0)), NULL IS NULL, (FALSE AND abs(-9223372036854775808)), -1.5 NOT NULL;
CREATE TABLE fold(a, b NOT NULL DEFAULT 1);
INSERT INTO fold(a) VALUES(1),(NULL);
SELECT a NOT NULL, a NOT NULL AND 1, NOT a NOT NULL, a NOT IN (1), a NOT BETWEEN 0 AND 0 FROM fold;
SELECT a FROM fold GROUP BY ('x' IS NULL);
SELECT a FROM fold GROUP BY (a AND 0);
SELECT a FROM fold ORDER BY ('x' IS NULL);
SELECT a FROM fold ORDER BY (a AND 0);
SELECT count(*) FROM fold GROUP BY (0 AND NULL) COLLATE BINARY;
SELECT a FROM fold WHERE a AND 0;
-- replace(): NULL string/pattern -> NULL; EMPTY pattern -> the string,
-- before the replacement is looked at.
SELECT replace(x'41', '', NULL), replace('abc', NULL, 'x'), replace('abc', 'b', NULL), replace(NULL, 'b', 'x'), replace('abc', '', 'x'), replace(5, '', 6), typeof(replace(5, '', 6));
-- CAST AS NUMERIC: sqlite3VdbeMemNumerify (exact integers past 2^53).
SELECT CAST('-9223372036854775807' AS NUMERIC), CAST('9223372036854775807' AS NUMERIC), CAST('9223372036854775808' AS NUMERIC), CAST('-9223372036854775808' AS NUMERIC);
SELECT CAST('12abc' AS NUMERIC), CAST('12.0' AS NUMERIC), CAST('1.5' AS NUMERIC), CAST('1e5' AS NUMERIC), CAST('' AS NUMERIC), CAST(' 3.0 ' AS NUMERIC), CAST(x'3132' AS NUMERIC);
SELECT CAST('abc' AS NUMERIC), CAST('-' AS NUMERIC), CAST('.5' AS NUMERIC), CAST('5.' AS NUMERIC), CAST('1e400' AS NUMERIC), CAST('0x10' AS NUMERIC), CAST('4503599627370497.0' AS NUMERIC), CAST('2251799813685248.0' AS NUMERIC), CAST('+7' AS DECIMAL);
