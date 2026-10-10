-- SQLite stores TEXT bytes as given: CAST(x'c3' AS TEXT), x'c3' || x'a9',
-- a C caller's bind_text of Latin-1 bytes. The engine used to replace
-- invalid UTF-8 with U+FFFD (changing stored data); it now keeps the
-- bytes through storage, index keys, comparison (memcmp), ||, CAST,
-- length / substr (SQLite's byte stepping), upper / lower, quote / hex,
-- instr, NOCASE / RTRIM, group_concat and GROUP BY / DISTINCT.
CREATE TABLE t(id INTEGER PRIMARY KEY, x TEXT, y);
CREATE INDEX t_x ON t(x);
INSERT INTO t VALUES (1, CAST(x'c3' AS TEXT), x'c3');
INSERT INTO t VALUES (2, CAST(x'61ff62' AS TEXT), CAST(x'ff' AS TEXT));
INSERT INTO t VALUES (3, x'c3' || x'a9', 'é');
INSERT INTO t VALUES (4, CAST(x'41c3' AS TEXT), CAST(x'41C3' AS TEXT));
INSERT INTO t VALUES (5, 'plain', CAST(x'80' AS TEXT));
INSERT INTO t VALUES (6, CAST(x'c3' AS TEXT), CAST(x'61c3' AS TEXT));
SELECT id, typeof(x), hex(x), length(x), octet_length(x), hex(quote(x)) FROM t ORDER BY id;
SELECT id, hex(CAST(x AS BLOB)), hex(x || x'a9'), hex(upper(x)), hex(lower(x)) FROM t ORDER BY id;
SELECT id, hex(substr(x, 1, 1)), hex(substr(x, 2)), hex(substr(x, -1)), length(substr(x, 2)) FROM t ORDER BY id;
SELECT id, hex(x) FROM t ORDER BY x, id;
SELECT id, hex(x) FROM t ORDER BY x DESC, id;
SELECT count(*) FROM t WHERE x = CAST(x'c3' AS TEXT);
SELECT id FROM t WHERE x = CAST(x'61ff62' AS TEXT);
SELECT id FROM t NOT INDEXED WHERE x = CAST(x'61ff62' AS TEXT);
SELECT id FROM t WHERE x > CAST(x'61' AS TEXT) ORDER BY id;
SELECT id FROM t WHERE x IN (CAST(x'c3' AS TEXT), 'plain') ORDER BY id;
SELECT hex(group_concat(x, '|')) FROM (SELECT x FROM t ORDER BY id);
SELECT hex(group_concat(y)) FROM (SELECT y FROM t ORDER BY id);
SELECT count(DISTINCT x), count(DISTINCT y) FROM t;
SELECT id FROM t WHERE x = y ORDER BY id;
SELECT id, x = y COLLATE NOCASE, x < y COLLATE NOCASE, x = y COLLATE RTRIM FROM t ORDER BY id;
SELECT id, instr(x, CAST(x'ff' AS TEXT)), instr(x, 'b'), instr(y, x'c3') FROM t ORDER BY id;
SELECT hex(CAST(x'c3' AS TEXT) || CAST(x'a9' AS TEXT)), typeof(CAST(x'c3' AS TEXT) || 1), hex(CAST(x'c3' AS TEXT) || 1);
SELECT length(CAST(x'80808041' AS TEXT)), length(CAST(x'c3808080' AS TEXT)), length(CAST(x'41c3' AS TEXT)), length(CAST(x'41c30042' AS TEXT));
SELECT hex(substr(CAST(x'41c3a9ff42' AS TEXT), 2, 2)), hex(substr(CAST(x'80c3a9' AS TEXT), 1, 1)), hex(substr(CAST(x'c3a9' AS TEXT), 2));
UPDATE t SET x = x || CAST(x'ff' AS TEXT) WHERE id = 5;
SELECT id, hex(x) FROM t ORDER BY id;
SELECT hex(x), count(*) FROM t GROUP BY x ORDER BY x;
SELECT hex(max(x)), hex(min(x)), hex(max(y)), hex(min(y)) FROM t;
SELECT id, hex(x) FROM t WHERE x LIKE 'a%' ORDER BY id;
CREATE TABLE u(k TEXT COLLATE NOCASE PRIMARY KEY, v) WITHOUT ROWID;
INSERT INTO u VALUES (CAST(x'41ff' AS TEXT), 1);
INSERT OR IGNORE INTO u VALUES (CAST(x'61ff' AS TEXT), 2);
INSERT OR IGNORE INTO u VALUES (CAST(x'61fe' AS TEXT), 3);
SELECT hex(k), v FROM u ORDER BY k;
SELECT v FROM u WHERE k = CAST(x'61FF' AS TEXT);
DELETE FROM t WHERE x = CAST(x'c3' AS TEXT);
SELECT id, hex(x) FROM t ORDER BY id;
SELECT hex(trim(char(9) || ' a ' || char(10))), hex(ltrim('  a  ')), hex(rtrim('  a  ')), trim('xxaxx', 'x');
SELECT trim('abc', NULL), ltrim(NULL, 'a'), hex(trim('éaé', 'é')), hex(trim(CAST(x'a9a941a9' AS TEXT), CAST(x'a9' AS TEXT)));
SELECT hex(ltrim(x'a9a941')), hex(rtrim(x'412020')), hex(trim(x'c3a9', x'a9')), typeof(trim(x'41'));
SELECT hex(trim('ab' || char(0) || 'ba', 'ab')), hex(trim('aXb', 'ab' || char(0) || 'X')), trim(12.50, '0'), trim(1200, '0');
SELECT hex(replace(CAST(x'41ff41' AS TEXT), 'A', 'é')), hex(replace('aéa', CAST(x'a9' AS TEXT), 'x')), hex(replace(x'c3a9c3', x'c3', 'Z')), replace('aaa', 'aa', 'b');
