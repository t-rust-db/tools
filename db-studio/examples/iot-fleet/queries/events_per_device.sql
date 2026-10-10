-- Stream mode (device.log): event count per device.
SELECT device, COUNT(*) FROM log GROUP BY device ORDER BY device
