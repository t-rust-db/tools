-- Cross-mode join (db-studio#54): device.log's stream table joined to
-- fleet.sqlite's devices table -- run this with device.log active.
SELECT log.timestamp, log.device, devices.model, devices.firmware, log.message
FROM log
JOIN devices ON log.device = devices.id
WHERE log.severity_text = 'ERROR'
