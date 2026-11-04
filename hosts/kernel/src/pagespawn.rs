//! A program the page starts (page/runtime programs.js `spawn`), in one call:
//! the program found, its process made, its terminal or pipes made with the
//! page's ends held under pid 0, and what its Worker starts from (the exec
//! record, its descriptors, a native wasm program's image span).

use std::sync::Arc;

use gex_kernel::errno::Errno;
use gex_kernel::vfs::{FileHandle, FileSystem, FileType, Metadata};

use crate::{pagefs, rootfs};

/// The pid the page's own ends are held under: no process has it.
const HOST: i32 = 0;
const O_WRONLY: u32 = 1;
const O_RDWR: u32 = 2;
/// The PATH a program is looked up on when the environment has none.
const PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
/// The fixed part of the answer, before the exec record and the descriptors.
const HEAD: usize = 64;

/// `path` with its links followed, from the root and through the page's
/// mounts.
fn reach(path: &str) -> Result<String, Errno> {
    let w = rootfs().walk(path, true, &pagefs::with_points(""))?;
    Ok(w.path.trim_start_matches('/').to_string())
}

fn meta(p: &str) -> Result<Metadata, Errno> {
    match pagefs::find(p) {
        Some((m, rel)) => m.metadata(&rel),
        None => rootfs().metadata(p),
    }
}

fn open(p: &str) -> Result<Arc<dyn FileHandle>, Errno> {
    match pagefs::find(p) {
        Some((m, rel)) => m.open(&rel, 0, 0),
        None => rootfs().open(p, 0, 0),
    }
}

/// 0 when the file at `path` can be run, else a negated errno.
fn usable(path: &str) -> i32 {
    match reach(path).and_then(|p| meta(&p)) {
        Ok(m) if m.file_type == FileType::Regular && m.mode & 0o111 != 0 => 0,
        Ok(_) => -13,
        Err(e) => -e.0,
    }
}

/// The program `file` names: itself with a slash in it (from `cwd` when
/// relative), else the first that can run of that name on `PATH` (execvp's
/// search). A negated errno when there is none: EACCES when one was found
/// that cannot run, else ENOENT.
fn find(file: &str, path: Option<&str>, cwd: &str) -> Result<String, i32> {
    if file.contains('/') {
        let p = if file.starts_with('/') { file.to_string() } else { format!("{}/{file}", cwd.trim_end_matches('/')) };
        return match usable(&p) {
            0 => Ok(p),
            e => Err(e),
        };
    }
    let mut best = -2;
    for d in path.unwrap_or(PATH).split(':') {
        let p = if d.is_empty() { file.to_string() } else { format!("{}/{file}", d.trim_end_matches('/')) };
        match usable(&p) {
            0 => return Ok(p),
            -13 => best = -13,
            _ => {}
        }
    }
    Err(best)
}

/// The image span of a native wasm program that runs no translated code
/// (one whose header asks for no Windows address space): the shim's
/// vfork.rs `exec_wasm_image`, read here from the file.
fn wasm_image(path: &str) -> Option<(u64, u64)> {
    let h = open(&reach(path).ok()?).ok()?;
    let span = gex_ld::elf::wasm_program_span(&mut |off, buf| h.read_at(off, buf).ok().map(|n| n as usize));
    span.filter(|&(_, _, flags)| flags & 3 == 0).map(|(lo, hi, _)| (lo, hi))
}

