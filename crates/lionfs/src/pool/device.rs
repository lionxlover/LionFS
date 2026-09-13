use bytemuck::{Pod, Zeroable};

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceState {
    Online = 0,
    Offline = 1,
    Failed = 2,
    Rebuilding = 3,
    Spare = 4,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct DeviceRecord {
    pub dev_id: u64,
    pub uuid: [u8; 16],
    pub capacity: u64,
    pub state: u8,
    pub role: u8,
    pub padding: [u8; 6],
}
