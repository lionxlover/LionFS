pub struct DeviceHealthStats {
    pub read_errors: u64,
    pub write_errors: u64,
    pub checksum_errors: u64,
    pub read_latency_ms: u32,
    pub write_latency_ms: u32,
    pub temperature_c: i8,
}

impl Default for DeviceHealthStats {
    fn default() -> Self {
        Self {
            read_errors: 0,
            write_errors: 0,
            checksum_errors: 0,
            read_latency_ms: 0,
            write_latency_ms: 0,
            temperature_c: 40,
        }
    }
}
