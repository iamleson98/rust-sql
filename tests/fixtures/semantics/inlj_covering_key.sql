-- An index nested-loop join whose inner side needs only the probed key
-- column (and the rowid) is COVERING: the key value is decoded from the
-- probe's index encoding instead of fetching the row. The decode must
-- return the STORED value exactly — INTEGER vs REAL by affinity, text
-- and blobs byte-exact — and fall back to the row where it is ambiguous
-- (BLOB affinity numbers, NOCASE-folded text, REAL zero).
CREATE TABLE o(id INTEGER PRIMARY KEY, x, y TEXT);
CREATE TABLE ti(k INTEGER, v);
CREATE TABLE tn(k NUMERIC, v);
CREATE TABLE tr(k REAL, v);
CREATE TABLE tt(k TEXT, v);
CREATE TABLE tb(k BLOB, v);
CREATE TABLE tc(k TEXT COLLATE NOCASE, v);
CREATE TABLE tm(k INTEGER, j TEXT, v);
CREATE INDEX ti_k ON ti(k);
CREATE INDEX tn_k ON tn(k);
CREATE INDEX tr_k ON tr(k);
CREATE INDEX tt_k ON tt(k);
CREATE INDEX tb_k ON tb(k);
CREATE INDEX tc_k ON tc(k);
CREATE INDEX tm_kj ON tm(k, j);
INSERT INTO o(x, y) VALUES (1,'a'),(2.5,'B'),(2,'b'),(0,'x'),(-0.0,'zz'),(9007199254740993,'q'),(x'0001','e'),('7','7'),(3.0,'c'),(1e300,'big'),(NULL,NULL);
INSERT INTO ti VALUES (1,1),(2.5,2),(2,3),(0,4),(9007199254740993,5),(7,6),(3,7);
INSERT INTO tn VALUES (1,1),(2.5,2),(2.0,3),(0,4),(9007199254740993,5),('7',6),(1e300,7);
INSERT INTO tr VALUES (1,1),(2.5,2),(2,3),(0.0,4),(-0.0,5),(3,6),(1e300,7);
INSERT INTO tt VALUES ('a',1),('b',2),('B',3),(2,4),('7',5),(x'0001',6);
INSERT INTO tb VALUES (1,1),(2.0,2),(2,3),(x'0001',4),('a',5),(2.5,6);
INSERT INTO tc VALUES ('A',1),('b',2),('B',3),('zZ',4),('x',5);
INSERT INTO tm VALUES (1,'p',1),(2,'q',2),(2,'r',3),(3.0,'s',4);
SELECT 1 FROM o WHERE id < 1000;
ANALYZE;
/*ordered*/ SELECT o.id, ti.k, typeof(ti.k) FROM o JOIN ti ON ti.k = o.x WHERE o.id < 1000 ORDER BY 1, 2;
/*ordered*/ SELECT o.id, tn.k, typeof(tn.k) FROM o JOIN tn ON tn.k = o.x WHERE o.id < 1000 ORDER BY 1, 2;
/*ordered*/ SELECT o.id, tr.k, typeof(tr.k), quote(tr.k) FROM o JOIN tr ON tr.k = o.x WHERE o.id < 1000 ORDER BY 1, 3, 4;
/*ordered*/ SELECT o.id, tt.k, typeof(tt.k) FROM o JOIN tt ON tt.k = o.y WHERE o.id < 1000 ORDER BY 1, 2;
/*ordered*/ SELECT o.id, hex(tb.k), typeof(tb.k) FROM o JOIN tb ON tb.k = o.x WHERE o.id < 1000 ORDER BY 1, 2, 3;
/*ordered*/ SELECT o.id, tc.k FROM o JOIN tc ON tc.k = o.y WHERE o.id < 1000 ORDER BY 1, 2;
/*ordered*/ SELECT o.id, tm.k, typeof(tm.k) FROM o JOIN tm ON tm.k = o.x WHERE o.id < 1000 ORDER BY 1, 2;
/*ordered*/ SELECT o.id, ti.k, ti.rowid FROM o JOIN ti ON ti.k = o.x WHERE o.id < 1000 ORDER BY 1, 3;
SELECT count(*), sum(ti.k), group_concat(typeof(ti.k)) FROM o JOIN ti ON ti.k = o.x WHERE o.id < 1000;
