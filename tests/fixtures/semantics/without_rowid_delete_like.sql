-- A WITHOUT ROWID DELETE whose WHERE takes the generic delete route (a LIKE
-- prefix over the PRIMARY KEY, with or without RETURNING) failed as
-- "unsupported": the route expected each row's rowid; the engine-internal PK
-- index now maps the row to its storage rowid (strict-fuzz seed 683935).
CREATE TABLE t3(j TEXT PRIMARY KEY, k INTEGER, l) WITHOUT ROWID;
CREATE INDEX t3_k ON t3(k);
INSERT INTO t3 VALUES ('+7', 1, NULL), ('-0', 2, 0), ('1a', 3, 1), ('12', 4, 2), ('1', 5, 3), ('2', 6, 4), ('10', 9223372036854775806, 5);
DELETE FROM t3 WHERE ((j LIKE '1%') AND printf('%5s', k));
SELECT * FROM t3 ORDER BY j;
DELETE FROM t3 WHERE j LIKE '+%' RETURNING j, k;
SELECT * FROM t3 ORDER BY j;
CREATE TABLE g(a INT, b TEXT COLLATE NOCASE, c, PRIMARY KEY (a, b)) WITHOUT ROWID;
INSERT INTO g VALUES (1, 'ab', 1), (1, 'AC', 2), (2, 'ad', 3), (2, 'x', 4);
DELETE FROM g WHERE b LIKE 'a%' AND c > 1;
SELECT * FROM g ORDER BY a, b;
PRAGMA integrity_check;
