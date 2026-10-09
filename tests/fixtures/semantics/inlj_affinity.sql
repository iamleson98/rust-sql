-- Index nested-loop joins follow sqlite3IndexAffinityOk: a column-vs-column
-- equality is NUMERIC when either side has numeric affinity. A NUMERIC key
-- index is probed with the outer value after NUMERIC affinity ('0' -> 0); a
-- TEXT / untyped key index cannot serve a NUMERIC comparison (the join
-- falls back to a comparison that applies the affinities).
CREATE TABLE o(id INTEGER PRIMARY KEY, t TEXT, n INTEGER, r REAL, u, w NUMERIC);
CREATE TABLE i(id INTEGER PRIMARY KEY, n INTEGER, t TEXT, u, w NUMERIC);
CREATE INDEX i_n ON i(n);
CREATE INDEX i_t ON i(t);
CREATE INDEX i_u ON i(u);
CREATE INDEX i_w ON i(w);
INSERT INTO o VALUES (1, '0', 0, 0.0, '0', 0), (2, '1.5e-07', 7, 1.5e-07, 7, 1.5e-07),
  (3, ' 12', 12, 12.0, x'3132', 12), (4, 'abc', -3, -3.25, 'abc', 'abc'), (5, '7.0', 7, 7.0, 7.0, 7);
INSERT INTO i(n, t, u, w) VALUES (0, '0', '0', 0), (7, '7', 7, 7), (1.5e-07, '1.5e-07', '1.5e-07', 1.5e-07),
  ('abc', 'abc', 'abc', 'abc'), (12, '12', 12, 12), (-3.25, '-3.25', -3.25, -3.25);
SELECT o.id, i.id FROM o JOIN i ON i.n = o.t WHERE o.id > 0 ORDER BY 1, 2;
SELECT o.id, i.id FROM o JOIN i ON o.t = i.n WHERE o.id > 0 ORDER BY 1, 2;
SELECT o.id, i.id FROM o JOIN i ON i.t = o.n WHERE o.id > 0 ORDER BY 1, 2;
SELECT o.id, i.id FROM o JOIN i ON i.t = o.r WHERE o.id > 0 ORDER BY 1, 2;
SELECT o.id, i.id FROM o JOIN i ON i.u = o.n WHERE o.id > 0 ORDER BY 1, 2;
SELECT o.id, i.id FROM o JOIN i ON i.u = o.t WHERE o.id > 0 ORDER BY 1, 2;
SELECT o.id, i.id FROM o JOIN i ON i.n = o.u WHERE o.id > 0 ORDER BY 1, 2;
SELECT o.id, i.id FROM o JOIN i ON i.w = o.t WHERE o.id > 0 ORDER BY 1, 2;
SELECT o.id, i.id FROM o JOIN i ON i.t = o.u WHERE o.id > 0 ORDER BY 1, 2;
SELECT o.id, i.id FROM o JOIN i ON i.n = o.r WHERE o.id > 0 ORDER BY 1, 2;
SELECT o.id, i.id FROM i JOIN o ON o.t = i.n WHERE i.id > 0 ORDER BY 1, 2;
SELECT o.id, i.id FROM o LEFT JOIN i ON i.n = o.t WHERE o.id > 1 ORDER BY 1, 2;
