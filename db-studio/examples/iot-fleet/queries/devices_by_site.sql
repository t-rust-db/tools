-- Row mode (fleet.sqlite): every device with its site and region.
SELECT devices.id, devices.model, devices.firmware, sites.name, sites.region
FROM devices
JOIN sites ON devices.site_id = sites.id
ORDER BY sites.region, devices.id
