//! OS cryptographic random number generation.
//!
//! 1.x read `/dev/urandom` directly in `security::encryption::fill_random`,
//! which is fine on Linux/macOS and wrong on Windows (no /dev). The PAL
//! version:
//!
//! * **unix**: `getentropy(2)` on the BSDs/macOS, the `getrandom(2)`
//!   syscall on Linux (via `libc::syscall`, with `/dev/urandom` as the
//!   fallback if the syscall is blocked by an old seccomp policy -- a
//!   real scenario on hardened containers).
//! * **Windows**: `ProcessPrng` (bcryptprimitives.dll, Windows 10
//!   1809+), falling back to `RtlGenRandom` (advapi32) on older builds.
//!
//! The fallback chain is depth-bounded and each hop returns a real error
//! rather than a weak substitute: CSPRNG is the one place where "degrade
//! gracefully" must never mean "degrade to a weaker source".

use std::io::Result;

/// Fills `out` with cryptographically secure random bytes.
pub fn fill_random(out: &mut [u8]) -> Result<()> {
    if out.is_empty() {
        return Ok(());
    }
    #[cfg(unix)]
    {
        unix_fill(out)
    }
    #[cfg(windows)]
    {
        windows_fill(out)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = out;
        Err(Error::new(
            std::io::ErrorKind::Unsupported,
            "no OS CSPRNG on this platform",
        ))
    }
}

#[cfg(unix)]
fn unix_fill(out: &mut [u8]) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        // getrandom(2): SYS_getrandom = 318 on x86_64, 278 on aarch64,
        // 355 on riscv64. `libc` exposes the constant on newer versions
        // via libc::getrandom? It exposes the *function* on glibc >= 2.25
        // and musl; call it if present, else fall back to urandom.
        #[cfg(any(target_env = "gnu", target_env = "musl"))]
        {
            // SAFETY: writes exactly out.len() bytes into `out`; the
            // function blocks until the crng is ready (or fails).
            let mut filled = 0;
            while filled < out.len() {
                let n = unsafe {
                    libc::getrandom(out[filled..].as_mut_ptr().cast(), out.len() - filled, 0)
                };
                if n < 0 {
                    // Interrupted or blocked: fall back to /dev/urandom
                    // rather than spinning.
                    return urandom_fill(out);
                }
                filled += n as usize;
            }
            return Ok(());
        }
        #[cfg(not(any(target_env = "gnu", target_env = "musl")))]
        {
            return urandom_fill(out);
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        // macOS and the BSDs: getentropy(2) caps at 256 bytes per call.
        let mut done = 0;
        while done < out.len() {
            let chunk = (out.len() - done).min(256);
            // SAFETY: writes exactly `chunk` bytes into out[done..], which
            // is a valid, exclusive slice for this call.
            let ret =
                unsafe { libc::getentropy(out[done..done + chunk].as_mut_ptr().cast(), chunk) };
            if ret != 0 {
                return urandom_fill(out);
            }
            done += chunk;
        }
        Ok(())
    }
}

#[cfg(unix)]
fn urandom_fill(out: &mut [u8]) -> Result<()> {
    use std::io::Read;
    // The documented, always-available unix fallback.
    let mut f = std::fs::File::open("/dev/urandom")?;
    f.read_exact(out)
}

#[cfg(windows)]
fn windows_fill(out: &mut [u8]) -> std::io::Result<()> {
    extern "system" {
        // bcryptprimitives!ProcessPrng, Windows 10 1809+.
        fn ProcessPrng(random_data: *mut u8, random_size: usize) -> u8;
        // advapi32!SystemFunction036 == RtlGenRandom, all supported builds.
        fn SystemFunction036(random_data: *mut u8, random_len: u32) -> u8;
    }
    // SAFETY: writes exactly out.len() bytes into `out`, which we
    // exclusively own for the duration of the call.
    let ok = unsafe { ProcessPrng(out.as_mut_ptr(), out.len()) };
    if ok != 0 {
        return Ok(());
    }
    // SAFETY: same contract, 32-bit length: chunk to satisfy it on huge
    // buffers (keys are 32 bytes; nonce 12; but keep it general).
    for chunk in out.chunks_mut(u32::MAX as usize) {
        // SAFETY: as above, with chunk.len() <= u32::MAX.
        let ok = unsafe { SystemFunction036(chunk.as_mut_ptr(), chunk.len() as u32) };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fills_and_is_nondeterministic() {
        let mut a = [0u8; 64];
        let mut b = [0u8; 64];
        fill_random(&mut a).unwrap();
        fill_random(&mut b).unwrap();
        // All-zero output would be a catastrophic CSPRNG failure.
        assert!(a.iter().any(|&x| x != 0));
        assert!(b.iter().any(|&x| x != 0));
        // Two draws agreeing on 512 bits of entropy is ~2^-512.
        assert_ne!(a, b);
    }

    #[test]
    fn empty_buffer_is_ok() {
        assert!(fill_random(&mut []).is_ok());
    }

    #[test]
    fn large_buffer_chunking() {
        // Exercises the 256-byte getentropy loop on macOS/BSD and any
        // chunking logic elsewhere.
        let mut buf = vec![0u8; 4096];
        fill_random(&mut buf).unwrap();
        assert!(buf.iter().any(|&x| x != 0));
    }
}
