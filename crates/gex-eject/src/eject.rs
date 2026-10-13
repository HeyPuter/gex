//! `gexc eject <record-dir> --out <project>`: write a project from a session
//! `gexc run --record` recorded.
//!
//! The project splits into three kinds of file:
//!
//! - **generated**: `gex-project.json`, `rootfs/`, `session/`, `runtime/`,
//!   `gen/`, `build.sh`, `run.sh`, `.gitignore`. Rewritten by every eject,
//!   and only from its inputs, so the same session and runtime give the same
//!   bytes.
//! - **user**: everything under `user/`. Created from templates when absent,
//!   never written after that.
//! - **build output**: `build/`. Made by `build.sh`; deleted by an eject,
//!   since it was built from what the eject replaces.

use crate::config;
use crate::project::{rootfs_rel, File, Object, Program, Project, PROJECT_FILE};
use crate::templates;
use anyhow::{anyhow, bail, Context, Result};
use gex_xlate::profile::Session;
use std::path::{Path, PathBuf};

/// Everything an eject writes apart from `user/` and `build/`.
pub const GENERATED: &[&str] = &[
    PROJECT_FILE,
    "rootfs",
    "session",
    "runtime",
    "gen",
    "build.sh",
    "run.sh",
    "run.mjs",
    "serve.mjs",
    // The app library's entry (a Linux program's `start()`); index.html is
    // the user's now (user/index.html), removed here from older projects.
    "app.mjs",
    "index.html",
    ".gitignore",
    // A W3 page's (w3.rs).
    "bundle",
    "units",
];

pub struct EjectOptions {
    /// The gexc to copy into `runtime/`: the one running.
    pub runtime: PathBuf,
    /// Its build fingerprint, when it has one.
    pub runtime_build: Option<String>,
    /// gex's browser shim (`gex_browser.wasm`, built from the same source for
    /// the same guest), for a project with a web build.
    pub web_shim: Option<PathBuf>,
    /// The app library (web/build-eject-page.sh's output: app.mjs, its
    /// chunks and the runtime's wasm), for a Windows program's web build in a
    /// browser: vendored as runtime/web/page.
    pub web_page: Option<PathBuf>,
}

#[derive(Debug, Default)]
pub struct Summary {
    pub objects: usize,
    pub files: usize,
    pub bytes: u64,
    pub missing: usize,
    pub outputs: usize,
    /// The project existed and its `user/` was kept.
    pub kept_user: bool,
    /// It has the browser runtime.
    pub web: bool,
}

/// `sha256:<hex>` of some bytes.
pub fn sha256(bytes: &[u8]) -> String {
    let mut h = gex_cache::sha256::Sha256::new();
    h.update(bytes);
    format!("sha256:{}", gex_cache::sha256::hex(&h.finish()))
}

/// Paths the guest sees that gex synthesises: never copied, never missing.
fn synthetic(path: &str) -> bool {
    ["/proc", "/dev", "/sys"]
        .iter()
        .any(|p| path == *p || path.starts_with(&format!("{p}/")))
}

pub(crate) fn write_file(path: &Path, bytes: &[u8], executable: bool) -> Result<()> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d).with_context(|| format!("creating {}", d.display()))?;
    }
    std::fs::write(path, bytes).with_context(|| format!("writing {}", path.display()))?;
    use std::os::unix::fs::PermissionsExt;
    let mode = if executable { 0o755 } else { 0o644 };
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .with_context(|| format!("setting the mode of {}", path.display()))?;
    Ok(())
}

/// Give `path` the modification time of `like`: a program may record it
/// (gzip puts it in its header), so a copy must keep it to behave the same.
pub fn keep_mtime(path: &Path, like: &std::fs::Metadata) -> Result<()> {
    let t = like.modified().context("reading a modification time")?;
    std::fs::File::options()
        .write(true)
        .open(path)
        .and_then(|f| f.set_modified(t))
        .with_context(|| format!("setting the modification time of {}", path.display()))
}

