//! Statistics over a subtree: extension breakdown, largest files, search.
//!
//! All functions look at the nodes strictly below `under` (plus `under`
//! itself if it is a file) and skip tombstones and virtual nodes (FREE,
//! UNACCOUNTED, "This PC", ...): only real files and directories are
//! reported. When `under` is the root they scan the arena linearly, which is
//! faster than following child links; otherwise they walk the subtree.
//! Each call is O(subtree); run them on a snapshot or under a read lock.

use std::cmp::Reverse;
use std::collections::BinaryHeap;

use crate::tree::{Node, Tree};
use crate::{NodeId, SizeMode};

/// Totals of one extension.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExtStat {
    /// Extension id ([`Tree::ext_name`]); 0 = files without extension.
    pub ext: u16,
    /// Number of files.
    pub files: u64,
    /// Sum of their sizes in the requested mode.
    pub size: u64,
}

/// Calls `f` for every live non-virtual file in scope.
fn for_each_file(tree: &Tree, under: NodeId, mut f: impl FnMut(NodeId, &Node)) {
    if !tree.is_live(under) {
        return;
    }
    if under == tree.root() {
        for (id, n) in tree.live_nodes() {
            if n.is_file() {
                f(id, n);
            }
        }
        return;
    }
    let top = tree.node(under);
    if top.is_file() {
        f(under, top);
    }
    for id in tree.descendants(under) {
        let n = tree.node(id);
        if n.is_file() {
            f(id, n);
        }
    }
}

/// Files and bytes per extension below `under`, largest total size first
/// (ties: more files first, then by extension name).
pub fn ext_breakdown(tree: &Tree, under: NodeId, mode: SizeMode) -> Vec<ExtStat> {
    let mut acc = vec![(0u64, 0u64); tree.ext_count()];
    for_each_file(tree, under, |_, n| {
        let e = &mut acc[usize::from(n.ext())];
        e.0 += 1;
        e.1 += n.size(mode);
    });
    let mut out: Vec<ExtStat> = acc
        .into_iter()
        .enumerate()
        .filter(|(_, (files, _))| *files > 0)
        .map(|(i, (files, size))| ExtStat {
            ext: i as u16,
            files,
            size,
        })
        .collect();
    out.sort_by(|a, b| {
        b.size
            .cmp(&a.size)
            .then(b.files.cmp(&a.files))
            .then_with(|| tree.ext_name(a.ext).cmp(tree.ext_name(b.ext)))
    });
    out
}

/// The `n` largest files (size > 0) below `under`, largest first (ties by
/// id, i.e. insertion order). Uses a bounded min-heap: O(files * log n).
pub fn largest_files(tree: &Tree, under: NodeId, mode: SizeMode, n: usize) -> Vec<NodeId> {
    if n == 0 {
        return Vec::new();
    }
    // Min-heap of (size, Reverse(id)): the root is the weakest candidate.
    let mut heap: BinaryHeap<Reverse<(u64, Reverse<NodeId>)>> = BinaryHeap::with_capacity(n + 1);
    for_each_file(tree, under, |id, node| {
        let size = node.size(mode);
        if size == 0 {
            return;
        }
        let item = Reverse((size, Reverse(id)));
        if heap.len() < n {
            heap.push(item);
        } else if heap.peek().is_some_and(|weakest| item < *weakest) {
            heap.pop();
            heap.push(item);
        }
    });
    let mut out: Vec<(u64, Reverse<NodeId>)> = heap.into_iter().map(|Reverse(x)| x).collect();
    out.sort_unstable_by(|a, b| b.cmp(a));
    out.into_iter().map(|(_, Reverse(id))| id).collect()
}

/// Up to `limit` live, non-virtual files and directories strictly below
/// `under` whose name contains `query` case-insensitively (Unicode
/// lowercase), in depth-first pre-order (child order). Virtual containers are
/// searched through but never reported. An empty query matches nothing.
pub fn search(tree: &Tree, under: NodeId, query: &str, limit: usize) -> Vec<NodeId> {
    let mut out = Vec::new();
    if query.is_empty() || limit == 0 || !tree.is_live(under) {
        return out;
    }
    let mut matcher = Matcher::new(query);
    for id in tree.descendants(under) {
        if tree.node(id).is_virtual() {
            continue;
        }
        if matcher.matches(tree.name(id)) {
            out.push(id);
            if out.len() >= limit {
                break;
            }
        }
    }
    out
}

/// Case-insensitive substring matcher; allocation-free per name.
struct Matcher {
    lower: String,
    ascii: bool,
    /// Scratch buffer for lowercased non-ASCII names.
    buf: String,
}

impl Matcher {
    fn new(query: &str) -> Matcher {
        let lower = query.to_lowercase();
        Matcher {
            ascii: lower.is_ascii(),
            lower,
            buf: String::new(),
        }
    }

