SELECT 1 / 0.5, 7 / 2, 7.0 / 2, '1.5' + 1, '12abc' + 0, ' 12 ' + 0, '1e2' + 0, x'3132' + 0, x'41' + 0;
SELECT 5.5 % 2, -7 % 3, 7 % -3, 7 % 0.5, '7' % '3', 7 % 2.5;
SELECT 1 << 64, 1 << 63, 1 << -1, 8 >> -1, -8 >> 70, 8 >> 64, 1 << 62.9, '2' << '3';
SELECT ~5, ~'5', ~5.7, ~x'31', -'5', -'abc', -x'35', +'5', -'-9223372036854775808';
SELECT 9223372036854775807 + 1, -9223372036854775808 - 1, 9223372036854775807 * 2, -9223372036854775808 / -1, -9223372036854775808 % -1;
SELECT '9223372036854775807' + 0, '9223372036854775808' + 0, 1e308 * 10, -1e308 * 10, 0.0 / 0;
SELECT x'41' AND 1, CASE WHEN x'41' THEN 1 ELSE 0 END, NOT x'31', NOT x'41', x'00' OR 0;
SELECT 3 & '6', 3 | 4.9, 5 & x'37', 'abc' | 0, 2.5 & 3;
SELECT 'Inf' + 0, '-inf' * 1, 'nan' + 0, CAST('inf' AS REAL), CAST('1e400' AS REAL), CAST('-1e400' AS REAL);
SELECT CAST('12abc' AS INTEGER), CAST(' 12' AS INTEGER), CAST('1.9' AS INTEGER), CAST('-1.9e1' AS INTEGER), CAST(x'3132' AS INTEGER), CAST(1e19 AS INTEGER), CAST(-1e19 AS INTEGER), CAST('9223372036854775808' AS INTEGER);
SELECT CAST('1.0' AS NUMERIC), CAST('1e3' AS NUMERIC), CAST('abc' AS NUMERIC), CAST('12abc' AS NUMERIC), CAST(' 3.5 ' AS NUMERIC), CAST(x'3132' AS NUMERIC), CAST('9223372036854775808' AS NUMERIC), CAST('0x10' AS NUMERIC);
SELECT CAST(1.0 AS TEXT), CAST(1e20 AS TEXT), CAST(0.1 AS TEXT), CAST(-0.0 AS TEXT), CAST(1.5e-7 AS TEXT), CAST(123456789012345678 AS REAL);
SELECT 1 = 1.0, '1' = 1, 1 < '1', x'31' > 'z', NULL = NULL, 1 IS 1.0, 'a' < 'B', 'a' < 'b' COLLATE NOCASE;
SELECT abs(-9223372036854775808);
SELECT abs('-5'), abs('abc'), abs(x'2d35'), abs(-0.0), abs(NULL);
SELECT round(2.5), round(-2.5), round(0.5), round(1.005, 2), round(2.675, 2), round('3.7'), round(1e20), round(123.456, -1), round(5, 2);
SELECT length(123), length(1.5), length(x'00ff'), length('héllo'), length(NULL), octet_length('héllo'), length(-0.0);
SELECT upper('ß'), lower('É'), upper('é'), lower('ÀB');
SELECT substr('hello', 0), substr('hello', -2), substr('hello', 2, -1), substr('hello', -10, 3), substr(x'010203', 2), substr('héllo', 2, 2), substr('abc', 0, 2);
SELECT instr('hello', 'l'), instr('héllo', 'l'), instr(x'0102', x'02'), instr('abc', ''), instr(NULL, 'a'), instr(12345, 34);
SELECT replace('aaa', 'a', 'bb'), replace('abc', '', 'x'), replace(123, 2, 9), replace('abc', 'b', NULL);
SELECT trim('  a  '), ltrim('xxaxx', 'x'), rtrim('xxaxx', 'x'), trim('abc', ''), trim(12321, 1);
SELECT hex(12), hex(-1), hex(1.5), hex('é'), hex(NULL), quote(1.5), quote('it''s'), quote(x'00ff'), quote(NULL), quote(1e100), quote(-0.0);
SELECT typeof(1), typeof(1.0), typeof('1'), typeof(x'01'), typeof(NULL), typeof(1/2), typeof(1/2.0), typeof('1' + 1), typeof('1.0' + 1);
SELECT min(1, '1', x'01'), max(1, '1', x'01'), min(1, NULL), max('a', 'B'), min(2.5, 2), max(1, 1.0);
SELECT coalesce(NULL, NULL), ifnull(NULL, 'x'), nullif(1, 1), nullif(1, 1.0), nullif('a', 'A'), iif(0, 1, 2), iif(NULL, 1, 2);
SELECT unicode('é'), unicode(''), char(233, 65), char(), unicode(NULL), char(NULL), char(-1);
SELECT printf('%d', 3.9), printf('%d', '12abc'), printf('%5.2f', 3.14159), printf('%s', NULL), printf('%x', -1), printf('%q', 'it''s'), printf('%Q', NULL), printf('%e', 12345.678), printf('%g', 0.0001), printf('%10s|', 'ab'), printf('%-10s|', 'ab'), printf('%c', 'abc'), printf('%%'), printf('%.3s', 'abcdef'), printf('%5d', -42), printf('%05d', 42), printf('%+d', 5), printf('%,d', 1234567);
SELECT 'a' LIKE 'A', 'é' LIKE 'É', 'abc' LIKE 'a_c', 'a%c' LIKE 'a\%c' ESCAPE '\', 'abc' GLOB 'a*', 'abc' GLOB 'A*', 'a' GLOB '[a-c]', 'b' GLOB '[^a]', 12 LIKE '1%', x'61' LIKE 'a', NULL LIKE 'a';
SELECT 5 BETWEEN 1 AND 10, '5' BETWEEN 1 AND 10, NULL BETWEEN 1 AND 2, 1 BETWEEN NULL AND 2, 3 BETWEEN NULL AND 2;
SELECT 1 IN (1.0, 2), '1' IN (1, 2), 1 IN ('1'), NULL IN (1), 1 IN (NULL, 1), 2 IN (NULL, 1), 2 NOT IN (NULL, 1), 1 IN ();
SELECT CASE 1 WHEN 1.0 THEN 'a' ELSE 'b' END, CASE '1' WHEN 1 THEN 'a' ELSE 'b' END, CASE NULL WHEN NULL THEN 'a' ELSE 'b' END, CASE WHEN 0.5 THEN 'a' ELSE 'b' END;
SELECT sign(-3), sign(0.0), sign('5'), sign('abc'), sign(NULL), sign(x'01');
SELECT concat(1, NULL, 'a'), concat_ws(',', 1, NULL, 2), concat_ws(NULL, 1, 2), concat();
SELECT 10 / 3.0, 1e15 + 0.3, 0.1 + 0.2, 1.0 / 3, 100.0, 1e16, 12345678901234567890, -0.0, 2.0 * 0.5;
SELECT substr(x'', 1), substr(x'', 5, 3), substr('', 1), substr(x'00', 1), typeof(substr(x'', 1)), substr(x'', 5, 3) & -40;

