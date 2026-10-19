//! The host's `openat2(RESOLVE_NO_SYMLINKS)` for gex-kernel's HostFs walk,
//! which forbids `unsafe` and so takes it as a hook (`set_host_no_links`):
//! one call says whether a path has a link on it, where the walk would
//! otherwise ask about each component.

use gex_kernel::vfs::hostfs::host_walk::set_host_no_links;
use std::ffi::CString;

pub fn install() {
    set_host_no_links(no_links);
}

/// `struct open_how` (linux/openat2.h).
#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}

const RESOLVE_NO_SYMLINKS: u64 = 0x04;

fn no_links(root: i32, rel: &str) -> bool {
    if rel.is_empty() {
        return true;
    }
    let Ok(path) = CString::new(rel) else { return false };
    let how = OpenHow { flags: (libc::O_PATH | libc::O_CLOEXEC) as u64, mode: 0, resolve: RESOLVE_NO_SYMLINKS };
    // SAFETY: a live CString and struct for the call's length.
    let fd = unsafe {
        libc::syscall(libc::SYS_openat2, root, path.as_ptr(), &how as *const OpenHow, std::mem::size_of::<OpenHow>())
    };
    if fd >= 0 {
        // SAFETY: the descriptor just opened, closed once.
        unsafe { libc::close(fd as i32) };
        return true;
    }
    // The walk stops at the first component missing, or not a directory,
    // before any link: there is none to follow up to there (ELOOP for a
    // link; ENOSYS and the rest cannot say).
    matches!(std::io::Error::last_os_error().raw_os_error(), Some(libc::ENOENT) | Some(libc::ENOTDIR))
}
