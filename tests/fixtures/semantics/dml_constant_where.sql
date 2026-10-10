-- UPDATE / DELETE evaluate constant WHERE terms once, before any row is read
-- (sqlite3WhereBegin): a FALSE / NULL one ends the statement, so a raising
-- per-row term never runs; a raising constant term raises.
CREATE TABLE d(j TEXT PRIMARY KEY, k INTEGER) WITHOUT ROWID;
INSERT INTO d VALUES ('a', 1), ('b', -9223372036854775808), ('c', 3);
CREATE TABLE r(id INTEGER PRIMARY KEY, k INTEGER);
INSERT INTO r VALUES (1, 1), (2, -9223372036854775808), (3, 3);
DELETE FROM d WHERE (abs(k) AND '%');
SELECT count(*) FROM d;
DELETE FROM r WHERE abs(k) AND 0;
SELECT count(*) FROM r;
DELETE FROM r WHERE abs(k) AND (1 = 2);
SELECT count(*) FROM r;
UPDATE r SET k = 5 WHERE abs(k) AND NULL;
SELECT id, k FROM r ORDER BY id;
UPDATE d SET k = k + 1 WHERE abs(k) > 0 AND 'x' AND j = 'a';
SELECT j, k FROM d ORDER BY j;
DELETE FROM r WHERE id = 1 AND 1;
SELECT count(*) FROM r;
DELETE FROM r WHERE abs(-9223372036854775808) AND id = 99;
SELECT count(*) FROM r;
DELETE FROM r WHERE 0 AND abs(-9223372036854775808);
SELECT count(*) FROM r;
UPDATE r SET k = 7 WHERE (1 = 1) AND id = 3 RETURNING id, k;
DELETE FROM r WHERE (2 = 3) AND id = 3 RETURNING id;
SELECT id, k FROM r ORDER BY id;
