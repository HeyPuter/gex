//! Reaching the X server from a guest process, for [`gex_xwm`].
//!
//! The manager itself is a crate: the protocol is the same wherever it runs.
//! What is particular to a guest process is finding the socket Xwayland bound,
//! which is either in the global kernel (so a manager in one process can reach
//! a server in another) or in this process's own name table when there is no
//! global kernel at all.

use gex_kernel::unix::{RemoteUnix, UnixSocket};
use gex_kernel::vfs::FileHandle;
use std::sync::Arc;

pub use gex_xwm::Xwm;

/// Where rootless Xwayland listens. Both spellings: a filesystem name and the
/// abstract one Linux writes with a leading NUL, which is what libxcb tries
/// first and therefore what Xwayland binds.
const X_SOCKETS: &[&str] = &["/tmp/.X11-unix/X0", "\0/tmp/.X11-unix/X0"];

/// Connect to the X server this process is running, if it is running one.
pub(crate) fn connect(cx: &crate::wl::ProcCx) -> Option<Xwm> {
    Xwm::start(open(cx)?)
}

/// The socket, whichever kind of name table holds the X server.
fn open(cx: &crate::wl::ProcCx) -> Option<Arc<dyn FileHandle>> {
    for name in X_SOCKETS {
        if let Some(r) = cx.unix_remote {
            // (the window manager is the page's, not the process's: root, no pid)
            let id = (r.connect)(name.as_ptr() as gex_kernel::PAddr, name.len() as u32, 0, 0, 0, 0, 0);
            if id >= 0 {
                unsafe {
                    crate::write_stderr(&format!(
                        "[xwm] connected to the X server on {:?} (global kernel)\n",
                        name
                    ));
                }
                let sock = RemoteUnix::new(id, false, Some((*name).to_string()), r);
                sock.set_nonblocking(true);
                return Some(Arc::new(sock));
            }
        }
        let sock = Arc::new(UnixSocket::new(true));
        if cx.socket_names.connect(&sock, name).is_ok() {
            unsafe {
                crate::write_stderr(&format!(
                    "[xwm] connected to the X server on {name:?}\n"
                ));
            }
            sock.set_nonblocking(true);
            return Some(sock);
        }
    }
    None
}

