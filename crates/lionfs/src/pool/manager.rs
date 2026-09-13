use super::device::{DeviceRecord, DeviceState};
use std::collections::HashMap;

pub struct MultiDeviceManager {
    devices: HashMap<u64, DeviceRecord>,
}

impl Default for MultiDeviceManager {
    fn default() -> Self {
        Self::new()
    }
}

impl MultiDeviceManager {
    pub fn new() -> Self {
        Self {
            devices: HashMap::new(),
        }
    }

    pub fn add_device(&mut self, record: DeviceRecord) {
        self.devices.insert(record.dev_id, record);
    }

    pub fn get_device(&self, dev_id: u64) -> Option<&DeviceRecord> {
        self.devices.get(&dev_id)
    }

    pub fn set_device_state(&mut self, dev_id: u64, state: DeviceState) {
        if let Some(dev) = self.devices.get_mut(&dev_id) {
            dev.state = state as u8;
        }
    }
}
