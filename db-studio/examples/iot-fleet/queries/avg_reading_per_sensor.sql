-- Batch mode (readings.parquet): average reading per sensor over the full window.
SELECT device_id, sensor_id, AVG(value) FROM readings GROUP BY device_id, sensor_id ORDER BY device_id, sensor_id
