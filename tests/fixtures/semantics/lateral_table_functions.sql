-- Lateral table-valued functions (json106.test): `FROM t, json_each(t.j)` runs
-- the function once per left row (it used to see NULL arguments and return
-- nothing); json_each / json_tree rows have a 0-based rowid; `e.*` lists only
-- the function's columns.
CREATE TABLE t1(id INTEGER PRIMARY KEY, j);
INSERT INTO t1 VALUES(1, '[10,20]'), (2, '{"a":1,"b":[2,3]}');
SELECT t1.id, e.key, e.value FROM t1, json_each(t1.j) AS e;
SELECT t1.id, e.key, e.value FROM t1 JOIN json_each(t1.j) e;
SELECT count(*) FROM t1, json_tree(j) AS rt WHERE rt.type NOT IN ('object','array');
SELECT rt.rowid, rt.id, rt.fullkey FROM t1, json_tree(j) AS rt WHERE t1.id = 2;
SELECT e.rowid, e.id FROM json_each('[5,6]') e;
SELECT id, (SELECT sum(value) FROM json_each(t1.j)) FROM t1 WHERE id = 1;
SELECT t1.id, e.value FROM t1 LEFT JOIN json_each(t1.j) e ON e.value > 15;
CREATE TABLE t2(k, v);
INSERT INTO t2 VALUES(0, 'zero'), (1, 'one'), ('a', 'A');
SELECT t1.id, e.value, t2.v FROM t1, json_each(t1.j) e, t2 WHERE t2.k = e.key;
SELECT t1.id, e.value, f.value FROM t1, json_each(t1.j) e, json_each(e.value) f WHERE e.type = 'array';
SELECT id, (SELECT count(*) FROM json_each(t1.j)) FROM t1;
SELECT t1.id, e.key FROM t1 JOIN json_each(t1.j) e ON e.key = t1.id - 1;
SELECT t1.id, e.key FROM t1 LEFT JOIN json_each(t1.j) e WHERE e.key IS NULL;
SELECT DISTINCT e.type FROM t1, json_tree(t1.j) e;
SELECT t1.id, max(e.value) FROM t1, json_each(t1.j) e GROUP BY t1.id;
SELECT e.* FROM t1, json_each(t1.j) e WHERE t1.id = 1;
SELECT * FROM t1, json_each(t1.j) e WHERE t1.id = 1;
SELECT t1.id, e.value FROM t1, json_each(t1.j) e ORDER BY e.value DESC LIMIT 3;
SELECT count(*) FROM t1 a, t1 b, json_each(b.j) e WHERE a.id = b.id;
