-- A TEXT rowid key converts by SQLite's own numeric parse (applyNumericAffinity):
-- its whitespace set, and an embedded NUL ends the text — `id <= ('-81' ||
-- x'00…')` is `id <= -81`, `id = ('1' || x'0041')` seeks rowid 1. A Rust parse
-- refused those texts, so the range matched every row and the seek nothing
-- (strict-fuzz seed 709220).
CREATE TABLE t2(id INTEGER PRIMARY KEY, g INTEGER, r REAL, x);
INSERT INTO t2 VALUES (1, 1, 1.5, 1), (2, -90, -90.5, '-90'), (-100, -100, -100.0, -100);
SELECT id FROM t2 WHERE id <= (-81 || x'00410042');
SELECT id FROM t2 WHERE g <= (-81 || x'00410042');
SELECT id FROM t2 WHERE r <= ('-81' || x'00');
SELECT id FROM t2 WHERE x <= (-81 || x'00410042');
SELECT id FROM t2 WHERE g = ('1' || x'0041');
SELECT id FROM t2 WHERE g IN ('1' || x'0041', 5);
SELECT typeof(CAST(('1' || x'0041') AS INTEGER)), CAST(('1' || x'0041') AS INTEGER);
SELECT ('5' || x'00') + 1, ('5' || x'0031') * 2;
SELECT ('7' || x'00') > 6, 7 = ('7' || x'00');
SELECT id FROM t2 WHERE id = ('1' || x'0041');
SELECT id FROM t2 WHERE id IN ('1' || x'0041', '-100' || x'00');
SELECT id FROM t2 WHERE id > (' 1 ' || x'00ff');
SELECT id FROM t2 WHERE id BETWEEN ('-200' || x'00') AND ('1.5' || x'00');
SELECT id FROM t2 WHERE id < '  2  ';
SELECT id FROM t2 WHERE id >= '0x10';
SELECT id FROM t2 WHERE id <= '1e1';
SELECT id FROM t2 WHERE id = ' 1 ';
SELECT id FROM t2 WHERE id = '1.0';
SELECT id FROM t2 WHERE id = '1e0';
SELECT id FROM t2 WHERE id = '0x1';
SELECT id FROM t2 WHERE id = ('-1' || '00');
SELECT id FROM t2 WHERE id IN ('1', ' -100', '2.5', 'abc');
INSERT INTO t2 (id, g) VALUES ('7' || x'0041', 0);
SELECT id, g FROM t2 WHERE id = 7;
INSERT INTO t2 (id, g) VALUES ('8.5', 0);
SELECT id FROM t2 WHERE id = (char(160) || '1');
