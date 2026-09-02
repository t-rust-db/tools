-- Full scan with an early LIMIT: measures open + decode + first-rows latency.
-- Column list is explicit because column-rs's SQL subset has no `SELECT *`.
SELECT id, amount, region, customer_id FROM bench LIMIT 1000
