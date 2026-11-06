// AURORA: object graphs, the Rust baseline of `src/mod.code`: `Box` trees;
// `Rc` graphs (a `Cell` mark, a `RefCell` edge list) and `Rc<RefCell<..>>` lists whose cycles are broken by hand before
// they are dropped (without that they would leak). The same shapes, the
// same check values.
#[path = "../common/bench.rs"]
mod bench;

use bench::{report, Rng};
use std::cell::{Cell, RefCell};
use std::hint::black_box;
use std::rc::Rc;

struct Tree {
    left: Option<Box<Tree>>,
    right: Option<Box<Tree>>,
}

fn bottom_up(depth: i64) -> Box<Tree> {
    if depth > 0 {
        Box::new(Tree { left: Some(bottom_up(depth - 1)), right: Some(bottom_up(depth - 1)) })
    } else {
        Box::new(Tree { left: None, right: None })
    }
}

fn count(t: &Tree) -> i64 {
    match (&t.left, &t.right) {
        (Some(l), Some(r)) => 1 + count(l) + count(r),
        _ => 1,
    }
}

struct Node {
    id: i64,
    mark: Cell<i64>,
    edges: RefCell<Vec<Rc<Node>>>,
}

struct Link {
    value: i64,
    prev: Option<Rc<RefCell<Link>>>,
    next: Option<Rc<RefCell<Link>>>,
}

fn main() {
    match bench::kernel().as_str() {
        "binary_trees" => {
            let max_depth = black_box(16i64);
            let start = bench::now();
            let long_lived = bottom_up(max_depth);
            let mut check = 0i64;
            let mut depth = 4;
            while depth <= max_depth {
                let iterations = 1 << (max_depth - depth + 4);
                for _ in 0..iterations {
                    check += count(&bottom_up(depth));
                }
                depth += 2;
            }
            check += count(&long_lived);
            report(start, check as u64);
        }
        "cyclic_graph" => {
            let mut r = Rng::new();
            let n = black_box(20_000usize);
            let start = bench::now();
            let mut check = 0u64;
            for round in 0..40i64 {
                let nodes: Vec<Rc<Node>> = (0..n)
                    .map(|i| Rc::new(Node { id: i as i64, mark: Cell::new(-1), edges: RefCell::new(Vec::new()) }))
                    .collect();
                for node in &nodes {
                    let mut edges = node.edges.borrow_mut();
                    for _ in 0..4 {
                        edges.push(nodes[(r.next() % n as u64) as usize].clone());
                    }
                }
                let mut stack = vec![nodes[0].clone()];
                nodes[0].mark.set(round);
                let (mut visited, mut sum) = (0u64, 0u64);
                while let Some(top) = stack.pop() {
                    visited += 1;
                    sum += top.id as u64;
                    for next in top.edges.borrow().iter() {
                        if next.mark.get() != round {
                            next.mark.set(round);
                            stack.push(next.clone());
                        }
                    }
                }
                check += visited * 1_000_000 + sum % 1_000_000;
                // Break the cycles, or the nodes would never be freed.
                for node in &nodes {
                    node.edges.borrow_mut().clear();
                }
            }
            report(start, check);
        }
        "linked_list" => {
            let start = bench::now();
            let mut check = 0u64;
            for _ in 0..black_box(10) {
                let head = Rc::new(RefCell::new(Link { value: 0, prev: None, next: None }));
                let mut tail = head.clone();
                for i in 1..1_000_000i64 {
                    let node = Rc::new(RefCell::new(Link { value: i, prev: Some(tail.clone()), next: None }));
                    tail.borrow_mut().next = Some(node.clone());
                    tail = node;
                }
                let mut forward = 0u64;
                let mut at = Some(head.clone());
                while let Some(node) = at {
                    let n = node.borrow();
                    forward += n.value as u64;
                    at = n.next.clone();
                }
                let mut backward = 0u64;
                let mut at = Some(tail.clone());
                while let Some(node) = at {
                    let n = node.borrow();
                    backward += (n.value as u64) & 255;
                    at = n.prev.clone();
                }
                check += forward + backward;
                // Break the cycles front to back (dropping a long `next`
                // chain recursively would overflow the stack).
                let mut at = Some(head);
                while let Some(node) = at {
                    let mut n = node.borrow_mut();
                    n.prev = None;
                    at = n.next.take();
                }
            }
            report(start, check);
        }
        k => bench::unknown(k),
    }
}
