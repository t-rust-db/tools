-- Top-N: a full sort is avoidable with a bounded heap, so a large gap here
-- points at the sort implementation rather than at scan speed.
SELECT id, amount FROM bench ORDER BY amount DESC LIMIT 100
