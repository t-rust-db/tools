-- Batch mode (readings.parquet): the 5 highest individual readings recorded.
SELECT device_id, sensor_id, ts, value FROM readings ORDER BY value DESC LIMIT 5