    fn matches(&mut self, name: &str) -> bool {
        if self.ascii {
            // ASCII query: compare bytes; non-ASCII bytes of the name never
            // equal ASCII query bytes, so UTF-8 names are handled correctly.
            let q = self.lower.as_bytes();
            let n = name.as_bytes();
            if q.len() > n.len() {
                return false;
            }
            let (lo, up) = (q[0], q[0].to_ascii_uppercase());
            (0..=n.len() - q.len()).any(|i| {
                (n[i] == lo || n[i] == up) && n[i + 1..i + q.len()].eq_ignore_ascii_case(&q[1..])
            })
        } else {
            // The lowercased query contains a non-ASCII char, which an
            // all-ASCII name cannot contain even after lowercasing.
            if name.is_ascii() {
                return false;
            }
            self.buf.clear();
            self.buf.extend(name.chars().flat_map(char::to_lowercase));
            self.buf.contains(self.lower.as_str())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flags::*;

    fn sample() -> (Tree, Vec<NodeId>) {
        let mut t = Tree::new("C:\\", 0);
        let docs = t.add_dir(0, "Docs", 0, 0);
        let a = t.add_file(docs, "Report.PDF", 300, 4096, 0, 0);
        let b = t.add_file(docs, "notes.txt", 20, 4096, 0, 0);
        let c = t.add_file(0, "movie.mkv", 5000, 8192, 0, 0);
        let d = t.add_file(0, "README", 10, 4096, 0, 0);
        let e = t.add_file(docs, "Звіт.pdf", 700, 4096, 0, 0);
        let h = t.add_file(docs, "hard.pdf", 999, 999, HARDLINK, 0);
        t.add_virtual(0, "<free space>", FREE, 1 << 40);
        t.add_virtual(0, "<unaccounted.pdf>", UNACCOUNTED, 1 << 30);
        let gone = t.add_file(docs, "gone.pdf", 1 << 20, 1 << 20, 0, 0);
        t.remove(gone);
        (t, vec![docs, a, b, c, d, e, h])
    }

    #[test]
    fn ext_breakdown_sorted_and_filtered() {
        let (t, ids) = sample();
        let s = ext_breakdown(&t, 0, SizeMode::Logical);
        let named: Vec<_> = s
            .iter()
            .map(|e| (t.ext_name(e.ext), e.files, e.size))
            .collect();
        assert_eq!(
            named,
            vec![
                ("mkv", 1, 5000),
                ("pdf", 3, 1000),
                ("txt", 1, 20),
                ("", 1, 10)
            ]
        );
        let s = ext_breakdown(&t, ids[0], SizeMode::Allocated);
        let named: Vec<_> = s
            .iter()
            .map(|e| (t.ext_name(e.ext), e.files, e.size))
            .collect();
        assert_eq!(named, vec![("pdf", 3, 8192), ("txt", 1, 4096)]);
        // A file as `under` describes itself.
        let s = ext_breakdown(&t, ids[3], SizeMode::Logical);
        assert_eq!(
            s,
            vec![ExtStat {
                ext: t.node(ids[3]).ext(),
                files: 1,
                size: 5000
            }]
        );
        assert!(ext_breakdown(&t, 999_999, SizeMode::Logical).is_empty());
    }

    #[test]
    fn largest_files_top_n() {
        let (t, ids) = sample();
        let (a, b, c, d, e) = (ids[1], ids[2], ids[3], ids[4], ids[5]);
        assert_eq!(largest_files(&t, 0, SizeMode::Logical, 3), vec![c, e, a]);
        assert_eq!(
            largest_files(&t, 0, SizeMode::Logical, 100),
            vec![c, e, a, b, d]
        );
        assert_eq!(largest_files(&t, ids[0], SizeMode::Logical, 2), vec![e, a]);
        // Ties (all 4096 in Allocated mode) are broken by id.
        assert_eq!(
            largest_files(&t, ids[0], SizeMode::Allocated, 2),
            vec![a, b]
        );
        assert!(largest_files(&t, 0, SizeMode::Logical, 0).is_empty());
    }

    #[test]
    fn largest_files_matches_full_sort_on_random_tree() {
        let t = crate::synth::tree(3, 30_000);
        let mut all: Vec<(u64, NodeId)> = t
            .live_nodes()
            .filter(|(_, n)| n.is_file() && n.alloc() > 0)
            .map(|(id, n)| (n.alloc(), id))
            .collect();
        all.sort_by(|x, y| y.0.cmp(&x.0).then(x.1.cmp(&y.1)));
        let want: Vec<NodeId> = all.iter().take(50).map(|x| x.1).collect();
        assert_eq!(largest_files(&t, 0, SizeMode::Allocated, 50), want);
    }

    #[test]
    fn search_case_insensitive_unicode_and_limit() {
        let (t, ids) = sample();
        let (docs, a, e) = (ids[0], ids[1], ids[5]);
        assert_eq!(search(&t, 0, "report", 10), vec![a]);
        assert_eq!(search(&t, 0, "DOCS", 10), vec![docs]);
        assert_eq!(search(&t, 0, "ЗВІТ", 10), vec![e]);
        assert_eq!(
            search(&t, 0, ".pdf", 10).len(),
            3,
            "hard link included, tombstone/virtual not"
        );
        assert_eq!(search(&t, 0, ".pdf", 2).len(), 2);
        assert!(search(&t, 0, "free", 10).is_empty());
        assert!(search(&t, 0, "", 10).is_empty());
        assert!(search(&t, 0, "gone", 10).is_empty());
        assert!(
            search(&t, docs, "docs", 10).is_empty(),
            "under itself excluded"
        );
        assert!(search(&t, 0, "x", 0).is_empty());
    }

    #[test]
    fn search_descends_into_virtual_containers() {
        let mut t = Tree::new("C:\\", 0);
        let o = t.add_virtual(0, "<orphaned>", DIR, 0);
        let f = t.add_file(o, "lost.dat", 1, 1, 0, 0);
        assert_eq!(search(&t, 0, "lost", 10), vec![f]);
        assert!(search(&t, 0, "orphaned", 10).is_empty());
        assert_eq!(ext_breakdown(&t, o, SizeMode::Logical).len(), 1);
    }
}
