//! A module made easier for wasmtime to compile, in memory only: what is
//! cached and keyed is still the module as it was.
//!
//! A position-independent module's element segment is active at a dynamic
//! offset (`global.get __table_base`). wasmtime compiles it into the
//! module's startup code, one table store per slot: about 60 us a slot,
//! and a large program's (a million slots) took register allocation hours.
//! Made passive and filled by one `table.init` at the head of the start
//! function, which runs before any other code of the module, it compiles
//! three to five times faster (4.0 s against 1.2 s at 64k slots, 13.3 s
//! against 2.8 s at 256k).
//!
//! Only for wasmtime: V8 fills an active segment's slots lazily but makes
//! every funcref of a `table.init` at once (6.5 times the heap for that
//! program), so the module as shipped keeps its active segment.

/// Segments of at most this many slots are left as they are (Wine's
/// builtins: up to about 27k, the core's): below it the rewrite saves
/// under a second of a compile that is cached.
pub const MIN_SLOTS: u64 = 1 << 14;

/// `b` with its one active element segment at a dynamic offset made
/// passive and filled by `table.init` at the head of its start function;
/// None when it has no such segment, one of at most `min` slots, or no
/// start function.
pub fn passive_elems(b: &[u8], min: u64) -> Option<Vec<u8>> {
    fn leb(b: &[u8], mut i: usize) -> Option<(u64, usize)> {
        let (mut r, mut s) = (0u64, 0);
        loop {
            let c = *b.get(i)?;
            i += 1;
            r |= ((c & 0x7f) as u64).checked_shl(s)?;
            s += 7;
            if c < 0x80 {
                return Some((r, i));
            }
        }
    }
    fn enc(mut v: u64, o: &mut Vec<u8>) {
        loop {
            let c = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                o.push(c);
                return;
            }
            o.push(c | 0x80);
        }
    }
    fn senc(mut v: i64, o: &mut Vec<u8>) {
        loop {
            let c = (v & 0x7f) as u8;
            v >>= 7;
            if (v == 0 && c & 0x40 == 0) || (v == -1 && c & 0x40 != 0) {
                o.push(c);
                return;
            }
            o.push(c | 0x80);
        }
    }
    if b.len() < 8 {
        return None;
    }
    let mut secs: Vec<(u8, std::ops::Range<usize>)> = Vec::new();
    let mut i = 8;
    while i < b.len() {
        let (size, j) = leb(b, i + 1)?;
        let end = j.checked_add(size as usize).filter(|&e| e <= b.len())?;
        secs.push((b[i], j..end));
        i = end;
    }
    let find = |id: u8| secs.iter().position(|(s, _)| *s == id);
    let (ei, si, ci) = (find(9)?, find(8)?, find(10)?);
    let eb = &b[secs[ei].1.clone()];
    let (n, k) = leb(eb, 0)?;
    if n != 1 || *eb.get(k)? != 0 || *eb.get(k + 1)? != 0x23 {
        return None;
    }
    let (g, k2) = leb(eb, k + 2)?;
    if *eb.get(k2)? != 0x0b {
        return None;
    }
    let items = &eb[k2 + 1..];
    let (cnt, _) = leb(items, 0)?;
    if cnt <= min || cnt > i32::MAX as u64 {
        return None;
    }
    let mut nimp = 0u64;
    if let Some(ii) = find(2) {
        let ib = &b[secs[ii].1.clone()];
        let (m, mut q) = leb(ib, 0)?;
        for _ in 0..m {
            for _ in 0..2 {
                let (l, q2) = leb(ib, q)?;
                q = q2 + l as usize;
            }
            let kind = *ib.get(q)?;
            q += 1;
            match kind {
                0 => {
                    nimp += 1;
                    q = leb(ib, q)?.1;
                }
                1 | 2 => {
                    q += (kind == 1) as usize;
                    let (fl, q2) = leb(ib, q)?;
                    q = leb(ib, q2)?.1;
                    if fl & 1 != 0 {
                        q = leb(ib, q)?.1;
                    }
                }
                3 => q += 2,
                4 => {
                    q += 1;
                    q = leb(ib, q)?.1;
                }
                _ => return None,
            }
        }
    }
    let (start, _) = leb(&b[secs[si].1.clone()], 0)?;
    let start = start.checked_sub(nimp)?;
    let cb = &b[secs[ci].1.clone()];
    let (nc, mut q) = leb(cb, 0)?;
    if start >= nc {
        return None;
    }
    let mut code = Vec::with_capacity(cb.len() + 32);
    enc(nc, &mut code);
    for x in 0..nc {
        let (sz, q2) = leb(cb, q)?;
        let body = cb.get(q2..q2 + sz as usize)?;
        if x == start {
            // after its locals: the segment's offset (in the table's
            // index type, as the global is), then source and length (i32)
            let (nl, mut r) = leb(body, 0)?;
            for _ in 0..nl {
                r = leb(body, r)?.1 + 1;
            }
            let mut init = vec![0x23];
            enc(g, &mut init);
            init.extend_from_slice(&[0x41, 0x00, 0x41]);
            senc(cnt as i64, &mut init);
            init.extend_from_slice(&[0xfc, 0x0c, 0x00, 0x00, 0xfc, 0x0d, 0x00]);
            enc((body.len() + init.len()) as u64, &mut code);
            code.extend_from_slice(&body[..r]);
            code.extend_from_slice(&init);
            code.extend_from_slice(&body[r..]);
        } else {
            enc(sz, &mut code);
            code.extend_from_slice(body);
        }
        q = q2 + sz as usize;
    }
    let mut elem = vec![1, 0x01, 0x00];
    elem.extend_from_slice(items);
    let mut out = b[..8].to_vec();
    for (k, (id, r)) in secs.iter().enumerate() {
        let body: &[u8] = if k == ei { &elem } else if k == ci { &code } else { &b[r.clone()] };
        out.push(*id);
        enc(body.len() as u64, &mut out);
        out.extend_from_slice(body);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use wasmtime::{Engine, Global, GlobalType, Instance, Module, Mutability, Ref, RefType, Store, Table, TableType, Val, ValType};

    fn enc(mut v: u64, o: &mut Vec<u8>) {
        loop {
            let c = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                o.push(c);
                return;
            }
            o.push(c | 0x80);
        }
    }

    /// `k` slots at `global.get env.tb` of an imported table, and a start
    /// function with a local.
    fn module(k: u64) -> Vec<u8> {
        let sec = |id: u8, body: Vec<u8>, o: &mut Vec<u8>| {
            o.push(id);
            enc(body.len() as u64, o);
            o.extend(body);
        };
        let mut o = b"\0asm\x01\0\0\0".to_vec();
        sec(1, vec![1, 0x60, 0, 0], &mut o);
        let mut imp = vec![2, 3, b'e', b'n', b'v', 2, b't', b'b', 3, 0x7f, 0, 3, b'e', b'n', b'v', 1, b't', 1, 0x70, 0];
        enc(k + 8, &mut imp);
        sec(2, imp, &mut o);
        sec(3, vec![2, 0, 0], &mut o);
        sec(8, vec![1], &mut o);
        let mut el = vec![1, 0, 0x23, 0, 0x0b];
        enc(k, &mut el);
        el.extend((0..k).map(|x| (x % 2) as u8));
        sec(9, el, &mut o);
        sec(10, vec![2, 2, 0, 0x0b, 4, 1, 1, 0x7f, 0x0b], &mut o);
        o
    }

    #[test]
    fn small_left_alone() {
        assert!(super::passive_elems(&module(100), super::MIN_SLOTS).is_none());
    }

    #[test]
    fn passive_fills_the_same_slots() {
        let k = super::MIN_SLOTS + 1;
        let wasm = super::passive_elems(&module(k), super::MIN_SLOTS).expect("rewritten");
        let engine = Engine::default();
        let m = Module::new(&engine, &wasm).unwrap();
        let mut store = Store::new(&engine, ());
        let tb = Global::new(&mut store, GlobalType::new(ValType::I32, Mutability::Const), Val::I32(3)).unwrap();
        let t = Table::new(&mut store, TableType::new(RefType::FUNCREF, (k + 8) as u32, None), Ref::Func(None)).unwrap();
        Instance::new(&mut store, &m, &[tb.into(), t.into()]).unwrap();
        let set = |s: &mut Store<()>, i: u64| !t.get(&mut *s, i).unwrap().unwrap_func().is_none();
        assert!(!set(&mut store, 2));
        assert!(set(&mut store, 3));
        assert!(set(&mut store, 3 + k - 1));
        assert!(!set(&mut store, 3 + k));
    }
}
