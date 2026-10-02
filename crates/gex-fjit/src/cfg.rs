//! The shape of a control-flow graph, for structured emission.
//!
//! Wasm has no `goto`: a branch names an enclosing `block` (forward, to its
//! end) or `loop` (backward, to its start). A reducible CFG maps onto those
//! exactly, with no dispatch variable, by Ramsey's construction ("Beyond
//! Relooper", ICFP 2022) over the dominator tree:
//!
//! * a loop header (a node a back-edge reaches) becomes a `loop`, so every
//!   back-edge to it is a `br` to the loop's label;
//! * a merge node (two or more forward in-edges) becomes the code right
//!   after a `block` wrapping its immediate dominator's code, so every
//!   forward edge to it is a `br` out of that block;
//! * any other node has exactly one forward in-edge, from its immediate
//!   dominator, and is emitted inline at that edge.
//!
//! This module computes what that needs: reverse postorder, immediate
//! dominators, the dominator tree, loop headers and merge nodes. An
//! irreducible graph (a retreating edge whose target does not dominate its
//! source) has no such shape; [`shape`] says so, and [`make_reducible`]
//! gives it one.

use std::collections::{HashMap, HashSet};

/// A graph over nodes `0..succs.len()`, entered at `entry`.
pub struct Graph {
    pub succs: Vec<Vec<usize>>,
    pub entry: usize,
}

pub struct Shape {
    /// Each node's reverse-postorder position, `usize::MAX` if unreachable.
    pub order: Vec<usize>,
    /// Reachable nodes in reverse postorder.
    pub rpo: Vec<usize>,
    /// Each reachable node's immediate dominator (the entry's is itself).
    pub idom: Vec<usize>,
    /// Dominator-tree children that are merge nodes, in reverse postorder.
    pub merge_children: Vec<Vec<usize>>,
    pub loop_header: Vec<bool>,
    pub merge: Vec<bool>,
}

impl Shape {
    /// Whether `from -> to` is a back-edge (to a loop header).
    pub fn backward(&self, from: usize, to: usize) -> bool {
        self.order[to] <= self.order[from]
    }

    /// Whether every edge into `to` from `from` is a `br` (rather than `to`
    /// being emitted inline there).
    pub fn branched(&self, from: usize, to: usize) -> bool {
        self.backward(from, to) || self.merge[to]
    }
}

/// The nodes reachable from the entry, in reverse postorder.
pub fn rpo(g: &Graph) -> Vec<usize> {
    let n = g.succs.len();
    let mut seen = vec![false; n];
    let mut post = Vec::with_capacity(n);
    let mut stack: Vec<(usize, usize)> = vec![(g.entry, 0)];
    seen[g.entry] = true;
    while let Some(top) = stack.last_mut() {
        let (b, i) = *top;
        if i < g.succs[b].len() {
            top.1 += 1;
            let s = g.succs[b][i];
            if !seen[s] {
                seen[s] = true;
                stack.push((s, 0));
            }
        } else {
            post.push(b);
            stack.pop();
        }
    }
    post.into_iter().rev().collect()
}

/// The shape of `g`, or `None` when it is irreducible.
pub fn shape(g: &Graph) -> Option<Shape> {
    let n = g.succs.len();
    const NONE: usize = usize::MAX;
    let rpo = rpo(g);
    let mut order = vec![NONE; n];
    for (k, &b) in rpo.iter().enumerate() {
        order[b] = k;
    }
    let mut preds: Vec<Vec<usize>> = vec![Vec::new(); n];
    for &b in &rpo {
        for &s in &g.succs[b] {
            preds[s].push(b);
        }
    }
    // Cooper, Harvey and Kennedy's iterative dominators, on rpo positions.
    let mut idom = vec![NONE; n];
    idom[g.entry] = g.entry;
    let intersect = |idom: &[usize], mut a: usize, mut b: usize| -> usize {
        while a != b {
            while order[a] > order[b] {
                a = idom[a];
            }
            while order[b] > order[a] {
                b = idom[b];
            }
        }
        a
    };
    let mut changed = true;
    while changed {
        changed = false;
        for &b in rpo.iter().skip(1) {
            let mut new = NONE;
            for &p in &preds[b] {
                if idom[p] == NONE {
                    continue;
                }
                new = if new == NONE { p } else { intersect(&idom, p, new) };
            }
            if new != idom[b] {
                idom[b] = new;
                changed = true;
            }
        }
    }
    let dominates = |a: usize, mut b: usize| -> bool {
        loop {
            if a == b {
                return true;
            }
            if b == g.entry {
                return false;
            }
            b = idom[b];
        }
    };
    let mut loop_header = vec![false; n];
    let mut forward_in = vec![0usize; n];
    for &b in &rpo {
        for &s in &g.succs[b] {
            if order[s] <= order[b] {
                if !dominates(s, b) {
                    return None;
                }
                loop_header[s] = true;
            } else {
                forward_in[s] += 1;
            }
        }
    }
    let merge: Vec<bool> = forward_in.iter().map(|&k| k >= 2).collect();
    let mut merge_children = vec![Vec::new(); n];
    for &b in rpo.iter().skip(1) {
        if merge[b] {
            merge_children[idom[b]].push(b);
        }
    }
    Some(Shape { order, rpo, idom, merge_children, loop_header, merge })
}

