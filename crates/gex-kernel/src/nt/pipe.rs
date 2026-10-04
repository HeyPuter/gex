//! Named pipes in this process: `NtCreateNamedPipeFile` makes the server end,
//! opening `\??\pipe\<name>` makes the client end.
//!
//! kernelbase's `CreatePipe` is the user: an anonymous pipe is a named pipe
//! with a unique name (`Win32.Pipes.<pid>.<n>`) whose read end is the server
//! and whose write end is the client, opened straight after. Unity's players
//! make one at startup; `CreatePipe` loops until a create succeeds, so an
//! unimplemented create was a hang, not an error.
//!
//! A pipe is two of gex-kernel's byte pipes, one per direction, in a table
//! owned by the process (there is no other process to share one with: a pipe
//! served by another program is absent, as before). Byte mode only; message
//! mode is accepted and behaves as byte mode. A synchronous read waits for
//! data or for the other end to close; overlapped I/O on a pipe is reported
//! as unimplemented.

use super::{st, Call, Status, R};
use crate::errno::{Errno, SysResult};
use crate::pipe::Pipe;
use crate::vfs::{FileHandle, Metadata};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// One named pipe: the server end exists, the client end may.
#[derive(Debug)]
pub struct NamedPipe {
    /// Client → server.
    to_server: Arc<Pipe>,
    /// Server → client.
    to_client: Arc<Pipe>,
    /// A client has opened it (one instance: a second open is refused).
    connected: AtomicBool,
}

/// The process's pipes, by lower-cased name (`pipe\…`).
#[derive(Debug, Default)]
pub struct Pipes {
    by_name: Mutex<HashMap<String, Arc<NamedPipe>>>,
}

/// One end, as a file handle: reads one direction, writes the other.
#[derive(Debug)]
pub struct PipeEndHandle {
    pipe: Arc<NamedPipe>,
    server: bool,
    /// Removes the name when the server end closes.
    name: Option<(String, Arc<Pipes>)>,
}

impl PipeEndHandle {
    fn incoming(&self) -> &Arc<Pipe> {
        if self.server { &self.pipe.to_server } else { &self.pipe.to_client }
    }
    fn outgoing(&self) -> &Arc<Pipe> {
        if self.server { &self.pipe.to_client } else { &self.pipe.to_server }
    }
}

impl Drop for PipeEndHandle {
    fn drop(&mut self) {
        // Our read side of one direction and write side of the other.
        self.incoming().close_end(false);
        self.outgoing().close_end(true);
        if let Some((name, pipes)) = &self.name {
            pipes.by_name.lock().unwrap().remove(name);
        }
    }
}

impl FileHandle for PipeEndHandle {
    fn read_at(&self, _offset: u64, buf: &mut [u8]) -> SysResult {
        self.incoming().read(buf)
    }
    fn write_at(&self, _offset: u64, buf: &[u8]) -> SysResult {
        self.outgoing().write(buf)
    }
    fn metadata(&self) -> Result<Metadata, Errno> {
        Ok(Metadata::regular(0))
    }
    fn is_stream(&self) -> bool {
        true
    }
    fn poll_readable(&self) -> bool {
        self.incoming().readable()
    }
    fn available(&self) -> Option<u64> {
        Some(self.incoming().buffered() as u64)
    }
}

impl Pipes {
    /// The pipe `name` (lower-cased, `pipe\…`), if a server made it.
    fn get(&self, name: &str) -> Option<Arc<NamedPipe>> {
        self.by_name.lock().unwrap().get(name).cloned()
    }

    /// Whether a server has the pipe `name` open.
    pub fn exists(&self, name: &str) -> bool {
        self.by_name.lock().unwrap().contains_key(name)
    }
}

/// `\??\pipe\x` → `pipe\x`, lower-cased; `None` for any other path.
pub fn pipe_name(rest_lower: &str) -> Option<String> {
    rest_lower.starts_with("pipe\\").then(|| rest_lower.to_string())
}

/// The server end of a new pipe called `name`. A name in use is
/// `ACCESS_DENIED`, Windows' answer once a pipe's instance limit is reached
/// (gex serves one instance); CreatePipe then tries the next name.
pub fn create_server(pipes: &Arc<Pipes>, name: String) -> R<Arc<dyn FileHandle>> {
    let mut by_name = pipes.by_name.lock().unwrap();
    if by_name.contains_key(&name) {
        return Err(Status(st::ACCESS_DENIED));
    }
    let pipe = Arc::new(NamedPipe {
        to_server: Arc::new(Pipe::new()),
        to_client: Arc::new(Pipe::new()),
        connected: AtomicBool::new(false),
    });
    by_name.insert(name.clone(), Arc::clone(&pipe));
    Ok(Arc::new(PipeEndHandle { pipe, server: true, name: Some((name, Arc::clone(pipes))) }))
}

/// The client end of the pipe `name`.
pub fn open_client(pipes: &Pipes, name: &str) -> R<Arc<dyn FileHandle>> {
    let pipe = pipes.get(name).ok_or(Status(st::OBJECT_NAME_NOT_FOUND))?;
    if pipe.connected.swap(true, Ordering::AcqRel) {
        return Err(Status(st::PIPE_BUSY));
    }
    Ok(Arc::new(PipeEndHandle { pipe, server: false, name: None }))
}

/// A synchronous read: data, or 0 once the writer has gone (end of file),
/// waiting in between. Polls in short slices on the thread's wait word, so
/// the process ending or an APC still gets through.
pub fn read_blocking(c: &mut Call<'_>, h: &Arc<dyn FileHandle>, buf: &mut [u8]) -> R<usize> {
    let mut slice = Duration::from_micros(200);
    loop {
        match h.read_at(0, buf) {
            Ok(n) => return Ok(n as usize),
            Err(Errno::EAGAIN) => {}
            Err(_) => return Err(Status(st::PIPE_BROKEN)),
        }
        if c.p.terminating.load(Ordering::Relaxed) {
            return Err(Status(st::PIPE_BROKEN));
        }
        let t = std::sync::Arc::clone(&c.t);
        super::sync::sleep_on_word(c, &t, Some(slice));
        slice = (slice * 2).min(Duration::from_millis(10));
    }
}

/// A synchronous write: all of `data`, waiting for room, or PIPE_BROKEN once
/// the reader has gone.
pub fn write_blocking(c: &mut Call<'_>, h: &Arc<dyn FileHandle>, data: &[u8]) -> R<usize> {
    let mut done = 0;
    let mut slice = Duration::from_micros(200);
    while done < data.len() {
        match h.write_at(0, &data[done..]) {
            Ok(n) if n > 0 => {
                done += n as usize;
                slice = Duration::from_micros(200);
                continue;
            }
            Ok(_) | Err(Errno::EAGAIN) => {}
            Err(_) => return Err(Status(st::PIPE_BROKEN)),
        }
        if c.p.terminating.load(Ordering::Relaxed) {
            return Err(Status(st::PIPE_BROKEN));
        }
        let t = std::sync::Arc::clone(&c.t);
        super::sync::sleep_on_word(c, &t, Some(slice));
        slice = (slice * 2).min(Duration::from_millis(10));
    }
    Ok(done)
}
