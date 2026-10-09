-- A GROUP BY term's grouping collation is sqlite3ExprCollSeq's: an explicit
-- COLLATE nested anywhere in the term (EP_Collate propagates through
-- operators, function arguments and CASE arms), else a column's declared
-- collation through CAST / unary +.
CREATE TABLE g(j TEXT, k INTEGER);
INSERT INTO g VALUES ('Abc', 1), ('abc', 2), ('ABC ', 3), ('x', NULL), ('X', 4);
SELECT (k NOTNULL) || (j COLLATE NOCASE), count(*) FROM g GROUP BY (k NOTNULL) || (j COLLATE NOCASE);
SELECT count(*) FROM g GROUP BY 'p' || (j COLLATE NOCASE);
SELECT count(*) FROM g GROUP BY (j COLLATE NOCASE) || 'p';
SELECT count(*) FROM g GROUP BY upper(j COLLATE NOCASE);
SELECT count(*) FROM g GROUP BY substr(j COLLATE NOCASE, 1, 2);
SELECT count(*) FROM g GROUP BY CASE WHEN k THEN j COLLATE NOCASE ELSE j END;
SELECT count(*) FROM g GROUP BY -(j COLLATE NOCASE);
SELECT DISTINCT 'p' || (j COLLATE NOCASE) FROM g ORDER BY 1;
SELECT 'p' || (j COLLATE NOCASE) AS x FROM g ORDER BY x, k;
SELECT j FROM g ORDER BY 'p' || (j COLLATE NOCASE), k;
SELECT count(*) FROM g GROUP BY j || (k COLLATE NOCASE);
SELECT count(DISTINCT 'p' || (j COLLATE NOCASE)) FROM g;
SELECT max('p' || (j COLLATE NOCASE)), min(j || '' COLLATE NOCASE) FROM g;
SELECT count(*) FROM g GROUP BY (j COLLATE NOCASE) COLLATE BINARY;
CREATE TABLE gc(j TEXT COLLATE NOCASE, id INTEGER PRIMARY KEY);
INSERT INTO gc(j) VALUES ('Q'), ('q'), ('Q ');
SELECT count(*) FROM gc GROUP BY CAST(j AS TEXT);
SELECT count(*) FROM gc GROUP BY +j;
SELECT count(*) FROM gc GROUP BY j || '';
SELECT count(*) FROM gc t1 JOIN gc t2 ON t1.id = t2.id WHERE t1.id > 0 GROUP BY 'k' || (t1.j COLLATE NOCASE);