/// Start a program for the page. `spec` is NUL-terminated fields: the stdio
/// (`pty`, `pipe` or `null`), the terminal's columns and rows, `1` for the
/// init (pid 1, made by nobody), the working directory, then the arguments
/// and the environment, each after its count, as the exec record has them
/// (vfork.rs `stage_exec_parts`).
///
/// The answer, into `out`: twelve little-endian words (the pid; the
/// terminal, stdin, stdout and stderr ids, -1 for none; their wait queues;
/// whether there is an image span; the exec record's and the descriptors'
/// lengths; two unused), the image span's two 64-bit ends, then the exec
/// record and the descriptor lines (host.mjs `spawnExec`). Its length, or a
/// negated errno: ENOENT or EACCES for no program, ERANGE when `cap` is too
/// small (nothing is made).
#[no_mangle]
pub extern "C" fn k_page_spawn(spec: gex_kmem::PAddr, spec_len: u32, out: gex_kmem::PAddr, cap: u32) -> i32 {
    let mut raw = vec![0u8; spec_len as usize];
    if spec_len > 0 {
        unsafe { gex_kmem::pull_checked(raw.as_mut_ptr(), spec, spec_len) };
    }
    let fields: Vec<&[u8]> = raw.split(|&b| b == 0).collect();
    let text = |i: usize| fields.get(i).map(|b| String::from_utf8_lossy(b).into_owned()).unwrap_or_default();
    let (stdio, cols, rows, init, cwd) = (text(0), text(1), text(2), text(3) == "1", text(4));
    // The arguments and environment, as they came: the exec record's tail.
    let tail_at = fields[..5.min(fields.len())].iter().map(|f| f.len() + 1).sum::<usize>().min(raw.len());
    let tail = &raw[tail_at..];
    let argc: usize = text(5).parse().unwrap_or(0);
    let envc_at = 6 + argc;
    let envc: usize = text(envc_at).parse().unwrap_or(0);
    let path_var = (envc_at + 1..envc_at + 1 + envc).map(text).find_map(|e| e.strip_prefix("PATH=").map(str::to_string));
    let cwd = if cwd.is_empty() { "/".to_string() } else { cwd };

    let prog = match find(&text(6), path_var.as_deref(), &cwd) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let mut exec = Vec::with_capacity(prog.len() + 1 + tail.len());
    exec.extend_from_slice(prog.as_bytes());
    exec.push(0);
    exec.extend_from_slice(tail);
    // The descriptors are a few short lines.
    if HEAD + exec.len() + 256 > cap as usize {
        return -34;
    }
    let image = wasm_image(&prog);

    let pid = if init { gex_kernel::INIT_PID } else { crate::k_spawn(gex_kernel::INIT_PID) };
    if !init {
        crate::k_setsid(pid);
    }
    let mut ids = [-1i32; 4];
    let mut queues = [0u32; 3];
    let mut fds = String::new();
    match stdio.as_str() {
        "pty" => {
            let id = crate::k_pty_open_master(HOST) >> 16;
            crate::k_pty_set_lock(id, 0);
            let (cols, rows) = (cols.parse::<u64>().unwrap_or(80) & 0xffff, rows.parse::<u64>().unwrap_or(24) & 0xffff);
            crate::k_pty_set_winsize(id, (rows << 48) | (cols << 32));
            for fd in 0..3 {
                crate::k_pty_open_end(id, 0, pid);
                fds += &format!("{fd} 0 {O_RDWR} 0 pty:[{id}:s]\n");
            }
            // Its controlling terminal, with its group in the foreground, as
            // a session leader's open of the slave makes it (syscalls.rs
            // `open_pty`).
            crate::k_pty_set_sid(id, pid);
            crate::k_pty_set_fg(id, pid);
            ids[0] = id;
            queues[0] = crate::k_pty_queue(id);
        }
        "null" => {
            for fd in 0..3 {
                fds += &format!("{fd} 0 {O_RDWR} 0 /dev/null\n");
            }
        }
        _ => {
            // Three pipes. The far end's first reference becomes the
            // program's; the page keeps the near one.
            for fd in 0..3i32 {
                let id = crate::k_pipe_new(HOST);
                let child_writes = (fd != 0) as i32;
                crate::k_pipe_open_end(id, child_writes, pid);
                crate::k_pipe_close_end(id, child_writes, HOST);
                fds += &format!("{fd} 0 {} 0 pipe:[{id}]\n", if child_writes != 0 { O_WRONLY } else { 0 });
                ids[1 + fd as usize] = id;
                queues[fd as usize] = crate::k_pipe_queue(id);
            }
        }
    }

    let (lo, hi) = image.unwrap_or((0, 0));
    let words = [
        pid as u32, ids[0] as u32, ids[1] as u32, ids[2] as u32, ids[3] as u32,
        queues[0], queues[1], queues[2], image.is_some() as u32, exec.len() as u32, fds.len() as u32, 0,
    ];
    let mut b = Vec::with_capacity(HEAD + exec.len() + fds.len());
    for w in words {
        b.extend_from_slice(&w.to_le_bytes());
    }
    b.extend_from_slice(&lo.to_le_bytes());
    b.extend_from_slice(&hi.to_le_bytes());
    b.extend_from_slice(&exec);
    b.extend_from_slice(fds.as_bytes());
    unsafe { gex_kmem::push_checked(out, b.as_ptr(), b.len() as u32) };
    b.len() as i32
}
