-- sum / total / avg classify each argument like sumStep
-- (sqlite3_value_numeric_type): numeric text stops at an embedded NUL.
SELECT sum('0' || x'00410042'), typeof(sum('0' || x'00410042'));
SELECT sum('0' || x'00'), sum('1' || x'0041'), sum(x'3000'), sum(CAST(x'3100' AS TEXT));
SELECT '0' || x'00410042' + 0, typeof('1' || x'0041' + 0), ('7' || x'0041') * 1;
SELECT total('3' || x'0041'), avg('3' || x'0041'), typeof(avg('3' || x'0041'));
SELECT CAST('3' || x'0041' AS NUMERIC), typeof(CAST('3' || x'0041' AS NUMERIC)), CAST('3' || x'0041' AS INTEGER), CAST('3' || x'0041' AS REAL);
SELECT ('3' || x'0041') = 3, ('3' || x'0041') > 2, abs('3' || x'0041');
CREATE TABLE n(a INTEGER, r REAL, m NUMERIC);
INSERT INTO n VALUES ('3' || x'0041', '3' || x'0041', '3' || x'0041');
SELECT typeof(a), typeof(r), typeof(m), hex(a) FROM n;
SELECT sum(4294967296 || ('-1' || '-1')), sum('4294967296-1-1'), sum('12-3');
SELECT sum('4294967296' || NULL), sum('1e3x'), sum(' 5 '), sum('5x');
SELECT avg('1' || x'0041'), typeof(avg('1' || x'0041')), sum(' +5 '), sum('9223372036854775807'), sum('9223372036854775808'), typeof(sum('5.0')), sum('0x10'), typeof(sum('0x10'));
SELECT sum(char(160) || '5'), typeof(sum(char(160) || '5')), sum('5' || char(9)), typeof(sum('5' || char(9)));
SELECT sum(x) OVER (ORDER BY y), typeof(sum(x) OVER (ORDER BY y)) FROM (SELECT '2' || x'00' AS x, 1 AS y UNION ALL SELECT '3', 2);
SELECT sum(v), avg(v), total(v), typeof(sum(v)) FROM (SELECT '4' || x'0041' AS v UNION ALL SELECT '6' || x'00');
