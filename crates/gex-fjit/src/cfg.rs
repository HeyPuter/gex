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
//! source) has no such shape; [`shape`] says so.

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

/// The shape of `g`, or `None` when it is irreducible.
pub fn shape(g: &Graph) -> Option<Shape> {
    let n = g.succs.len();
    const NONE: usize = usize::MAX;
    // Iterative DFS for the postorder.
    let mut order = vec![NONE; n];
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
    let rpo: Vec<usize> = post.into_iter().rev().collect();
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
    fn two_edges_from_one_node_merge() {
        // Both arms of 0's branch go to 1.
        let g = Graph { succs: vec![vec![1, 1], vec![]], entry: 0 };
        let s = shape(&g).unwrap();
        assert!(s.merge[1]);
    }
}
