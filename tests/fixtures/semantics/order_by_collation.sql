-- ORDER BY term collation is sqlite3ExprCollSeq of the RESOLVED term:
-- an explicit COLLATE anywhere inside it (EP_Collate propagation, e.g.
-- through a nested CASE operand), else a column's declared collation
-- through CAST / unary +, never through || or arithmetic. Aliases resolve
-- through a COLLATE wrapper (ORDER BY alias COLLATE NOCASE).
CREATE TABLE o(id INTEGER PRIMARY KEY, h TEXT COLLATE NOCASE, l, k INTEGER);
INSERT INTO o VALUES(1,'abc','_',1),(2,'ABC','Inf',2),(3,'abd','a',3),(4,'Abc','B',4),(5,'_','b',5),(6,'b','A',6);
/*ordered*/ SELECT CAST(h AS VARCHAR(5)), id FROM o ORDER BY 1, 2;
/*ordered*/ SELECT h, id FROM o ORDER BY CAST(h AS TEXT), id;
/*ordered*/ SELECT h, id FROM o ORDER BY +h, id;
/*ordered*/ SELECT h, id FROM o ORDER BY -h, id;
/*ordered*/ SELECT h, id FROM o ORDER BY h || '', id;
/*ordered*/ SELECT h, id FROM o ORDER BY upper(h) || h, id;
/*ordered*/ SELECT h, id FROM o ORDER BY lower(h), id;
/*ordered*/ SELECT l, id FROM o ORDER BY CASE WHEN k > 100 THEN (l COLLATE NOCASE) ELSE l END, id;
/*ordered*/ SELECT l, id FROM o ORDER BY CASE k WHEN 0 THEN 1 ELSE l END || (l COLLATE NOCASE), id;
/*ordered*/ SELECT DISTINCT CASE CASE k WHEN ('x' COLLATE NOCASE) THEN 1 ELSE 0 END WHEN 9 THEN 1 ELSE l END AS c FROM o ORDER BY 1;
/*ordered*/ SELECT CASE WHEN k THEN l END AS c, id FROM o ORDER BY c COLLATE NOCASE, id;
/*ordered*/ SELECT l || '' AS c, id FROM o ORDER BY c, id;
/*ordered*/ SELECT (l COLLATE NOCASE) || '' AS c, id FROM o ORDER BY c, id;
/*ordered*/ SELECT h, id FROM o ORDER BY CAST(h AS TEXT) DESC, id LIMIT 3;
/*ordered*/ SELECT max(h), k % 2 FROM o GROUP BY k % 2 ORDER BY CAST(max(h) AS TEXT), 2;
/*ordered*/ SELECT id FROM o ORDER BY abs(k) COLLATE NOCASE, l, id;
/*ordered*/ SELECT l AS c, id FROM o ORDER BY c COLLATE NOCASE, id;
/*ordered*/ SELECT l AS c, id FROM o ORDER BY c COLLATE NOCASE DESC, id;
/*ordered*/ SELECT upper(l) AS c, id FROM o ORDER BY c COLLATE BINARY, id;
/*ordered*/ SELECT h AS c, id FROM o ORDER BY c COLLATE BINARY, id;
/*ordered*/ SELECT h AS c, id FROM o ORDER BY c, id;
/*ordered*/ SELECT l AS h, id FROM o ORDER BY h, id;
/*ordered*/ SELECT k % 3 AS g, count(*) FROM o GROUP BY g ORDER BY g COLLATE NOCASE DESC;
/*ordered*/ SELECT l AS c, id FROM o ORDER BY c COLLATE NOCASE, id LIMIT 3;
