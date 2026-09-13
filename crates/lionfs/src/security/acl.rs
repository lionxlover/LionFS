pub const ACL_FLAG_IMMUTABLE: u16 = 1 << 0;
pub const ACL_FLAG_APPEND_ONLY: u16 = 1 << 1;
pub const ACL_FLAG_SECURE_DELETE: u16 = 1 << 2;

pub struct AclManager;

impl AclManager {
    pub fn is_immutable(flags: u16) -> bool {
        (flags & ACL_FLAG_IMMUTABLE) != 0
    }

    pub fn is_append_only(flags: u16) -> bool {
        (flags & ACL_FLAG_APPEND_ONLY) != 0
    }

    pub fn requires_secure_delete(flags: u16) -> bool {
        (flags & ACL_FLAG_SECURE_DELETE) != 0
    }
}
