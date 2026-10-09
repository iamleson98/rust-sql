-- SQLITE_FUNC_NEEDCOLL functions (nullif, scalar min / max) compare through
-- the collation of their FIRST argument that has one; a plain column has one
-- (BINARY) even when no collated column is in scope; the rowid has none.
CREATE TABLE n3(j TEXT PRIMARY KEY, k INTEGER, l) WITHOUT ROWID;
INSERT INTO n3 VALUES ('x y', 1, 'X Y'), ('abc', 2, 'ABC'), ('Q', 3, 'q');
SELECT j, nullif(j, upper((j COLLATE NOCASE))), nullif(j COLLATE NOCASE, upper(j)), nullif(upper(j), j COLLATE NOCASE) FROM n3 ORDER BY j;
SELECT nullif(j, l), nullif(l, j), nullif(j || '', l COLLATE NOCASE), nullif(+j, l COLLATE NOCASE) FROM n3 ORDER BY j;
SELECT max(j, l COLLATE NOCASE), min(j, upper(j) COLLATE NOCASE), max(CAST(j AS TEXT), l COLLATE NOCASE) FROM n3 ORDER BY j;
CREATE TABLE n4(id INTEGER PRIMARY KEY, s TEXT);
INSERT INTO n4 VALUES (1, 'a'), (2, 'B');
SELECT nullif(id, s COLLATE NOCASE), max(rowid, s COLLATE NOCASE, 'b'), nullif(s, upper(s) COLLATE NOCASE) FROM n4;
SELECT nullif(x, upper(x COLLATE NOCASE)) FROM (SELECT j AS x FROM n3) ORDER BY 1;
