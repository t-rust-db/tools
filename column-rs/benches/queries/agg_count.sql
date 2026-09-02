-- COUNT(*): the metadata-only best case -- can be answered from row counts in
-- the Parquet footer without decoding a single page.
SELECT COUNT(*) FROM bench
