//! Replays a batch dumped by GEX_FJIT_DUMP (GIR text, functions separated by
//! blank lines): GEX_FJIT_REPLAY=<file> cargo test -p gex-fjit --test replay.
#[test]
fn replay() {
    let Ok(path) = std::env::var("GEX_FJIT_REPLAY") else { return };
    let src = std::fs::read_to_string(path).unwrap();
    let mut funcs = Vec::new();
    for chunk in src.split("\nfunc ") {
        let text = if chunk.starts_with("func ") { chunk.to_string() } else { format!("func {chunk}") };
        funcs.push(gex_ir::text::parse(&text).unwrap());
    }
    let mut inl = funcs.clone();
    gex_fjit::inline::inline_all(&mut inl);
    for (a, b) in funcs.iter().zip(&inl) {
        for blk in &b.blocks {
            let mut d = std::collections::HashSet::new();
            for s in &blk.stmts {
                gex_ir::func::for_each_def(s, |v| {
                    d.insert(v.0);
                });
            }
            blk.term.for_each_use(|v| {
                if !d.contains(&v.0) {
                    println!("after inlining, {:#x} block {:#x}: {} uses {v:?}", a.entry, blk.start, gex_ir::text::term(&blk.term));
                }
            });
        }
    }
    let sums = std::collections::HashMap::new();
    let r = gex_fjit::runtime(funcs, &sums, true, &|_| 1, &[], &std::collections::HashMap::new(), &Default::default());
    println!("{:?}", r.map(|(w, e, _)| (w.len(), e.len())));
}
