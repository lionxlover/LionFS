#[cfg(test)]
mod tests {
    use crate::pool::device::{DeviceRecord, DeviceState};
    use crate::pool::manager::MultiDeviceManager;
    use crate::pool::raid::{RaidEngine, RaidProfile};

    #[test]
    fn test_device_manager() {
        let mut manager = MultiDeviceManager::new();
        let record = DeviceRecord {
            dev_id: 1,
            uuid: [0; 16],
            capacity: 1000,
            state: DeviceState::Online as u8,
            role: 0,
            padding: [0; 6],
        };

        manager.add_device(record);
        assert!(manager.get_device(1).is_some());

        manager.set_device_state(1, DeviceState::Offline);
        let updated = manager.get_device(1).unwrap();
        assert_eq!(updated.state, DeviceState::Offline as u8);
    }

    #[test]
    fn test_raid_engine_mapping() {
        // Test RAID 0 (Striping)
        let engine = RaidEngine::new(RaidProfile::Raid0, 16, 2);

        // Block 0 should map to device 0, physical block 0
        let map0 = engine.map_read(0);
        assert_eq!(map0[0], (0, 0));

        // Block 16 should map to device 1, physical block 0
        let map16 = engine.map_read(16);
        assert_eq!(map16[0], (1, 0));

        // Block 32 should map to device 0, physical block 16
        let map32 = engine.map_read(32);
        assert_eq!(map32[0], (0, 16));

        // Test RAID 1 (Mirroring)
        let mirror_engine = RaidEngine::new(RaidProfile::Raid1, 0, 2);
        let write_map = mirror_engine.map_write(10);
        assert_eq!(write_map.len(), 2);
        assert_eq!(write_map[0], (0, 10));
        assert_eq!(write_map[1], (1, 10));
    }
}
