-- COLLATE binds looser than unary - / + / ~ (SQLite's %left COLLATE vs
-- MINUS expr [BITNOT]): only a TOP-level COLLATE is skipped when a
-- GROUP BY / ORDER BY term is read as an ordinal.
CREATE TABLE g(a INTEGER, b TEXT);
INSERT INTO g VALUES (1, 'x'), (2, 'Y'), (3, 'y'), (1, 'X');
SELECT count(*) FROM g GROUP BY +(-88 COLLATE RTRIM);
SELECT count(*) FROM g GROUP BY +(-1 COLLATE RTRIM);
SELECT a FROM g GROUP BY -1 COLLATE nocase;
SELECT a, count(*) FROM g GROUP BY +1 COLLATE nocase;
SELECT a, count(*) FROM g GROUP BY - -1 COLLATE nocase;
SELECT a FROM g ORDER BY +(1 COLLATE nocase);
SELECT b FROM g ORDER BY 1 COLLATE nocase, a;
SELECT -0.0 COLLATE nocase, typeof(-0.0 COLLATE nocase), 1/(-0.0 COLLATE nocase);
SELECT -9223372036854775808 COLLATE binary, typeof(-9223372036854775808 COLLATE rtrim);
SELECT -b COLLATE nocase FROM g;
SELECT b FROM g WHERE -a COLLATE nocase < -1 ORDER BY b;
SELECT ~a COLLATE binary, +b COLLATE nocase = 'y' FROM g;
SELECT 'a' || b COLLATE nocase = 'AY' FROM g;
SELECT -'5' COLLATE nocase IS NULL, '5' COLLATE nocase IS NULL, -'5' IS NULL;
SELECT max(b COLLATE nocase), min(-a COLLATE nocase) FROM g;
SELECT DISTINCT b COLLATE nocase FROM g ORDER BY 1;
SELECT count(*) FROM g GROUP BY -(2) COLLATE nocase;
