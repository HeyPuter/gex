//! Can `gex-ld` link a real program by itself, without the guest's `ld.so`?
//!
//! `link` has been in this crate since dynamic linking landed and nothing has
//! ever called it: the browser shim uses `load_program`, which loads the
//! executable and the interpreter and lets the guest enter `ld.so` to do the
//! work. That costs 157ms of emulated time in every process -- measured
//! against the same program linked static-pie, which forks in 133ms against
//! Debian's 330ms.
//!
//! Before wiring `link` into the runtime, the question is whether it can
//! honestly claim to have linked these binaries. `Linked::unhandled` and
//! `Linked::missing` are the answer and cost nothing to ask for.
//!
//!     cargo run -p gex-ld --example linkcheck -- <sysroot> <program>...
//!
//! Prints, per program: the entry it would jump to, how many IFUNC resolvers
//! the guest would have to run, how many initialisers, the static TLS it
//! needs, and -- the two that decide it -- relocations no rule covers and
//! symbols nothing defined.

use std::path::PathBuf;

fn main() {
    let mut args = std::env::args().skip(1);
    let sysroot = PathBuf::from(args.next().expect("usage: linkcheck <sysroot> <program>..."));
    let programs: Vec<String> = args.collect();
    if programs.is_empty() {
        eprintln!("usage: linkcheck <sysroot> <program>...");
        std::process::exit(2);
    }

    // The same rule the runtime uses: a guest path is a path under the
    // sysroot, and a library named without a directory is looked for in the
    // usual places.
    let read = |guest: &str| -> Option<Vec<u8>> {
        let rel = guest.trim_start_matches('/');
        let direct = sysroot.join(rel);
        if let Ok(b) = std::fs::read(&direct) {
            return Some(b);
        }
        for dir in [
            "lib/aarch64-linux-gnu",
            "usr/lib/aarch64-linux-gnu",
            "lib",
            "usr/lib",
        ] {
            if let Ok(b) = std::fs::read(sysroot.join(dir).join(rel)) {
                return Some(b);
            }
        }
        None
    };

    let mut bad = 0;
    for program in &programs {
        let Some(bytes) = read(program) else {
            println!("{program}: not in the sysroot");
            bad += 1;
            continue;
        };
        // Where each object goes. The runtime pins these; here they only have
        // to be distinct and not overlap, because what is being checked is
        // whether every relocation has a rule, not where it lands.
        let mut next = gex_abi::INTERP_BASE;
        let mut place = |_name: &str, span: u64| {
            let at = next;
            next += (span + 0xffff) & !0xffff;
            at
        };
        match gex_ld::link(&bytes, program, gex_abi::PIE_BASE, &read, &mut place) {
            Ok(l) => {
                let ok = l.unhandled.is_empty() && l.missing.is_empty();
                if !ok {
                    bad += 1;
                }
                println!(
                    "{program}: {} objects, entry {:#x}, {} ifunc, {} tlsdesc, {} init, tls {}/{} -- {}",
                    l.objects.len(),
                    l.entry,
                    l.irelative.len(),
                    l.tlsdesc.len(),
                    l.init.len(),
                    l.tls_size,
                    l.tls_align,
                    if ok { "linked" } else { "INCOMPLETE" },
                );
                for (obj, ty, n) in &l.unhandled {
                    println!("    unhandled: {obj} type {ty} x{n}");
                }
                for m in l.missing.iter().take(10) {
                    println!("    missing:   {m}");
                }
                if l.missing.len() > 10 {
                    println!("    missing:   ... and {} more", l.missing.len() - 10);
                }
            }
            Err(e) => {
                println!("{program}: {e:?}");
                bad += 1;
            }
        }
    }
    if bad != 0 {
        std::process::exit(1);
    }
}