/// An irreducible region made reducible: a dispatch node entered with a
/// selector, whose successors are the region's entries.
pub struct Dispatch {
    pub node: usize,
    pub entries: Vec<usize>,
}

/// [`make_reducible`]'s result.
pub struct Fixed {
    pub graph: Graph,
    pub shape: Shape,
    pub dispatch: Vec<Dispatch>,
    /// Edge `(from, to)` now goes to `(dispatch node, selector)`.
    pub redirect: HashMap<(usize, usize), (usize, u32)>,
    /// The function starts at a dispatch node with this selector.
    pub start: Option<u32>,
}

/// Make `g` reducible: every strongly connected region with more than one
/// entry gets a dispatch node that becomes its only entry (each edge into
/// one of its entries goes through it, setting a selector), the way LLVM's
/// FixIrreducibleControlFlow does for wasm. Only edges into those entries
/// pay; everything else stays a plain `br`. Threaded interpreters (every
/// case ending in its own jump through the table) are the usual case.
pub fn make_reducible(mut g: Graph) -> Option<Fixed> {
    let mut dispatch = Vec::new();
    let mut redirect: HashMap<(usize, usize), (usize, u32)> = HashMap::new();
    let mut start = None;
    for _ in 0..64 {
        if let Some(shape) = shape(&g) {
            return Some(Fixed { graph: g, shape, dispatch, redirect, start });
        }
        let all: Vec<usize> = rpo(&g);
        let preds = preds_of(&g);
        let (_, entries) = irreducible_region(&g, &preds, &all, g.entry)?;
        let d = g.succs.len();
        g.succs.push(entries.clone());
        let sel = |e: usize| entries.iter().position(|&x| x == e).map(|k| k as u32);
        for p in 0..d {
            for s in g.succs[p].iter_mut() {
                if let Some(k) = sel(*s) {
                    redirect.insert((p, *s), (d, k));
                    *s = d;
                }
            }
        }
        if let Some(k) = sel(g.entry) {
            start = Some(k);
            g.entry = d;
        }
        dispatch.push(Dispatch { node: d, entries });
    }
    None
}

fn preds_of(g: &Graph) -> Vec<Vec<usize>> {
    let mut preds = vec![Vec::new(); g.succs.len()];
    for (b, ss) in g.succs.iter().enumerate() {
        for &s in ss {
            preds[s].push(b);
        }
    }
    preds
}

/// A strongly connected region of `nodes` with more than one entry, and its
/// entries; inside a single-entry region, one without its entry.
fn irreducible_region(g: &Graph, preds: &[Vec<usize>], nodes: &[usize], entry: usize) -> Option<(Vec<usize>, Vec<usize>)> {
    for scc in sccs(g, nodes) {
        let set: HashSet<usize> = scc.iter().copied().collect();
        if scc.len() == 1 && !g.succs[scc[0]].contains(&scc[0]) {
            continue;
        }
        let mut entries: Vec<usize> =
            scc.iter().copied().filter(|&v| v == entry || preds[v].iter().any(|u| !set.contains(u))).collect();
        entries.sort_unstable();
        if entries.len() > 1 {
            return Some((scc, entries));
        }
        let Some(&h) = entries.first() else { continue };
        let inner: Vec<usize> = scc.iter().copied().filter(|&v| v != h).collect();
        if let Some(r) = irreducible_region(g, preds, &inner, usize::MAX) {
            return Some(r);
        }
    }
    None
}

