-- NULLS FIRST / LAST (SQLite 3.30+) in ORDER BY — with and without LIMIT, ordinal
-- terms, compound ORDER BY, aliases and window ORDER BY. The clause used to
-- be parsed and ignored.
CREATE TABLE q(id INTEGER PRIMARY KEY, v);
INSERT INTO q(v) VALUES (3), (NULL), (1), (NULL), (2);
/*ordered*/ SELECT id, v FROM q ORDER BY v NULLS LAST;
/*ordered*/ SELECT id, v FROM q ORDER BY v NULLS LAST LIMIT 3;
/*ordered*/ SELECT id, v FROM q ORDER BY v DESC NULLS FIRST LIMIT 3;
/*ordered*/ SELECT id, v FROM q ORDER BY v + 0 NULLS LAST LIMIT 3;
/*ordered*/ SELECT id, v FROM q WHERE id > 0 ORDER BY v NULLS LAST LIMIT 3;
/*ordered*/ SELECT id, v FROM q ORDER BY v NULLS LAST, id DESC LIMIT 4;
/*ordered*/ SELECT * FROM q ORDER BY 2 NULLS LAST;
/*ordered*/ SELECT id, v FROM q UNION ALL SELECT 9, NULL ORDER BY 2 NULLS LAST;
/*ordered*/ SELECT id, v AS w FROM q ORDER BY w DESC NULLS FIRST;
/*ordered*/ SELECT id, v, row_number() OVER (ORDER BY v NULLS LAST) FROM q ORDER BY id;
/*ordered*/ SELECT id, v, row_number() OVER (ORDER BY v DESC NULLS FIRST) FROM q ORDER BY id;
