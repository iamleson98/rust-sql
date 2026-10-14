-- JSONB path lookups walk element headers instead of decoding the whole
-- document: every selector shape, misses, duplicate keys (first wins),
-- escaped keys, and blobs that are JSON TEXT rather than JSONB.
CREATE TABLE j(x);
INSERT INTO j VALUES (jsonb('{"a":1,"b":[10,20,{"c":"x","c":"dup"}],"d":{"e":null,"f":true},"gA":2.50,"h":"t\\"q"}'));
INSERT INTO j VALUES (jsonb('[1,[2,3],{"k":[4,5,6]},"s"]'));
INSERT INTO j VALUES (CAST('{"ab":1}' AS BLOB));
INSERT INTO j VALUES (CAST('[1,2,3]' AS BLOB));
INSERT INTO j VALUES (jsonb('7'));
/*ordered*/ SELECT rowid, x->>'a', x->'b', x->>'$.b[1]', x->>'$.b[#-1].c', x->'$.d', x->>'$.d.f', x->>'$.gA', x->>'$.h' FROM j ORDER BY rowid;
/*ordered*/ SELECT rowid, x->>0, x->'$[1]', x->>'$[2].k[#-1]', x->>'$[#-1]', x->>'$[9]', x->>'$.zz' FROM j ORDER BY rowid;
/*ordered*/ SELECT rowid, json_extract(x, '$.ab'), json_extract(x, '$[2]'), json_extract(x, '$'), json_type(x, '$.b'), json_type(x, '$[1]'), json_type(x, '$.nope') FROM j ORDER BY rowid;
/*ordered*/ SELECT rowid, typeof(jsonb_extract(x, '$.b')), typeof(jsonb_extract(x, '$.a')), x->>'ab' FROM j ORDER BY rowid;
SELECT x->>'$.a' FROM j WHERE rowid = 5;
SELECT json_extract(x'0c', '$.a');
