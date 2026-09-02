-- Low-cardinality grouping (4 regions): hash aggregation over a dictionary-
-- encoded string key.
SELECT region, SUM(amount) FROM bench GROUP BY region