/// A guest path without `.` components or a trailing slash.
fn normalize(p: &str) -> String {
    let parts: Vec<&str> = p.split('/').filter(|c| !c.is_empty() && *c != ".").collect();
    format!("/{}", parts.join("/"))
}

/// Make `out` ready for a (re-)eject: new, empty, or a project whose
/// generated files and build are removed and whose `user/` stays.
pub(crate) fn prepare(out: &Path) -> Result<bool> {
    if !out.exists() {
        std::fs::create_dir_all(out).with_context(|| format!("creating {}", out.display()))?;
        return Ok(false);
    }
    let entries: Vec<_> = std::fs::read_dir(out)
        .with_context(|| format!("reading {}", out.display()))?
        .collect::<std::io::Result<_>>()?;
    if entries.is_empty() {
        return Ok(false);
    }
    if !out.join(PROJECT_FILE).exists() {
        bail!(
            "{} exists and is not an ejected project (no {PROJECT_FILE}); refusing to write into it",
            out.display()
        );
    }
    for g in GENERATED.iter().chain(["build"].iter()) {
        let p = out.join(g);
        match std::fs::symlink_metadata(&p) {
            Ok(m) if m.is_dir() => std::fs::remove_dir_all(&p),
            Ok(_) => std::fs::remove_file(&p),
            Err(_) => continue,
        }
        .with_context(|| format!("removing {}", p.display()))?;
    }
    Ok(out.join("user").exists())
}

/// Where the recorded run read a guest path from.
fn host_path(sysroot: Option<&str>, guest: &str) -> PathBuf {
    match sysroot {
        Some(r) => Path::new(r).join(guest.trim_start_matches('/')),
        None => PathBuf::from(guest),
    }
}

