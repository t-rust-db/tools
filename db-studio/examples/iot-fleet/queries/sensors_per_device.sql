-- Row mode (fleet.sqlite): sensor count per device.
SELECT device_id, COUNT(*) FROM sensors GROUP BY device_id ORDER BY device_id
