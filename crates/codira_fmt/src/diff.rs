//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 5, 2026
//!
//! Functionality:
//! - A line diff (Myers' O(ND) algorithm) for `codira fmt --check` output and
//!   for counting how many lines formatting changes.

/// One line of a diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffLine<'a> {
    Same(&'a str),
    Removed(&'a str),
    Added(&'a str),
}

/// The shortest line diff turning `before` into `after`.
pub fn line_diff<'a>(before: &'a str, after: &'a str) -> Vec<DiffLine<'a>> {
    let a: Vec<&str> = before.lines().collect();
    let b: Vec<&str> = after.lines().collect();
    let (n, m) = (a.len() as isize, b.len() as isize);
    let max = (n + m) as usize;
    let offset = max as isize;
    let mut v = vec![0isize; 2 * max + 2];
    let mut trace: Vec<Vec<isize>> = Vec::new();
    'search: for d in 0..=max as isize {
        trace.push(v.clone());
        let mut k = -d;
        while k <= d {
            let idx = (k + offset) as usize;
            let mut x = if k == -d || (k != d && v[idx - 1] < v[idx + 1]) {
                v[idx + 1]
            } else {
                v[idx - 1] + 1
            };
            let mut y = x - k;
            while x < n && y < m && a[x as usize] == b[y as usize] {
                x += 1;
                y += 1;
            }
            v[idx] = x;
            if x >= n && y >= m {
                break 'search;
            }
            k += 2;
        }
    }
    // Walk the trace back from the end.
    let mut out = Vec::new();
    let (mut x, mut y) = (n, m);
    for d in (0..trace.len() as isize).rev() {
        let v = &trace[d as usize];
        let k = x - y;
        let prev_k =
            if k == -d || (k != d && v[(k - 1 + offset) as usize] < v[(k + 1 + offset) as usize]) {
                k + 1
            } else {
                k - 1
            };
        let prev_x = if d == 0 {
            0
        } else {
            v[(prev_k + offset) as usize]
        };
        let prev_y = prev_x - prev_k;
        while x > prev_x.max(0)
            && y > prev_y.max(0)
            && x > 0
            && y > 0
            && a[(x - 1) as usize] == b[(y - 1) as usize]
        {
            out.push(DiffLine::Same(a[(x - 1) as usize]));
            x -= 1;
            y -= 1;
        }
        if d > 0 {
            if x == prev_x {
                out.push(DiffLine::Added(b[(y - 1) as usize]));
                y -= 1;
            } else {
                out.push(DiffLine::Removed(a[(x - 1) as usize]));
                x -= 1;
            }
        }
    }
    while x > 0 && y > 0 {
        out.push(DiffLine::Same(a[(x - 1) as usize]));
        x -= 1;
        y -= 1;
    }
    out.reverse();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(diff: &[DiffLine<'_>]) -> (String, String) {
        let mut before = String::new();
        let mut after = String::new();
        for line in diff {
            match line {
                DiffLine::Same(l) => {
                    before += l;
                    before += "\n";
                    after += l;
                    after += "\n";
                }
                DiffLine::Removed(l) => {
                    before += l;
                    before += "\n";
                }
                DiffLine::Added(l) => {
                    after += l;
                    after += "\n";
                }
            }
        }
        (before, after)
    }

    #[test]
    fn diff_reconstructs_both_sides() {
        let cases = [
            ("", ""),
            ("a\n", ""),
            ("", "a\n"),
            ("a\nb\nc\n", "a\nc\n"),
            ("a\nb\nc\n", "a\nx\nb\nc\ny\n"),
            ("x\ny\nz\n", "a\nb\n"),
            ("a\na\nb\na\n", "b\na\na\na\nb\n"),
        ];
        for (before, after) in cases {
            let diff = line_diff(before, after);
            assert_eq!(
                apply(&diff),
                (before.to_string(), after.to_string()),
                "{before:?} -> {after:?}"
            );
        }
    }

    #[test]
    fn diff_is_minimal_for_one_change() {
        let diff = line_diff("a\nb\nc\n", "a\nB\nc\n");
        let changed = diff
            .iter()
            .filter(|l| !matches!(l, DiffLine::Same(_)))
            .count();
        assert_eq!(changed, 2);
    }
}
