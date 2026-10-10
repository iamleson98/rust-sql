-- Joins on equalities the hash join cannot key by bare same-affinity
-- columns (`b.y = a.x + 1`, `a.n = b.t || ''`, cross-affinity `a.n =
-- b.t`) used to run a nested loop over every pair; they are now hashed
-- on a key that never separates values SQLite's `=` finds equal under
-- any built-in affinity / collation, and every candidate pair is
-- re-tested with the full condition. Tables are large enough
-- (>= 4096 pairs) for the keyed path; the special rows cover numeric
-- text, case and trailing-space variants, NULs, -0.0, 16-digit
-- integers, a REAL's 15-digit rendering, Inf and blobs.
CREATE TABLE a(id INTEGER PRIMARY KEY, n INTEGER, r REAL, t TEXT, tn TEXT COLLATE NOCASE, tr TEXT COLLATE RTRIM, u, bl BLOB);
CREATE TABLE b(id INTEGER PRIMARY KEY, n INTEGER, r REAL, t TEXT, u, bl BLOB);
WITH RECURSIVE s(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM s WHERE i < 80)
INSERT INTO a SELECT i, i % 17, i / 4.0,
  CASE i % 6 WHEN 0 THEN 'x' || i WHEN 1 THEN CAST(i % 13 AS TEXT) WHEN 2 THEN (i % 13) || '.0'
             WHEN 3 THEN ' ' || (i % 13) WHEN 4 THEN 'X' || (i % 9) || '  ' ELSE NULL END,
  CASE i % 4 WHEN 0 THEN 'ab' || (i % 5) WHEN 1 THEN 'AB' || (i % 5) WHEN 2 THEN 'Ab' || (i % 5) || ' ' ELSE (i % 7) END,
  CASE i % 3 WHEN 0 THEN 'k' || (i % 6) WHEN 1 THEN 'k' || (i % 6) || '   ' ELSE 'K' || (i % 6) END,
  CASE i % 5 WHEN 0 THEN i % 10 WHEN 1 THEN (i % 10) + 0.0 WHEN 2 THEN CAST(i % 10 AS TEXT) WHEN 3 THEN NULL ELSE x'0102' END,
  CASE i % 4 WHEN 0 THEN x'00' WHEN 1 THEN x'6162' WHEN 2 THEN NULL ELSE CAST('ab' AS BLOB) END
FROM s;
WITH RECURSIVE s(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM s WHERE i < 80)
INSERT INTO b SELECT i, i % 13, i * 0.25,
  CASE i % 5 WHEN 0 THEN 'X' || (i % 9) WHEN 1 THEN (i % 13) WHEN 2 THEN 'ab' || (i % 5) WHEN 3 THEN 'k' || (i % 6) || ' ' ELSE NULL END,
  CASE i % 6 WHEN 0 THEN i % 10 WHEN 1 THEN CAST(i % 10 AS TEXT) || '.0' WHEN 2 THEN (i % 10) * 1.0 WHEN 3 THEN NULL WHEN 4 THEN x'0102' ELSE 'AB' || (i % 5) END,
  CASE i % 3 WHEN 0 THEN x'6162' WHEN 1 THEN x'00' ELSE NULL END
