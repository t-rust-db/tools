-- Highly selective filter (~1% of rows): rewards row-group/page statistics
-- pruning. 9900 is the 99th percentile of the generated `amount` spread.
SELECT id, amount, region, customer_id FROM bench WHERE amount > 9900
