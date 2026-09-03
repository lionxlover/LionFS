#[cfg(test)]
mod tests {
    use crate::fs::compression::CompressionManager;
    use crate::fs::dedupe::DeduplicationManager;
    use crate::security::acl::{AclManager, ACL_FLAG_IMMUTABLE, ACL_FLAG_SECURE_DELETE};
    use crate::security::encryption::EncryptionManager;

    #[test]
    fn test_compression_algorithms() {
        assert!(CompressionManager::get_algorithm(1).is_some());
        assert!(CompressionManager::get_algorithm(2).is_some());
        assert!(CompressionManager::get_algorithm(3).is_some());
        assert!(CompressionManager::get_algorithm(99).is_none());

        let data = b"Hello LionFS! This is a test of the compression system. Repeating data repeating data repeating data.";
        let (algo_id, compressed) = CompressionManager::adaptive_compress(data, 2);
        // This input is genuinely repetitive, so real compression should
        // actually shrink it -- and it must decompress back byte-for-byte.
        assert_ne!(algo_id, 0);
        assert!(compressed.len() < data.len());
        let algo = CompressionManager::get_algorithm(algo_id).unwrap();
        assert_eq!(algo.decompress(&compressed).unwrap(), data);
    }

    #[test]
    fn test_deduplication_hashing() {
        let data = b"Test block data";
        let hash1 = DeduplicationManager::hash_block(data);
        let hash2 = DeduplicationManager::hash_block(data);
        assert_eq!(hash1, hash2);

        let data2 = b"Different test block data";
        let hash3 = DeduplicationManager::hash_block(data2);
        assert_ne!(hash1, hash3);
    }

    #[test]
    fn test_encryption_algorithms() {
        assert!(EncryptionManager::get_algorithm(1).is_some());
        assert!(EncryptionManager::get_algorithm(2).is_some());
        assert!(EncryptionManager::get_algorithm(99).is_none());

        let key = [0u8; 32];
        let iv = [0u8; 12];
        let data = b"Secret data";

        let algo = EncryptionManager::get_algorithm(1).unwrap();
        let encrypted = algo.encrypt(&key, data, &iv).unwrap();
        let decrypted = algo.decrypt(&key, &encrypted, &iv).unwrap();

        assert_eq!(data, decrypted.as_slice());
    }

    #[test]
    fn test_acl_flags() {
        let flags = ACL_FLAG_IMMUTABLE | ACL_FLAG_SECURE_DELETE;

        assert!(AclManager::is_immutable(flags));
        assert!(!AclManager::is_append_only(flags));
        assert!(AclManager::requires_secure_delete(flags));
    }
}