-- AND / OR short-circuit (the deciding first operand stops evaluation).
SELECT (0 AND abs(-9223372036854775808)), (1 OR abs(-9223372036854775808)), (NULL AND 0), (NULL OR 1), (NULL AND 1), (0 OR NULL), ('x' AND 1), (2 AND 3.5);
-- sum() / avg() / total() over integers past 2^52: SQLite's split
-- Kahan-Babuska-Neumaier steps.
CREATE TABLE bigsum(v);
INSERT INTO bigsum VALUES(5),('Inf'),(-9223372036854775808),('1'),('ABC'),(-3),(-3.5),(NULL),(9223372036854775807);
SELECT sum(v), total(v), avg(v) FROM bigsum;
SELECT sum(x) FROM (SELECT 9223372036854775807 x UNION ALL SELECT 1 UNION ALL SELECT 0.5 UNION ALL SELECT -9223372036854775807);
SELECT sum(v) OVER () FROM bigsum LIMIT 1;
-- replace(): a pattern whose text starts with NUL reads as empty in C.
SELECT replace(1e308, x'00', NULL), replace('abc', x'0062', 'z'), replace('abc', char(0), 'z');

-- Ordinals through nested unary operators (`- -3` is 3, `+ -2` is -2).
CREATE TABLE ordu(a, b);
INSERT INTO ordu VALUES(1,2),(1,3),(2,4);
SELECT a, count(*) FROM ordu GROUP BY (- -1);
SELECT a, count(*) FROM ordu GROUP BY (+ -1);
SELECT a, count(*), sum(b) FROM ordu GROUP BY (- -3);
/*ordered*/ SELECT a FROM ordu ORDER BY (- -1) DESC;
SELECT a FROM ordu ORDER BY (+ -1);
-- NaN results are NULL; an overflowed error term is not folded.
CREATE TABLE ovf(v);
INSERT INTO ovf VALUES(1e308),(1e308),(-1e308),(5);
SELECT sum(v), total(v), avg(v) FROM ovf;
-- LIKE / GLOB see C strings (stop at NUL); NOCASE compares up to a NUL
-- and then by length.
SELECT ('2' || char(0) || 'B') LIKE '%b%', ('2' || char(0) || 'B') GLOB '*B*';
SELECT ('a' || char(0) || 'x') = ('A' || char(0) || 'y') COLLATE NOCASE, ('a' || char(0) || 'x') = ('A' || char(0) || 'yy') COLLATE NOCASE, ('a' || char(0)) = 'a' COLLATE NOCASE;
SELECT count(DISTINCT x COLLATE NOCASE) FROM (SELECT char(0) || 'A' x UNION ALL SELECT char(0) || 'B' UNION ALL SELECT char(0) || 'BB');
-- substr(): the default length is SQLITE_LIMIT_LENGTH.
SELECT substr('1', '-34294967296'), substr('abc', -1000000001), substr('abc', -999999999), substr(x'0102', -5000000000);
-- `%needle%` LIKE over NON-text values (it matched nothing).
CREATE TABLE lk(a INTEGER, b);
INSERT INTO lk VALUES(15, 15),(25, 2.5),(7, x'3531');
SELECT a FROM lk WHERE a LIKE '%5%';
SELECT a FROM lk WHERE b LIKE '%5%';
SELECT a FROM lk WHERE b LIKE '%.%';
