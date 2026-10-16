-- TEXT JSON documents resolved and edited as JSONB bytes: `->` / `->>`,
-- json_set / json_insert / json_replace / json_remove (text and jsonb_
-- outputs), JSON5 input, escapes, missing paths and root removal; then a
-- multi-row UPDATE keeping JSON expression indexes and a partial index
-- in step (old and new keys staged per row).
CREATE TABLE d(id INTEGER PRIMARY KEY, x TEXT);
INSERT INTO d VALUES
  (1, '{"a":1,"b":[1,2,{"c":"x\ty"}],"s":"café","n":null}'),
  (2, '{a:0x1F, b:[.5, +3, Infinity], c:''q'', d:{e:{f:[]}}}'),
  (3, '[1, "two", {"three": 3}, [4]]'),
  (4, '"just a string"'),
  (5, '  {"dup":1, "dup":2, "z":-0.0e1}  ');
/*ordered*/ SELECT id, x -> '$.a', x ->> '$.a', x -> 'b', x ->> '$.b[1]', x -> '$.b[#-1]' FROM d ORDER BY id;
/*ordered*/ SELECT id, x ->> '$.s', x -> '$.n', x ->> '$.n', x -> '$.missing', x ->> 2, x -> '$[2].three' FROM d ORDER BY id;
/*ordered*/ SELECT id, x ->> '$.d.e.f', x ->> '$.c', x ->> '$.dup', x -> '$.z' FROM d ORDER BY id;
/*ordered*/ SELECT id, json_set(x, '$.a', 99, '$.new', 'v"q') FROM d ORDER BY id;
/*ordered*/ SELECT id, json_insert(x, '$.a', 5, '$.b[#]', json('{"k":[1]}')) FROM d ORDER BY id;
/*ordered*/ SELECT id, json_replace(x, '$.a', 'text', '$.zz', 1) FROM d ORDER BY id;
/*ordered*/ SELECT id, json_remove(x, '$.a', '$.b[0]'), json_remove(x, '$') FROM d ORDER BY id;
/*ordered*/ SELECT id, hex(jsonb_set(x, '$.a', 7)), hex(jsonb_remove(x, '$.b')) FROM d ORDER BY id;
/*ordered*/ SELECT id, json_set(x, '$', json('[0]')), json_set(x, '$[1]', 'y') FROM d ORDER BY id;
SELECT json_set('{"a":', '$.a', 1);
SELECT json_set('{"a":1}', 'nopath', 1);
SELECT '{"a":' -> '$.a';
-- Expression indexes over TEXT JSON, maintained by a batched UPDATE.
CREATE TABLE j(id INTEGER PRIMARY KEY, x TEXT);
WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i + 1 FROM n WHERE i < 150)
INSERT INTO j SELECT i, json_object('a', i % 17, 'b', 'v' || i, 'f', i, 'pad', printf('%.300c', 'p')) FROM n;
CREATE INDEX ja ON j(x ->> 'a');
CREATE INDEX jb ON j(x ->> 'b');
CREATE INDEX jf ON j(x ->> 'f') WHERE x ->> 'f' > 100;
UPDATE j SET x = json_insert(x, '$.g', (x ->> 'f') + 1);
UPDATE j SET x = json_set(x, '$.a', (x ->> 'a') * 2, '$.f', (x ->> 'f') - 60);
UPDATE j SET x = json_remove(x, '$.b') WHERE id % 3 = 0;
/*ordered*/ SELECT x ->> 'a', count(*) FROM j INDEXED BY ja GROUP BY 1 ORDER BY 1;
SELECT count(*), sum(id) FROM j INDEXED BY jb WHERE x ->> 'b' IS NULL;
SELECT count(*), sum(id) FROM j INDEXED BY jb WHERE x ->> 'b' > 'v5';
SELECT count(*), sum(id) FROM j INDEXED BY jf WHERE x ->> 'f' > 100;
SELECT count(*), sum(x ->> 'g') FROM j WHERE x ->> 'f' > 50;
PRAGMA integrity_check;
