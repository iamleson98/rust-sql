-- jsonb_set / jsonb_insert / jsonb_replace / jsonb_remove on a JSONB
-- blob edit the bytes in place: the results must be byte-identical to
-- SQLite's (headers of enclosing containers resized; untouched bytes
-- kept), including created keys / array appends, misses, duplicate keys
-- and the root path.
CREATE TABLE j(id INTEGER PRIMARY KEY, x);
INSERT INTO j VALUES (1, jsonb('{"a":1,"b":[10,20,{"c":"x","c":"dup"}],"d":{"e":null},"s":"0123456789"}'));
INSERT INTO j VALUES (2, jsonb('[1,[2,3],{"k":[4,5,6]},"s"]'));
INSERT INTO j VALUES (3, jsonb('{"p":"' || printf('%.300c', 'x') || '"}'));
INSERT INTO j VALUES (4, jsonb('7'));
/*ordered*/ SELECT id, hex(jsonb_set(x, '$.a', 99)), hex(jsonb_set(x, '$.b[1]', 'twenty')), hex(jsonb_set(x, '$.b[2].c', 'y')) FROM j ORDER BY id;
/*ordered*/ SELECT id, hex(jsonb_set(x, '$.new', 5)), hex(jsonb_set(x, '$.n1.n2', 5)), hex(jsonb_set(x, '$.b[3]', 'app')), hex(jsonb_set(x, '$.b[9]', 'no')) FROM j ORDER BY id;
/*ordered*/ SELECT id, hex(jsonb_insert(x, '$.a', 2)), hex(jsonb_insert(x, '$.z', json('[1,2]'))), hex(jsonb_insert(x, '$[#]', 8)), hex(jsonb_insert(x, '$[4]', 8)) FROM j ORDER BY id;
/*ordered*/ SELECT id, hex(jsonb_replace(x, '$.a', 'r')), hex(jsonb_replace(x, '$.q', 'r')), hex(jsonb_replace(x, '$', 3)), hex(jsonb_replace(x, '$[1][0]', 'deep')) FROM j ORDER BY id;
/*ordered*/ SELECT id, hex(jsonb_remove(x, '$.a')), hex(jsonb_remove(x, '$.b[#-1].c')), hex(jsonb_remove(x, '$[1]', '$[0]')), hex(jsonb_remove(x, '$.missing')), jsonb_remove(x, '$') IS NULL FROM j ORDER BY id;
/*ordered*/ SELECT id, hex(jsonb_set(x, '$.p', 'short')), hex(jsonb_set(x, '$.a', printf('%.300c', 'y'))) FROM j ORDER BY id;
/*ordered*/ SELECT id, json(jsonb_set(x, '$.a', 1, '$.a', 2, '$.b', NULL)), json(jsonb_insert(x, '$.a.b', 1)) FROM j ORDER BY id;
-- The same edits over JSON TEXT (the tree path) and `[#]` lookups.
/*ordered*/ SELECT id, json_set(json(x), '$.b[1]', 'no'), json_set(json(x), '$.n1[0]', 5), json_set(json(x), '$.n1[#].k', 5), json_insert(json(x), '$[#]', 8), json_insert(json(x), '$.a', 2) FROM j ORDER BY id;
/*ordered*/ SELECT id, json_replace(json(x), '$.a', 'r'), json_remove(json(x), '$[#]'), json_extract(json(x), '$[#]'), json_extract(json(x), '$[#-1]'), x->>'$[#-0]' FROM j ORDER BY id;
