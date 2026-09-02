-- Non-selective filter (~50% of rows): nothing can be pruned, so this
-- measures raw predicate evaluation and materialization throughput.
-- 5000 is the median of the generated `amount` spread.
SELECT id, amount, region, customer_id FROM bench WHERE amount > 5000