pub fn eject(record: &Path, out: &Path, o: &EjectOptions) -> Result<Summary> {
    let session_path = record.join("session.json");
    let session_bytes = std::fs::read(&session_path).with_context(|| {
        format!("reading {} (record one with `gexc run --record DIR`)", session_path.display())
    })?;
    let session = Session::from_json(
        std::str::from_utf8(&session_bytes).map_err(|_| anyhow!("session.json is not UTF-8"))?,
    )
    .map_err(|e| anyhow!("{}: {e}", session_path.display()))?;
    let profile_bytes = match std::fs::read(record.join("profile.json")) {
        Ok(b) => Some(b),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(anyhow!("reading {}/profile.json: {e}", record.display())),
    };
    if session.isa != gex_abi::ARCH_NAME {
        bail!(
            "the session is of a {} guest and this gexc runs {} guests; eject it with the \
             other build",
            session.isa,
            gex_abi::ARCH_NAME
        );
    }
    // A Windows program's run is one process whose program is the NT
    // layer's (docs/eject-pe.md).
    let nt = session.nt.as_ref();
    let (exe, argv) = match nt {
        Some(n) => (n.exe.clone(), std::iter::once(n.exe.clone()).chain(n.args.iter().cloned()).collect::<Vec<_>>()),
        None => {
            let first = session.programs.first().ok_or_else(|| anyhow!("the session ran no program"))?;
            (first.exe.clone(), session.argv.clone())
        }
    };
    let sysroot = session.sysroot.as_deref();

    let kept_user = prepare(out)?;
    let mut summary = Summary {
        kept_user,
        ..Summary::default()
    };

    // The closure, each object checked against the hash the session saw.
    let mut objects = Vec::new();
    for ob in &session.objects {
        if objects.iter().any(|x: &Object| x.path == ob.path) {
            continue;
        }
        let src = match &ob.host_path {
            Some(h) => PathBuf::from(h),
            None => host_path(sysroot, &ob.path),
        };
        let bytes = std::fs::read(&src)
            .with_context(|| format!("reading {} ({}), an object of the session", src.display(), ob.path))?;
        let id = sha256(&bytes);
        if id != ob.id {
            bail!(
                "{} is not the file the session ran: it is {id}, the session recorded {}. \
                 Record the session again",
                src.display(),
                ob.id
            );
        }
        let dst = out.join(rootfs_rel(&ob.path)?);
        write_file(&dst, &bytes, true)?;
        keep_mtime(&dst, &std::fs::metadata(&src)?)?;
        summary.bytes += bytes.len() as u64;
        objects.push(Object {
            id,
            path: ob.path.clone(),
            role: if nt.is_some() && ob.path == exe { "exe".into() } else { ob.role.clone() },
            size: bytes.len() as u64,
        });
    }
    if !objects.iter().any(|x| x.path == exe) {
        bail!("the session's program {exe} is not among its objects (was it recorded under `anon`?)");
    }

    // The files it read. Created and write-only files are its output; failed
    // opens stay missing; directories are made.
    let mut files = Vec::new();
    let mut dirs = std::collections::BTreeSet::new();
    let (mut missing, mut outputs) = (Vec::new(), Vec::new());
    // A Windows program's prefix: the build writes its own image layout, and
    // the NT layer checks the prefix's Wine version.
    let mut nt_files: Vec<gex_xlate::profile::FileEntry> = Vec::new();
    if let Some(n) = nt {
        let v = format!("{}/.wine-version", n.prefix);
        if !session.files.contains_key(&v) {
            nt_files.push(gex_xlate::profile::FileEntry { path: v, read: true, ok: 1, ..Default::default() });
        }
    }
    let layout_file = nt.map(|n| format!("{}/{}", n.prefix, gex_kernel::nt::section::LAYOUT_FILE));
    for f in session.files.values().chain(nt_files.iter()) {
        if layout_file.as_deref() == Some(f.path.as_str()) {
            continue;
        }
        if !f.path.starts_with('/') {
            // `<fd N>/name`: relative to a directory nobody could name.
            bail!("the session opened {:?}, relative to a directory with no known path", f.path);
        }
        let p = &normalize(&f.path);
        if synthetic(p) || objects.iter().any(|x| &x.path == p) {
            continue;
        }
        if f.ok == 0 {
            missing.push(p.clone());
            continue;
        }
        if f.directory {
            dirs.insert(p.clone());
            continue;
        }
        if f.create || !f.read {
            outputs.push(p.clone());
            continue;
        }
        if p == "/tmp" || p.starts_with("/tmp/") {
            bail!(
                "the session read {p}, but a project's /tmp is a fresh tmpfs (as under any \
                 --sysroot); keep inputs outside /tmp and record again"
            );
        }
        let src = host_path(sysroot, p);
        let meta = match std::fs::metadata(&src) {
            Ok(m) => m,
            // A Windows program's file made and removed during the session
            // by a way the NT layer's open hook does not see as a create
            // (a rename, a copy): its output.
            Err(_) if nt.is_some() => {
                outputs.push(p.clone());
                continue;
            }
            Err(e) => return Err(anyhow!("the session read {p}, and {} is gone: {e}", src.display())),
        };
        if meta.is_dir() {
            dirs.insert(p.clone());
            continue;
        }
        if !meta.is_file() {
            bail!("the session read {p}, and {} is not a regular file", src.display());
        }
        let bytes = std::fs::read(&src).with_context(|| format!("reading {}", src.display()))?;
        let executable = {
            use std::os::unix::fs::PermissionsExt;
            meta.permissions().mode() & 0o111 != 0
        };
        let dst = out.join(rootfs_rel(p)?);
        write_file(&dst, &bytes, executable)?;
        keep_mtime(&dst, &meta)?;
        summary.bytes += bytes.len() as u64;
        files.push(File {
            path: p.clone(),
            id: sha256(&bytes),
            size: bytes.len() as u64,
        });
    }
    if let Some(c) = &session.cwd {
        dirs.insert(normalize(c));
    }
    for d in &dirs {
        let p = out.join(rootfs_rel(d)?);
        std::fs::create_dir_all(&p).with_context(|| format!("creating {}", p.display()))?;
    }

    // The session itself, for the build (the profile seeds translation) and
    // for the reader.
    write_file(&out.join("session/session.json"), &session_bytes, false)?;
    if let Some(p) = &profile_bytes {
        write_file(&out.join("session/profile.json"), p, false)?;
    }

    // The runtime. A hard link when it can be (the same bytes, without
    // another hundred megabytes per project), a copy otherwise.
    let runtime = std::fs::read(&o.runtime).with_context(|| format!("reading {}", o.runtime.display()))?;
    let rt = out.join("runtime/gexc");
    std::fs::create_dir_all(out.join("runtime")).context("creating runtime/")?;
    if std::fs::hard_link(&o.runtime, &rt).is_err() {
        write_file(&rt, &runtime, true)?;
    }
    // A Windows program's AOT-only runtime (gex-rt: no translator, no wasm
    // compiler), from `GEX_RT` or beside this gexc.
    if nt.is_some() {
        let found = std::env::var_os("GEX_RT")
            .map(PathBuf::from)
            .or_else(|| o.runtime.parent().map(|d| d.join("gex-rt")))
            .filter(|p| p.is_file());
        // Without it run.sh's default would be gexc, which holds the
        // translator (docs/eject-pe.md §0): refuse rather than ship that.
        let Some(src) = found else {
            bail!(
                "no gex-rt for this Windows program's project: set GEX_RT to a gex-rt binary \
                 (cargo build --release -p gex-rt --features x86 --target-dir target/x86-rt) \
                 or put one beside {}",
                o.runtime.display()
            );
        };
        let dst = out.join("runtime/gex-rt");
        if std::fs::hard_link(&src, &dst).is_err() {
            write_file(&dst, &std::fs::read(&src)?, true)?;
        }
    }

    let project = Project {
        isa: session.isa.clone(),
        program: Program {
            exe: exe.clone(),
            argv,
            env: session.env.clone(),
            cwd: session.cwd.clone(),
        },
        objects,
        files,
        dirs: dirs.into_iter().collect(),
        missing,
        outputs,
        session_id: sha256(&session_bytes),
        profile_id: profile_bytes.as_deref().map(sha256),
        syscalls: session
            .syscalls
            .keys()
            .map(|nr| gex_xlate::profile::syscall_name(*nr).to_string())
            .collect(),
        processes: session.processes,
        threads: session.threads,
        exit: session.exit.clone(),
        runtime_id: sha256(&runtime),
        runtime_build: o.runtime_build.clone(),
    };
    summary.objects = project.objects.len();
    summary.files = project.files.len();
    summary.missing = project.missing.len();
    summary.outputs = project.outputs.len();

    write_file(&out.join(PROJECT_FILE), project.to_json().as_bytes(), false)?;
    write_file(&out.join("build.sh"), templates::build_sh().as_bytes(), true)?;
    write_file(&out.join("run.sh"), templates::run_sh().as_bytes(), true)?;
    write_file(&out.join(".gitignore"), templates::GITIGNORE.as_bytes(), false)?;
    // The web launchers, and the browser runtime when there is one: without
    // it they say so. The page is the user's (user/index.html, written once),
    // and calls the app library (app.mjs's `start()`).
    write_file(&out.join("run.mjs"), crate::web::RUN_MJS.as_bytes(), true)?;
    write_file(&out.join("app.mjs"), crate::web::APP_MJS.as_bytes(), false)?;
    crate::web::write_shell(out)?;
    write_file(&out.join("serve.mjs"), crate::web::SERVE_MJS.as_bytes(), true)?;
    write_file(&out.join("runtime/web/eject.mjs"), crate::web::EJECT_MJS.as_bytes(), false)?;
    write_file(&out.join("runtime/web/gex.mjs"), crate::web::GEX_MJS.as_bytes(), false)?;
    if let (Some(shim), true) = (&o.web_shim, nt.is_some()) {
        // A Windows program runs on the shared-memory x86 shim with its
        // kernel and seed (the browser's NT layer), named by their directory
        // (web/public/gex) or any file in it.
        let from = if shim.is_dir() { shim.clone() } else { shim.parent().map(Path::to_path_buf).unwrap_or_default() };
        let to = out.join(crate::pe::WEB_NT_DIR);
        for f in crate::pe::WEB_NT_WASM {
            let src = from.join(f);
            let b = std::fs::read(&src).with_context(|| {
                format!("reading {} (a Windows program's web build needs the x86 shared-memory shim, kernel and seed: GEX_ARCH=x86 shims/browser/build.sh)", src.display())
            })?;
            write_file(&to.join(f), &b, false)?;
        }
        for (f, text) in crate::pe::WEB_NT_JS {
            write_file(&to.join("js").join(f), text.as_bytes(), false)?;
        }
        write_file(&out.join("run.mjs"), crate::pe::WEB_NT_RUN_MJS.as_bytes(), true)?;
        // The app library, when given (web/build-eject-page.sh's output):
        // serve.mjs runs the web build in a browser, user/index.html at `/`
        // calling app.mjs's `start()` (with OpenGL, on Mesa over WebGPU).
        if let Some(page) = &o.web_page {
            if !page.join("app.mjs").is_file() {
                bail!("{} has no app.mjs (make it with web/build-eject-page.sh)", page.display());
            }
            crate::web::check_no_test_page(page)?;
            let to = out.join(crate::pe::WEB_NT_PAGE);
            if to.exists() {
                std::fs::remove_dir_all(&to)?;
            }
            let mut n = 0;
            crate::build::copy_tree(page, &to, &mut n)?;
            // The Linux launcher's app.mjs is not this project's: the
            // library is the vendored one, served at `/`.
            let _ = std::fs::remove_file(out.join("app.mjs"));
            write_file(&out.join("serve.mjs"), crate::pe::WEB_NT_SERVE_MJS.as_bytes(), true)?;
        }
        summary.web = true;
    } else if let Some(shim) = &o.web_shim {
        let b = std::fs::read(shim).with_context(|| format!("reading {}", shim.display()))?;
        if b.get(..4) != Some(b"\0asm") {
            bail!("{} is not a wasm module", shim.display());
        }
        write_file(&out.join(crate::web::SHIM), &b, false)?;
        summary.web = true;
    }
    write_file(&out.join("gen/gex_replace.h"), templates::header(&project.isa).as_bytes(), false)?;
    // The function tier's view of gex-abi, which decompiled C includes.
    write_file(&out.join("gen/gex_abi.h"), gex_cgen::header::header().as_bytes(), false)?;

    // A Windows program's stateful providers: LuaJIT's lua51.dll (which
    // generates code at run time) is served by PUC Lua in wasm
    // (docs/eject-pe.md §5).
    let mut providers = Vec::new();
    if nt.is_some() {
        write_file(&out.join("gen/provider.h"), crate::pe::PROVIDER_H.as_bytes(), false)?;
        for o in &project.objects {
            let name = o.path.rsplit('/').next().unwrap_or("").to_ascii_lowercase();
            if name == "lua51.dll" && crate::pe::is_luajit(&out.join(rootfs_rel(&o.path)?))? {
                providers.push(config::Provider { library: "lua51.dll".into(), dir: "user/lib/lua51".into() });
            }
        }
    }

    // The user's part, once.
    let user = out.join("user");
    if !user.join("eject.json").exists() {
        write_file(&user.join("eject.json"), config::template_with(&providers).as_bytes(), false)?;
    }
    for p in &providers {
        let lib = p.dir.rsplit('/').next().unwrap_or("");
        if !out.join(&p.dir).exists() {
            for (l, path, bytes, exec) in crate::libs::LIBS.iter().filter(|(l, ..)| *l == lib) {
                let _ = l;
                write_file(&out.join(&p.dir).join(path), bytes, *exec)?;
            }
        }
    }
    if !user.join("README").exists() {
        write_file(&user.join("README"), templates::USER_README.as_bytes(), false)?;
    }
    for d in ["src", "files"] {
        std::fs::create_dir_all(user.join(d)).with_context(|| format!("creating user/{d}"))?;
    }
    Ok(summary)
}
