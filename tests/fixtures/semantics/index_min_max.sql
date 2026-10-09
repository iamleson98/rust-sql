-- min(c) / max(c) read from an index on c (SQLite's min/max optimization):
-- the first / last non-NULL entry of the range whose row passes the residual;
-- the stored value comes back (5 vs 5.0), and among equal values the entry
-- SQLite's walk reaches first (lowest rowid for min, highest for max).
CREATE TABLE m(id INTEGER PRIMARY KEY, x, n TEXT COLLATE NOCASE, g INTEGER);
INSERT INTO m(x, n, g) VALUES (5, 'b', 1), (5.0, 'B', 2), (NULL, NULL, 1), (-9223372036854775808, 'a', 3), (-9.2233720368547758e18, 'A', 1), ('text', 'c', 2), (x'01', 'C', 3), (3.5, NULL, 1), (NULL, 'z', 2);
CREATE INDEX m_x ON m(x);
CREATE INDEX m_n ON m(n);
CREATE INDEX m_g_x ON m(g, x);
SELECT min(x), typeof(min(x)), max(x), typeof(max(x)) FROM m;
SELECT max(x), typeof(max(x)) FROM m WHERE x < 5;
SELECT max(x), typeof(max(x)) FROM m WHERE x <= 5;
SELECT min(x), typeof(min(x)) FROM m WHERE x > -1;
SELECT min(x), typeof(min(x)) FROM m WHERE x >= 5;
SELECT max(x) FROM m WHERE x < 5 AND g = 2;
SELECT max(x) FROM m WHERE x > 100 AND x < 1000;
SELECT min(x) FROM m WHERE x > 'text';
SELECT max(x) FROM m WHERE x < 'zzz';
SELECT min(n), max(n) FROM m;
SELECT min(n COLLATE BINARY), max(n COLLATE BINARY) FROM m;
SELECT max(n) FROM m WHERE n < 'c';
SELECT min(g), max(g) FROM m;
SELECT max(x) FROM m WHERE g = 1;
SELECT (SELECT max(x) FROM m WHERE x < 4), (SELECT min(x) FROM m);
SELECT max(x) + 1, min(x) IS NULL FROM m;
CREATE TABLE e(a INTEGER);
CREATE INDEX e_a ON e(a);
SELECT min(a), max(a), count(*) FROM e;
SELECT min(a) FROM e;
INSERT INTO e VALUES (NULL), (NULL);
SELECT min(a), max(a) FROM e;
SELECT max(a) FROM e WHERE a > 0;
DELETE FROM m WHERE id = 2;
SELECT max(x), typeof(max(x)), min(x) FROM m;
