-- Stream mode (device.log): device events at error severity.
SELECT timestamp, device, message FROM log WHERE severity_text = 'ERROR'
