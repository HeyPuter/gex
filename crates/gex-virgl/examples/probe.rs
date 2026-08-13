//! Does this machine have a working host renderer?
//!
//!     cargo run -p gex-virgl --example probe
//!
//! "The guest sees no GPU" has several causes -- no library, no EGL, no driver
//! behind it -- with different fixes. This says which one it is.

fn main() {
    if !gex_virgl::AVAILABLE {
        println!("virgl: this build has no libvirglrenderer");
        std::process::exit(1);
    }
    match gex_virgl::Virgl::new() {
        Ok(mut v) => {
            println!("virgl: {}", v.describe());
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