FROM s;
INSERT INTO a VALUES (101, 0, -0.0, '0.3', 'a' || char(0) || 'x', 'z ', 9007199254740993, x'');
INSERT INTO a VALUES (102, 1234567890123456, 0.0, 'Inf', 'a' || char(0) || 'y', 'z', 1234567890123456, x'00');
INSERT INTO a VALUES (103, -1, 1e20, '1.0e+20', 'A' || char(0) || 'q', 'Z', '1e20', NULL);
INSERT INTO b VALUES (101, 0, 0.30000000000000004, 'a' || char(0) || 'Z', 9007199254740992.0, x'');
INSERT INTO b VALUES (102, -2, 1e999, '0.3', 1234567890123456.0, x'00');
INSERT INTO b VALUES (103, 7, -1e999, '-0.0', ' 1e20 ', x'0000');
SELECT count(*), sum(a.id * 1000 + b.id) FROM a JOIN b ON b.n = a.n + 1;
SELECT count(*), sum(a.id * 1000 + b.id) FROM a JOIN b ON a.n + 0 = b.n;
SELECT count(*), sum(a.id * 1000 + b.id) FROM a JOIN b ON a.t = b.n + 0;
SELECT count(*), sum(a.id * 1000 + b.id) FROM a JOIN b ON a.n = b.t || '';
SELECT count(*), sum(a.id * 1000 + b.id) FROM a JOIN b ON a.n = b.t;
SELECT count(*), sum(a.id * 1000 + b.id) FROM a JOIN b ON b.t = a.n;
SELECT count(*), sum(a.id * 1000 + b.id) FROM a JOIN b ON a.tn = b.t || '';
SELECT count(*), sum(a.id * 1000 + b.id) FROM a JOIN b ON b.t || '' = a.tn;
SELECT count(*), sum(a.id * 1000 + b.id) FROM a JOIN b ON a.tr = b.t || '';
SELECT count(*), sum(a.id * 1000 + b.id) FROM a JOIN b ON a.t = b.t COLLATE NOCASE;
SELECT count(*), sum(a.id * 1000 + b.id) FROM a JOIN b ON a.t COLLATE RTRIM = b.t || '';
SELECT count(*), sum(a.id * 1000 + b.id) FROM a JOIN b ON +a.u = b.u;
SELECT count(*), sum(a.id * 1000 + b.id) FROM a JOIN b ON a.u = +b.u;
SELECT count(*), sum(a.id * 1000 + b.id) FROM a JOIN b ON a.u = b.t || '';
SELECT count(*), sum(a.id * 1000 + b.id) FROM a JOIN b ON a.t = b.u + 0;
SELECT count(*), sum(a.id * 1000 + b.id) FROM a JOIN b ON a.t = b.r * 1;
SELECT count(*), sum(a.id * 1000 + b.id) FROM a JOIN b ON a.r = b.n * 0.25 - 0.0;
SELECT count(*), sum(a.id * 1000 + b.id) FROM a JOIN b ON a.r = -(b.r * 0);
SELECT count(*), sum(a.id * 1000 + b.id) FROM a JOIN b ON a.u = b.u * 1;
SELECT count(*), sum(a.id * 1000 + b.id) FROM a JOIN b ON a.n = b.u * 1;
SELECT count(*), sum(a.id * 1000 + b.id) FROM a JOIN b ON a.bl = CAST(b.bl AS BLOB);
SELECT count(*), sum(a.id * 1000 + b.id) FROM a JOIN b ON a.tn = CAST(b.bl AS TEXT);
SELECT count(*), sum(a.id * 1000 + b.id) FROM a JOIN b ON a.n = b.n + 0 AND a.t = b.t || '';
SELECT count(*), sum(a.id * 1000 + b.id) FROM a JOIN b ON a.n = b.n + 1 AND b.id % 3 = 0;
SELECT count(*), sum(a.id * 1000 + coalesce(b.id, 0)), count(b.id) FROM a LEFT JOIN b ON a.n = b.n + 1 AND b.id % 3 = 0;
SELECT count(*), sum(coalesce(a.id, 0) * 1000 + b.id), count(a.id) FROM a RIGHT JOIN b ON a.n = b.n + 1 AND a.id % 2 = 0;
SELECT count(*), sum(coalesce(a.id, 0) * 1000 + coalesce(b.id, 0)), count(a.id), count(b.id) FROM a FULL JOIN b ON a.t = b.n + 0;
SELECT count(*) FROM a JOIN b ON lower(a.tn) = lower(b.t) AND a.id < b.id;
SELECT count(*) FROM a JOIN b ON abs(a.n) = abs(b.n - 5);
SELECT count(*) FROM a JOIN b ON CASE WHEN a.n > 5 THEN a.n END = b.n + 0;
SELECT a.id, b.id FROM a JOIN b ON a.t = b.r * 1 ORDER BY 1, 2;
SELECT a.id, b.id FROM a JOIN b ON a.u = b.t || '' WHERE a.id > 100 OR b.id > 100 ORDER BY 1, 2;
SELECT a.id, b.id FROM a JOIN b ON a.tn = b.t || '' WHERE a.id > 100 ORDER BY 1, 2;
SELECT a.id, b.id FROM a JOIN b ON a.r = b.r + 0 ORDER BY 1, 2;
