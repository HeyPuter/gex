//! What an object says about the library functions it calls.
//!
//!     cargo run -p gex-ld --example dump-relocs -- <elf>...
//!
//! Prints the `STT_GNU_IFUNC` symbols and named GOT slots, which is how
//! `gex_xlate::intrinsic` finds a `memcpy` that has no address until the
//! dynamic linker has run.

fn main() {
    for path in std::env::args().skip(1) {
        let Ok(bytes) = std::fs::read(&path) else {
            println!("{path}: unreadable");
            continue;
        };
        let Ok(image) = gex_ld::elf::load(&bytes, 0) else {
            println!("{path}: not an ELF this can load");
            continue;
        };
        println!("{path}:");
        for s in &image.symbols {
            if gex_xlate::intrinsic::recognise(&s.name).is_some() {
                println!(
                    "  symbol   {:<24} {:#x}{}",
                    s.name,
                    s.value,
                    if s.ifunc { "  (ifunc: a resolver)" } else { "" }
                );
            }
        }
        for (slot, name) in &image.named_relocs {
            if let Some(kind) = gex_xlate::intrinsic::recognise(name) {
                println!("  got slot {name:<24} {slot:#x} -> {kind:?}");
            }
        }
    }
}
