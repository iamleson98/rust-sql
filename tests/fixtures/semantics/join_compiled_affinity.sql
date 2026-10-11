-- Join conditions compiled with their comparison affinity: a term that
-- compares a column with a column of another affinity, a literal, a
-- parameter-free expression, or a CAST applies SQLite's comparison
-- affinity (sqlite3CompareAffinity) to the operand VALUES per pair.
-- These shapes used to run through the general evaluator; the small
-- tables take the nested loop, the 80-row ones (6400 pairs) the keyed
-- equality path.
CREATE TABLE p(id INTEGER PRIMARY KEY, n INTEGER, r REAL, t TEXT, c TEXT COLLATE NOCASE, u, bl BLOB);
CREATE TABLE q(id INTEGER PRIMARY KEY, n INTEGER, r REAL, t TEXT, c TEXT COLLATE NOCASE, u, bl BLOB);
INSERT INTO p VALUES (1, 5, 5.0, '5', 'abc', 5, x'35');
INSERT INTO p VALUES (2, 10, 2.5, ' 5', 'ABC', '5', x'00');
INSERT INTO p VALUES (3, -3, -3.0, '5.0', 'Abd', 5.0, NULL);
INSERT INTO p VALUES (4, NULL, NULL, 'abc', NULL, NULL, x'');
INSERT INTO p VALUES (5, 0, 0.0, '0x10', '10', 'abc', x'3130');
INSERT INTO p VALUES (6, 16, 1e20, '1e20', '1E20', x'3136', x'3136');
INSERT INTO p VALUES (7, 9223372036854775807, 9.2233720368547758e18, '9223372036854775807', '-0', -0.0, x'2d30');
INSERT INTO q VALUES (1, 5, 5.5, '5', 'ABC', '5.0', x'35');
INSERT INTO q VALUES (2, 4, 10.0, '10', 'abd', 10, NULL);
INSERT INTO q VALUES (3, NULL, -3.0, '-3', 'x', NULL, x'');
INSERT INTO q VALUES (4, 1, 1e20, '1e20', '1E20', '16', x'3136');
INSERT INTO q VALUES (5, 0, 0.0, '', '', '', x'00');
INSERT INTO q VALUES (6, 17, 4.5, 'abc', 'ABD', x'35', x'35');
-- column vs column, cross affinity, every operator
SELECT p.id, q.id FROM p JOIN q ON p.n = q.t;
SELECT p.id, q.id FROM p JOIN q ON p.n < q.t;
SELECT p.id, q.id FROM p JOIN q ON p.n >= q.t;
SELECT p.id, q.id FROM p JOIN q ON p.t <> q.n;
SELECT p.id, q.id FROM p JOIN q ON p.r > q.t;
SELECT p.id, q.id FROM p JOIN q ON p.t <= q.r;
SELECT p.id, q.id FROM p JOIN q ON p.n = q.u;
SELECT p.id, q.id FROM p JOIN q ON p.u = q.n;
SELECT p.id, q.id FROM p JOIN q ON p.t = q.u;
SELECT p.id, q.id FROM p JOIN q ON p.u < q.t;
SELECT p.id, q.id FROM p JOIN q ON p.u = q.u;
SELECT p.id, q.id FROM p JOIN q ON p.bl = q.t;
SELECT p.id, q.id FROM p JOIN q ON p.bl < q.n;
SELECT p.id, q.id FROM p JOIN q ON p.c = q.n;
SELECT p.id, q.id FROM p JOIN q ON p.c = q.t;
SELECT p.id, q.id FROM p JOIN q ON q.c > p.t;
-- column vs expression / literal / CAST
SELECT p.id, q.id FROM p JOIN q ON q.n = p.n + 0;
SELECT p.id, q.id FROM p JOIN q ON q.t = p.n || '';
SELECT p.id, q.id FROM p JOIN q ON q.t < p.n * 2;
SELECT p.id, q.id FROM p JOIN q ON p.t = q.n + 1 - 1;
SELECT p.id, q.id FROM p JOIN q ON q.u = p.r - 0.0;
SELECT p.id, q.id FROM p JOIN q ON q.c = p.t || 'C';
SELECT p.id, q.id FROM p JOIN q ON CAST(p.t AS INTEGER) = q.t;
SELECT p.id, q.id FROM p JOIN q ON p.u = CAST(q.t AS REAL);
SELECT p.id, q.id FROM p JOIN q ON CAST(p.n AS TEXT) = q.u;
SELECT p.id, q.id FROM p JOIN q ON p.id = q.id AND p.t > 4;
SELECT p.id, q.id FROM p JOIN q ON p.id = q.id AND q.n < '5';
SELECT p.id, q.id FROM p JOIN q ON p.id <= q.id AND p.u = '5';
SELECT p.id, q.id FROM p JOIN q ON p.id <= q.id AND q.t = 10;
SELECT p.id, q.id FROM p JOIN q ON p.id < q.id AND p.c = 'ABC';
-- rowid spellings
SELECT p.id, q.id FROM p JOIN q ON p.rowid = q.t;
SELECT p.id, q.id FROM p JOIN q ON q.t > p.oid;
SELECT p.id, q.id FROM p JOIN q ON p._rowid_ + 0 = q.u;
-- outer joins, NOT, OR
SELECT p.id, q.id FROM p LEFT JOIN q ON p.n = q.t;
SELECT p.id, q.id FROM p LEFT JOIN q ON p.t < q.n AND q.c <> p.c;
SELECT p.id, q.id FROM p JOIN q ON NOT (p.n = q.t);
SELECT p.id, q.id FROM p JOIN q ON p.n = q.t OR p.t = q.n;
SELECT p.id, q.id FROM p JOIN q ON (p.r < q.t) = (q.n < p.t);
-- the keyed equality path (6400 pairs)
CREATE TABLE ka(id INTEGER PRIMARY KEY, n INTEGER, t TEXT, u);
CREATE TABLE kb(id INTEGER PRIMARY KEY, n INTEGER, t TEXT, u, c TEXT COLLATE NOCASE);
WITH RECURSIVE s(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM s WHERE i < 80)
INSERT INTO ka SELECT i, i % 11,
  CASE i % 4 WHEN 0 THEN CAST(i % 9 AS TEXT) WHEN 1 THEN (i % 9) || '.0' WHEN 2 THEN ' ' || (i % 9) ELSE 'v' || (i % 5) END,
  CASE i % 3 WHEN 0 THEN i % 7 WHEN 1 THEN CAST(i % 7 AS TEXT) ELSE (i % 7) * 1.0 END
