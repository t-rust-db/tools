-- Equi-join against the smaller customer dimension, then aggregate. Columns
-- are table-qualified because column-rs requires qualification in joins (and
-- DuckDB accepts it), keeping one SQL text valid on both engines.
SELECT bench_customers.tier, SUM(bench.amount)
FROM bench JOIN bench_customers ON bench.customer_id = bench_customers.customer_id
GROUP BY bench_customers.tier
