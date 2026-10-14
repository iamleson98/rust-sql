-- PRAGMA table_list and its table-valued forms (fossil's schema probe
-- in speedtest1: `FROM pragma_table_list WHERE schema=… AND type IN …`):
-- the unfiltered order is SQLite's schema-hash order, compared unordered.
CREATE TABLE b(x);
CREATE TABLE a(x INT, y INT) STRICT;
CREATE VIEW v AS SELECT 1, 2, 3;
CREATE TABLE w(k PRIMARY KEY) WITHOUT ROWID;
CREATE TABLE c(z);
CREATE VIEW v2 AS SELECT * FROM a, c;
PRAGMA table_list;
PRAGMA table_list(a);
PRAGMA main.table_list;
SELECT * FROM pragma_table_list WHERE schema='main';
/*ordered*/ SELECT name, ncol FROM pragma_table_list('v2');
/*ordered*/ SELECT name FROM pragma_table_list WHERE type IN ('table','virtual') AND name NOT GLOB 'sqlite_*' ORDER BY name;
/*ordered*/ SELECT * FROM pragma_table_xinfo('a');
SELECT 1 FROM pragma_table_xinfo('a') WHERE name = 'y';
