-- A list IN compares with the LEFT operand's affinity and collation alone
-- (exprINAffinity), so a COLUMN member is converted too: int_col IN
-- (text_col) compares numerically, text_col IN (int_col) as text. The
-- compiled WHERE path used to leave column members unconverted (wrong rows,
-- including NOT IN returning excluded rows).
CREATE TABLE z(id INTEGER PRIMARY KEY, a TEXT, b TEXT COLLATE NOCASE, n INTEGER, u, r REAL);
INSERT INTO z VALUES(1,'A','a',0,'0',1.0),(2,'10','10',10,'10',10.0),(3,'x','X',5,'5.0',5.0),(4,'7',' 7',7,7,7.5),(5,NULL,'b',NULL,x'30',NULL);
CREATE INDEX z_n ON z(n);
SELECT id, a IN (b), b IN (a), n IN (u), u IN (n), a IN (n), n IN (a), r IN (u), u IN (r), a IN (r), n IN (u, 99), u IN (a, b) FROM z ORDER BY id;
SELECT id FROM z WHERE n IN (u) ORDER BY id;
SELECT id FROM z WHERE u IN (n) ORDER BY id;
SELECT id FROM z WHERE a IN (n) ORDER BY id;
SELECT id FROM z WHERE n IN (a) ORDER BY id;
SELECT id FROM z WHERE r IN (u) ORDER BY id;
SELECT id FROM z WHERE u IN (r) ORDER BY id;
SELECT id FROM z WHERE a IN (b) ORDER BY id;
SELECT id FROM z WHERE b IN (a) ORDER BY id;
SELECT id FROM z WHERE a IN (b, 'zz') ORDER BY id;
SELECT id FROM z WHERE b IN (a, 'zz') ORDER BY id;
SELECT id FROM z WHERE n IN (u, 99) ORDER BY id;
SELECT id FROM z WHERE n NOT IN (u, 99) ORDER BY id;
SELECT id FROM z WHERE a IN (n, 'zz') ORDER BY id;
SELECT id FROM z WHERE a NOT IN (n, 'zz') ORDER BY id;
SELECT id FROM z WHERE id IN (u) ORDER BY id;
SELECT id FROM z WHERE id IN (u, a) ORDER BY id;
SELECT id FROM z WHERE n IN (u+0) ORDER BY id;
SELECT id FROM z WHERE n IN (CAST(u AS TEXT)) ORDER BY id;
SELECT id FROM z WHERE a IN (n+0) ORDER BY id;
SELECT id FROM z WHERE u IN (b, 'q') ORDER BY id;
SELECT id FROM z WHERE n IN (a, u) AND id > 1 ORDER BY id;
SELECT id FROM z WHERE n > 1 AND n IN (a) ORDER BY id;
SELECT id FROM z WHERE (n COLLATE NOCASE) IN (a) ORDER BY id;
SELECT id FROM z WHERE a IN (b COLLATE NOCASE) ORDER BY id;
SELECT count(*) FROM z WHERE n IN (u);
SELECT count(*) FROM z WHERE a IN (n, r);
DELETE FROM z WHERE n IN (u) AND id < 3;
SELECT id FROM z ORDER BY id;
UPDATE z SET u = 'hit' WHERE a IN (n);
SELECT id, u FROM z ORDER BY id;
