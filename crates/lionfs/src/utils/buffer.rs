//! A small fixed-size buffer pool for block-sized (`BLOCK_SIZE`) buffers.
//!
//! `file::writer`/`disk::block_io` currently allocate a fresh
//! `[u8; BLOCK_SIZE]` (or `Vec<u8>`) for essentially every block read or
//! written. That's simple and correct, which is why the hot path wasn't
//! changed to use this -- swapping in pooled buffers there is a real
//! future optimization, but it changes ownership/lifetime patterns
//! throughout code that's already careful and working, and isn't done
//! blind. What's here is a real, usable, freestanding pool new code (or a
//! future targeted optimization pass) can build on.

use std::sync::Mutex;

pub struct BufferPool {
    buffer_size: usize,
    free: Mutex<Vec<Vec<u8>>>,
}

pub struct PooledBuffer<'a> {
    pool: &'a BufferPool,
    buf: Option<Vec<u8>>,
}

impl BufferPool {
    pub fn new(buffer_size: usize) -> Self {
        Self {
            buffer_size,
            free: Mutex::new(Vec::new()),
        }
    }

    /// Hands out a zeroed buffer of `buffer_size` bytes, reusing a
    /// previously-returned one if available.
    pub fn acquire(&self) -> PooledBuffer<'_> {
        let mut free = self.free.lock().unwrap();
        let mut buf = free.pop().unwrap_or_else(|| vec![0u8; self.buffer_size]);
        buf.iter_mut().for_each(|b| *b = 0);
        PooledBuffer {
            pool: self,
            buf: Some(buf),
        }
    }

    pub fn free_count(&self) -> usize {
        self.free.lock().unwrap().len()
    }
}

impl std::ops::Deref for PooledBuffer<'_> {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        self.buf.as_ref().unwrap()
    }
}

impl std::ops::DerefMut for PooledBuffer<'_> {
    fn deref_mut(&mut self) -> &mut [u8] {
        self.buf.as_mut().unwrap()
    }
}

impl Drop for PooledBuffer<'_> {
    fn drop(&mut self) {
        if let Some(buf) = self.buf.take() {
            let mut free = self.pool.free.lock().unwrap();
            // Cap how many buffers we hoard so a burst of activity doesn't
            // leave the pool permanently oversized.
            if free.len() < 256 {
                free.push(buf);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acquired_buffer_is_the_right_size_and_zeroed() {
        let pool = BufferPool::new(4096);
        let buf = pool.acquire();
        assert_eq!(buf.len(), 4096);
        assert!(buf.iter().all(|&b| b == 0));
    }

    #[test]
    fn buffers_are_reused_after_drop() {
        let pool = BufferPool::new(64);
        {
            let mut buf = pool.acquire();
            buf[0] = 0xFF;
        } // dropped, returned to pool
        assert_eq!(pool.free_count(), 1);
        let buf2 = pool.acquire();
        // Reused buffer must come back zeroed even though it held 0xFF before.
        assert_eq!(buf2[0], 0);
        assert_eq!(pool.free_count(), 0);
    }
}
