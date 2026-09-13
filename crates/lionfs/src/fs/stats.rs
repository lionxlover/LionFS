pub struct FsStats {
    pub compression_ratio: f64,
    pub deduplication_savings_bytes: u64,
    pub dedupe_objects_count: u64,
    pub compressed_bytes: u64,
    pub uncompressed_bytes: u64,
}

impl Default for FsStats {
    fn default() -> Self {
        Self::new()
    }
}

impl FsStats {
    pub fn new() -> Self {
        Self {
            compression_ratio: 1.0,
            deduplication_savings_bytes: 0,
            dedupe_objects_count: 0,
            compressed_bytes: 0,
            uncompressed_bytes: 0,
        }
    }
}