FROM s;
WITH RECURSIVE s(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM s WHERE i < 80)
INSERT INTO kb SELECT i, i % 9,
  CASE i % 3 WHEN 0 THEN CAST(i % 11 AS TEXT) WHEN 1 THEN 'V' || (i % 5) ELSE (i % 11) || '.0' END,
  CASE i % 4 WHEN 0 THEN CAST(i % 7 AS TEXT) || '.0' WHEN 1 THEN i % 7 WHEN 2 THEN NULL ELSE 'x' END,
  CASE i % 2 WHEN 0 THEN 'v' || (i % 5) ELSE CAST(i % 9 AS TEXT) END
FROM s;
SELECT count(*), sum(ka.id * kb.id) FROM ka JOIN kb ON kb.n = ka.n + 1;
SELECT count(*), sum(ka.id * kb.id) FROM ka JOIN kb ON kb.t = ka.n;
SELECT count(*), sum(ka.id * kb.id) FROM ka JOIN kb ON ka.t = kb.n;
SELECT count(*), sum(ka.id * kb.id) FROM ka JOIN kb ON ka.u = kb.u;
SELECT count(*), sum(ka.id * kb.id) FROM ka JOIN kb ON ka.u = kb.t;
SELECT count(*), sum(ka.id * kb.id) FROM ka JOIN kb ON kb.c = ka.t;
SELECT count(*), sum(ka.id * kb.id) FROM ka JOIN kb ON kb.c = ka.n;
SELECT count(*), sum(ka.id * kb.id) FROM ka JOIN kb ON kb.t = 'V' || (ka.n % 5);
SELECT count(*), sum(ka.id * kb.id) FROM ka JOIN kb ON kb.u = ka.n * 1.0 AND ka.t < kb.n;
SELECT count(*), sum(ka.id * coalesce(kb.id, 0)) FROM ka LEFT JOIN kb ON kb.t = ka.u + 0;
