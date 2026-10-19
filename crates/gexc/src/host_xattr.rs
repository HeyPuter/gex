//! The host's `l*xattr`/`f*xattr` calls for gex-kernel's HostFs, which
//! forbids `unsafe` and so takes them as a hook (`set_host_xattr`).

use gex_kernel::errno::Errno;
use gex_kernel::vfs::hostfs::host_xattr::{set_host_xattr, On};
use gex_kernel::vfs::XattrOp;
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;

pub fn install() {
    set_host_xattr(call);
}

fn call(on: &On<'_>, op: XattrOp<'_>) -> Result<usize, Errno> {
    let path = match on {
        On::Path(p) => Some(CString::new(p.as_os_str().as_bytes()).map_err(|_| Errno::EINVAL)?),
        On::Fd(_) => None,
    };
    let fd = match on { On::Fd(fd) => *fd, On::Path(_) => -1 };
    let name_c = |n: &str| CString::new(n).map_err(|_| Errno::EINVAL);
    // SAFETY: every pointer is a live CString or slice for the call's length.
    let r: isize = unsafe {
        match op {
            XattrOp::Get { name, buf } => {
                let n = name_c(name)?;
                let (b, l) = (buf.as_mut_ptr() as *mut libc::c_void, buf.len());
                match &path {
                    Some(p) => libc::lgetxattr(p.as_ptr(), n.as_ptr(), b, l),
                    None => libc::fgetxattr(fd, n.as_ptr(), b, l),
                }
            }
            XattrOp::Set { name, value, flags } => {
                let n = name_c(name)?;
                let (v, l) = (value.as_ptr() as *const libc::c_void, value.len());
                (match &path {
                    Some(p) => libc::lsetxattr(p.as_ptr(), n.as_ptr(), v, l, flags as i32),
                    None => libc::fsetxattr(fd, n.as_ptr(), v, l, flags as i32),
                }) as isize
            }
            XattrOp::List { buf } => {
                let (b, l) = (buf.as_mut_ptr() as *mut libc::c_char, buf.len());
                match &path {
                    Some(p) => libc::llistxattr(p.as_ptr(), b, l),
                    None => libc::flistxattr(fd, b, l),
                }
            }
            XattrOp::Remove { name } => {
                let n = name_c(name)?;
                (match &path {
                    Some(p) => libc::lremovexattr(p.as_ptr(), n.as_ptr()),
                    None => libc::fremovexattr(fd, n.as_ptr()),
                }) as isize
            }
        }
    };
    if r < 0 {
        let e = std::io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO);
        return Err(Errno(e as _));
    }
    Ok(r as usize)
}
