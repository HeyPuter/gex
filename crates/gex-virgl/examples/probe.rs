//! Does this machine have a working host renderer?
//!
//!     cargo run -p gex-virgl --example probe
//!
//! Worth having as a program rather than a note in a README: "the guest sees
//! no GPU" has several causes -- no library, no EGL, no driver behind the EGL
//! -- and they need different fixes. This says which one it is before anyone
//! starts translating a binary to find out.

fn main() {
    if !gex_virgl::AVAILABLE {
        println!("virgl: this build has no libvirglrenderer");
        std::process::exit(1);
    }
    match gex_virgl::Virgl::new() {
        Ok(mut v) => {
            println!("virgl: {}", v.describe());
            // The capability set is what the guest's Mesa reads to decide
            // which GL version it can offer. An empty one means the host
            // context came up but has nothing behind it.
            // A renderer context is the first thing any client asks for, so
            // failing here means nothing else can be tried.
            match v.context_create("probe") {
                Ok(id) => println!("virgl: renderer context {id}"),
                Err(e) => {
                    println!("virgl: context_create failed: {e}");
                    std::process::exit(1);
                }
            }
            let caps = v.caps(2);
            println!("virgl: capability set 2 is {} bytes", caps.len());
            if caps.is_empty() {
                println!("virgl: no capabilities -- the host context is not usable");
                std::process::exit(1);
            }
            println!("virgl: ok");
        }
        Err(e) => {
            println!("virgl: unavailable: {e}");
            std::process::exit(1);
        }
    }
}
