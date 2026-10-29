//! The only unsafe/FFI in the crate: three libc calls std does not expose.
//! libc is already linked by std, so this adds no dependency.

use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::net::UnixStream;

#[repr(C)]
struct IoVec {
    base: *const u8,
    len: usize,
}

// Linux 64-bit layout (glibc; musl is bit-compatible on little endian).
#[repr(C)]
struct MsgHdr {
    name: *const u8,
    namelen: u32,
    iov: *const IoVec,
    iovlen: usize,
    control: *const u8,
    controllen: usize,
    flags: i32,
}

extern "C" {
    fn sendmsg(fd: i32, msg: *const MsgHdr, flags: i32) -> isize;
    fn memfd_create(name: *const u8, flags: u32) -> i32;
    fn getuid() -> u32;
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    fn munmap(addr: *mut u8, len: usize) -> i32;
}

const PROT_READ: i32 = 1;
const PROT_WRITE: i32 = 2;
const MAP_SHARED: i32 = 1;
const MAP_PRIVATE: i32 = 2;

const SOL_SOCKET: i32 = 1;
const SCM_RIGHTS: i32 = 1;
const MSG_NOSIGNAL: i32 = 0x4000;
const MFD_CLOEXEC: u32 = 1;

/// Current user id (D-Bus EXTERNAL authentication sends it).
pub fn uid() -> u32 {
    // SAFETY: getuid has no preconditions and cannot fail.
    unsafe { getuid() }
}

/// Write all of `data` to `sock`, attaching `fd` (SCM_RIGHTS) to the first
/// bytes. Wayland needs this to hand the shared-memory file to the compositor.
pub fn send_with_fd(sock: &UnixStream, data: &[u8], fd: RawFd) -> io::Result<()> {
    use std::io::Write;
    if data.is_empty() {
        return Ok(());
    }
    // cmsghdr { len: usize, level: i32, type: i32 } followed by the fd, padded to 8
    let mut cmsg = [0u8; 24];
    cmsg[0..8].copy_from_slice(&(20usize).to_ne_bytes()); // CMSG_LEN(4) = 16 + 4
    cmsg[8..12].copy_from_slice(&SOL_SOCKET.to_ne_bytes());
    cmsg[12..16].copy_from_slice(&SCM_RIGHTS.to_ne_bytes());
    cmsg[16..20].copy_from_slice(&fd.to_ne_bytes());

    let iov = IoVec { base: data.as_ptr(), len: data.len() };
    let hdr = MsgHdr {
        name: std::ptr::null(),
        namelen: 0,
        iov: &iov,
        iovlen: 1,
        control: cmsg.as_ptr(),
        controllen: cmsg.len(),
        flags: 0,
    };
    // SAFETY: `hdr`, `iov`, `cmsg` and `data` all outlive the call, and the
    // lengths match the buffers they describe.
    let n = unsafe { sendmsg(sock.as_raw_fd(), &hdr, MSG_NOSIGNAL) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    let sent = n as usize;
    if sent < data.len() {
        // the fd went with the first chunk; the rest is plain data
        let mut s = sock;
        s.write_all(&data[sent..])?;
    }
    Ok(())
}

/// Anonymous in-memory file (for Wayland shared-memory buffers).
pub fn memfd(name: &str, size: u64) -> io::Result<File> {
    let mut cname = name.as_bytes().to_vec();
    cname.push(0);
    // SAFETY: `cname` is NUL-terminated and lives across the call.
    let fd = unsafe { memfd_create(cname.as_ptr(), MFD_CLOEXEC) };
    if fd >= 0 {
        // SAFETY: memfd_create returned a fresh fd that nothing else owns.
        let f = unsafe { File::from_raw_fd(fd) };
        f.set_len(size)?;
        return Ok(f);
    }
    // Fallback: an unlinked file in the runtime dir (tmpfs on every desktop).
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("/dev/shm"));
    let path = dir.join(format!("notify-rs-shm-{}-{}", std::process::id(), size));
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)?;
    let _ = std::fs::remove_file(&path);
    f.set_len(size)?;
    Ok(f)
}

/// A memory-mapped file. Fonts use this read-only (only the pages that are
/// touched count, and they are shared page cache); Wayland buffers use it
/// read-write so frames are drawn straight into the memory the compositor reads.
pub struct Map {
    ptr: *mut u8,
    len: usize,
    writable: bool,
}

// SAFETY: the mapping is plain memory owned by this struct; access goes
// through &self / &mut self slices like a Vec<u8>.
unsafe impl Send for Map {}
unsafe impl Sync for Map {}

impl Map {
    /// Map the whole file read-only (private).
    pub fn open_ro(file: &File) -> io::Result<Map> {
        let len = file.metadata()?.len() as usize;
        if len == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "empty file"));
        }
        // SAFETY: valid fd, nonzero length, kernel chooses the address.
        let p = unsafe { mmap(std::ptr::null_mut(), len, PROT_READ, MAP_PRIVATE, file.as_raw_fd(), 0) };
        if p as isize == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(Map { ptr: p, len, writable: false })
    }

    /// Map the first `len` bytes of `file` shared and writable.
    pub fn shared_rw(file: &File, len: usize) -> io::Result<Map> {
        if len == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "zero length"));
        }
        // SAFETY: as above; the file was sized to at least `len` by the caller.
        let p = unsafe { mmap(std::ptr::null_mut(), len, PROT_READ | PROT_WRITE, MAP_SHARED, file.as_raw_fd(), 0) };
        if p as isize == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(Map { ptr: p, len, writable: true })
    }

    pub fn len(&self) -> usize {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: ptr/len describe a live mapping for as long as `self` exists.
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
    /// Writable view; panics on a read-only map.
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        assert!(self.writable, "read-only mapping");
        // SAFETY: as above, and `&mut self` guarantees exclusive access on our side.
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }
}

impl Drop for Map {
    fn drop(&mut self) {
        // SAFETY: ptr/len came from a successful mmap and are unmapped once.
        unsafe {
            munmap(self.ptr, self.len);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn maps_a_file_read_only() {
        let dir = std::env::temp_dir().join(format!("notify-rs-map-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("f.bin");
        std::fs::File::create(&p).unwrap().write_all(b"hello map").unwrap();
        let m = Map::open_ro(&std::fs::File::open(&p).unwrap()).unwrap();
        assert_eq!(m.as_slice(), b"hello map");
        assert!(Map::open_ro(&std::fs::File::create(dir.join("empty")).unwrap()).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn shared_memory_is_writable_and_visible_through_the_file() {
        use std::os::unix::fs::FileExt;
        let f = memfd("test", 4096).unwrap();
        let mut m = Map::shared_rw(&f, 4096).unwrap();
        m.as_mut_slice()[10] = 0xab;
        let mut b = [0u8; 1];
        f.read_exact_at(&mut b, 10).unwrap();
        assert_eq!(b[0], 0xab, "what we draw is what the other side reads");
    }

    #[test]
    fn uid_is_stable() {
        assert_eq!(uid(), uid());
    }
}