/// Tarjan's strongly connected components of the subgraph on `nodes`.
fn sccs(g: &Graph, nodes: &[usize]) -> Vec<Vec<usize>> {
    let inside: HashSet<usize> = nodes.iter().copied().collect();
    let n = g.succs.len();
    let mut index = vec![usize::MAX; n];
    let mut low = vec![0usize; n];
    let mut on = vec![false; n];
    let mut stack = Vec::new();
    let mut out = Vec::new();
    let mut next = 0;
    for &root in nodes {
        if index[root] != usize::MAX {
            continue;
        }
        // Iterative: (node, next successor).
        let mut work: Vec<(usize, usize)> = vec![(root, 0)];
        index[root] = next;
        low[root] = next;
        next += 1;
        stack.push(root);
        on[root] = true;
        while let Some(&mut (v, ref mut i)) = work.last_mut() {
            if *i < g.succs[v].len() {
                let w = g.succs[v][*i];
                *i += 1;
                if !inside.contains(&w) {
                    continue;
                }
                if index[w] == usize::MAX {
                    index[w] = next;
                    low[w] = next;
                    next += 1;
                    stack.push(w);
                    on[w] = true;
                    work.push((w, 0));
                } else if on[w] {
                    low[v] = low[v].min(index[w]);
                }
            } else {
                work.pop();
                if let Some(&(u, _)) = work.last() {
                    low[u] = low[u].min(low[v]);
                }
                if low[v] == index[v] {
                    let mut c = Vec::new();
                    while let Some(w) = stack.pop() {
                        on[w] = false;
                        c.push(w);
                        if w == v {
                            break;
                        }
                    }
                    out.push(c);
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diamond_merges_and_loops() {
        // 0 -> 1, 2; 1 -> 3; 2 -> 3; 3 -> 0 (loop), 4.
        let g = Graph { succs: vec![vec![1, 2], vec![3], vec![3], vec![0, 4], vec![]], entry: 0 };
        let s = shape(&g).expect("reducible");
        assert!(s.loop_header[0]);
        assert!(s.merge[3] && !s.merge[1] && !s.merge[4]);
        assert_eq!(s.idom[3], 0);
        assert_eq!(s.merge_children[0], vec![3]);
    }

    #[test]
    fn irreducible_is_refused() {
        // 0 -> 1, 2; 1 <-> 2: a cycle with two entries.
        let g = Graph { succs: vec![vec![1, 2], vec![2], vec![1]], entry: 0 };
        assert!(shape(&g).is_none());
    }

    #[test]
    fn a_two_entry_cycle_gets_a_dispatch_node() {
        let g = Graph { succs: vec![vec![1, 2], vec![2], vec![1]], entry: 0 };
        let f = make_reducible(g).expect("fixable");
        assert_eq!(f.dispatch.len(), 1);
        let d = f.dispatch[0].node;
        assert_eq!(f.dispatch[0].entries, vec![1, 2]);
        assert_eq!(f.redirect[&(0, 1)], (d, 0));
        assert_eq!(f.redirect[&(1, 2)], (d, 1));
        assert!(f.shape.loop_header[d]);
    }

    #[test]
    fn a_threaded_interpreter_is_fixed() {
        // 0 dispatches to cases 1..=3; each case jumps through its own copy
        // of the dispatch (4..=6) to any case.
        let g = Graph {
            succs: vec![vec![1, 2, 3], vec![4], vec![5], vec![6], vec![1, 2, 3], vec![1, 2, 3], vec![1, 2, 3, 7], vec![]],
            entry: 0,
        };
        assert!(shape(&g).is_none());
        let f = make_reducible(g).expect("fixable");
        assert_eq!(f.dispatch.len(), 1);
    }

    #[test]
    fn two_edges_from_one_node_merge() {
        // Both arms of 0's branch go to 1.
        let g = Graph { succs: vec![vec![1, 1], vec![]], entry: 0 };
        let s = shape(&g).unwrap();
        assert!(s.merge[1]);
    }
}
