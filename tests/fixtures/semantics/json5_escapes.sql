-- JSON5 string escapes and single-quoted strings, as SQLite accepts and
-- renders them: JSON5-only escapes (\x \v \' \0 line continuations) are
-- lenient-only; \1-\7, \0 before a digit and \u{...} are errors; text
-- rendering rewrites them SQLite's way (\x41 -> A, \v -> \u0009);
-- extraction decodes them; single-quoted strings type TEXT/TEXTJ/TEXT5.
SELECT json_valid('" \0 "'), json_valid('" \0 "', 2), json_valid('" \1 "', 2), json_valid('" \7 "', 2);
SELECT json_valid('" \'' "'), json_valid('" \'' "', 2), json_valid('"\u{41}"', 2), json_valid('"\01"', 2);
SELECT json_valid('"\x41"', 1), json_valid('"\x41"', 2), json_valid('" \v "', 1), json_valid('"A"', 1);
SELECT json_valid('"a\
b"', 2), json_valid('"a\
b"', 1), json('"a\
b"');
SELECT json('"\x4a"'), json('"\xAF"'), json('"\0"'), json('"\''"'), json('"\v"');
SELECT json('"\n\x41\t"'), hex(json('" \x41\v "')), json(jsonb('"\x41\v"')), hex(jsonb('"\x41"'));
SELECT json_extract('{"a":"\x41\v"}', '$.a') = 'A' || char(11), hex(json_extract('{"a":"\x41\v"}', '$.a'));
SELECT '{"a":"\x41\v\''"}' ->> '$.a' = 'A' || char(11) || '''';
SELECT json('[''a'']'), json('[''a"b'']'), json('[''it\''s'']'), json('{''k"'':1}'), json('[''x\ty'']');
SELECT hex(jsonb('[''a'']')), hex(jsonb('[''a"b'']')), hex(jsonb('[''a\''b'']')), hex(jsonb('{''k'':1}')), hex(jsonb('[''x\ty'']'));
SELECT json_valid('[''a'']', 1), json_valid('[''a'']', 2), json_valid('{''k'':''v''}', 2);
SELECT json_set('{a:''x''}', '$.b', 'y'), json_remove('[''p'', ''q'']', '$[0]');
SELECT json('"\1"');
SELECT json('"\u{41}"');
SELECT json('"\01"');
