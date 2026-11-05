.open /tmp/native-libs-cb.db
CREATE TABLE IF NOT EXISTS f(x REAL, s TEXT);
DELETE FROM f;
INSERT INTO f SELECT value*0.25, printf('%05d-%s', value, hex(value)) FROM generate_series(1, 2000);
SELECT count(*), sum(x), avg(x), max(s) FROM f;
SELECT ieee754(2.5), ieee754_from_blob(x'4004000000000000'), decimal_add('1.1','2.25');
SELECT hex(sha3_query('SELECT x FROM f LIMIT 10'));
.mode box
SELECT value, value*1.5 AS d, format('%.3f', value/7.0) FROM generate_series(1,4);
.mode list
.schema
.tables
SELECT json_group_array(x) FROM (SELECT x FROM f ORDER BY x LIMIT 5);
CREATE VIRTUAL TABLE ft USING fts5(body);
INSERT INTO ft SELECT s FROM f;
SELECT count(*) FROM ft WHERE ft MATCH '00042*';
SELECT total_changes();
.dbinfo
SELECT 'x' REGEXP 'x';
.stats on
SELECT 1;
