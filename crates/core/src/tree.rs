//! Collapsible file tree, diffnav-style: build a trie from paths (always
//! split on `/`), collapse single-child directory chains GitHub-style, and
//! flatten to display rows against a collapsed-set. Lives in core so all
//! three frontends show the identical tree.

use std::collections::BTreeSet;

use serde::Serialize;

use crate::model::FileEntry;

/// One node of the built tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TreeNode {
    /// Display segment; collapsed chains join with `/` (e.g. `src/app/ui`).
    pub name: String,
    /// Full path of this node from the root.
    pub path: String,
    /// Index into the input file list for leaves; None for directories.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_index: Option<usize>,
    pub children: Vec<TreeNode>,
}

impl TreeNode {
    pub fn is_dir(&self) -> bool {
        self.file_index.is_none()
    }
}

/// One flattened display row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TreeRow {
    pub depth: usize,
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    /// For directories: whether this row is currently collapsed.
    pub collapsed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_index: Option<usize>,
    /// Open/reopened comment count for this file leaf; 0 for directories and
    /// filled in by the caller (the projection derives it from the review,
    /// which this module knows nothing about).
    #[serde(default)]
    pub comments_todo: usize,
    /// Total comment count for this file leaf.
    #[serde(default)]
    pub comments_total: usize,
}

/// Pass 1+2+3: build the trie, collapse single-child dir chains, sort
/// (directories first, then files, both alphabetical). `file_index` on each
/// leaf is its position in `files`.
pub fn build_file_tree(files: &[FileEntry]) -> Vec<TreeNode> {
    build_tree_from_indices(files, 0..files.len())
}

/// Build a tree from only the files at `indices`, keeping each leaf's
/// `file_index` canonical (its position in the FULL `files` slice, not in
/// the filtered subset). Used to build a filtered tree without renumbering
/// files out from under comment tallies and anchors (S1/B15).
pub fn build_filtered_file_tree(files: &[FileEntry], indices: &[usize]) -> Vec<TreeNode> {
    build_tree_from_indices(files, indices.iter().copied())
}

fn build_tree_from_indices(
    files: &[FileEntry],
    indices: impl Iterator<Item = usize>,
) -> Vec<TreeNode> {
    let mut roots: Vec<TreeNode> = Vec::new();
    for index in indices {
        insert_path(&mut roots, &files[index].path, index);
    }
    let mut roots = roots
        .into_iter()
        .map(collapse_single_child_dirs)
        .collect::<Vec<_>>();
    sort_tree(&mut roots);
    roots
}

fn insert_path(nodes: &mut Vec<TreeNode>, path: &str, file_index: usize) {
    let mut segments = path.split('/').filter(|s| !s.is_empty()).peekable();
    let mut current = nodes;
    let mut prefix = String::new();
    while let Some(segment) = segments.next() {
        if !prefix.is_empty() {
            prefix.push('/');
        }
        prefix.push_str(segment);
        let is_leaf = segments.peek().is_none();
        // A path that is both a file and a directory prefix cannot happen in
        // git listings; still, match on name+kind so nothing merges wrongly.
        let pos = current
            .iter()
            .position(|n| n.name == segment && n.is_dir() == !is_leaf);
        let pos = match pos {
            Some(p) => p,
            None => {
                current.push(TreeNode {
                    name: segment.to_string(),
                    path: prefix.clone(),
                    file_index: is_leaf.then_some(file_index),
                    children: Vec::new(),
                });
                current.len() - 1
            }
        };
        if is_leaf {
            return;
        }
        current = &mut current[pos].children;
    }
}

/// GitHub-style collapse: a directory with exactly one child that is also a
/// directory merges into it (`src` + `app` -> `src/app`).
fn collapse_single_child_dirs(mut node: TreeNode) -> TreeNode {
    while node.is_dir() && node.children.len() == 1 && node.children[0].is_dir() {
        let child = node.children.remove(0);
        node.name = format!("{}/{}", node.name, child.name);
        node.path = child.path;
        node.children = child.children;
    }
    node.children = node
        .children
        .into_iter()
        .map(collapse_single_child_dirs)
        .collect();
    node
}

fn sort_tree(nodes: &mut [TreeNode]) {
    for node in nodes.iter_mut() {
        sort_tree(&mut node.children);
    }
    nodes.sort_by(|a, b| match (a.is_dir(), b.is_dir()) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.name.cmp(&b.name),
    });
}

/// Flatten the tree to display rows, honoring the collapsed-directory set
/// (keyed by node path).
pub fn flatten_tree(nodes: &[TreeNode], collapsed: &BTreeSet<String>) -> Vec<TreeRow> {
    let mut rows = Vec::new();
    flatten_into(nodes, collapsed, 0, &mut rows);
    rows
}

fn flatten_into(
    nodes: &[TreeNode],
    collapsed: &BTreeSet<String>,
    depth: usize,
    rows: &mut Vec<TreeRow>,
) {
    for node in nodes {
        let is_collapsed = node.is_dir() && collapsed.contains(&node.path);
        rows.push(TreeRow {
            depth,
            name: node.name.clone(),
            path: node.path.clone(),
            is_dir: node.is_dir(),
            collapsed: is_collapsed,
            file_index: node.file_index,
            comments_todo: 0,
            comments_total: 0,
        });
        if node.is_dir() && !is_collapsed {
            flatten_into(&node.children, collapsed, depth + 1, rows);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::FileStatus;

    fn files(paths: &[&str]) -> Vec<FileEntry> {
        paths
            .iter()
            .map(|p| FileEntry {
                path: (*p).to_string(),
                old_path: None,
                status: FileStatus::Modified,
                adds: None,
                dels: None,
            })
            .collect()
    }

    #[test]
    fn builds_trie_and_collapses_single_child_chains() {
        let tree = build_file_tree(&files(&[
            "src/app/ui/view.rs",
            "src/app/ui/model.rs",
            "src/lib.rs",
            "README.md",
        ]));
        // Root: "src" (dir), "README.md" (file). "app/ui" collapses under src.
        assert_eq!(tree.len(), 2);
        assert_eq!(tree[0].name, "src");
        assert!(tree[0].is_dir());
        assert_eq!(tree[1].name, "README.md");
        // src has children: app/ui (collapsed dir) and lib.rs.
        let src = &tree[0];
        assert_eq!(src.children[0].name, "app/ui");
        assert_eq!(src.children[0].path, "src/app/ui");
        assert_eq!(src.children[1].name, "lib.rs");
        let ui = &src.children[0];
        assert_eq!(ui.children.len(), 2);
        assert_eq!(ui.children[0].name, "model.rs");
    }

    #[test]
    fn root_single_chain_collapses_but_keeps_leaf() {
        let tree = build_file_tree(&files(&["a/b/c/file.txt"]));
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].name, "a/b/c");
        assert_eq!(tree[0].children[0].name, "file.txt");
        assert_eq!(tree[0].children[0].file_index, Some(0));
    }

    #[test]
    fn dirs_sort_before_files_alphabetically() {
        let tree = build_file_tree(&files(&["z.txt", "b/x.txt", "a.txt", "y/q.txt"]));
        let names: Vec<&str> = tree.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, vec!["b", "y", "a.txt", "z.txt"]);
    }

    #[test]
    fn flatten_honors_collapsed_set() {
        let tree = build_file_tree(&files(&["src/a.rs", "src/b.rs", "top.txt"]));
        let open = flatten_tree(&tree, &BTreeSet::new());
        assert_eq!(open.len(), 4);
        assert_eq!(open[0].name, "src");
        assert_eq!(open[1].depth, 1);

        let mut collapsed = BTreeSet::new();
        collapsed.insert("src".to_string());
        let rows = flatten_tree(&tree, &collapsed);
        assert_eq!(rows.len(), 2);
        assert!(rows[0].collapsed);
        assert_eq!(rows[1].name, "top.txt");
    }

    #[test]
    fn filtered_tree_keeps_canonical_file_indices() {
        let all = files(&["src/a.rs", "src/b.rs", "top.txt"]);
        // Only "src/b.rs" (index 1) passes the filter; its file_index must
        // still be 1, not renumbered to 0 within the filtered subset.
        let tree = build_filtered_file_tree(&all, &[1]);
        let rows = flatten_tree(&tree, &BTreeSet::new());
        let leaf = rows.iter().find(|r| !r.is_dir).expect("one file leaf");
        assert_eq!(leaf.path, "src/b.rs");
        assert_eq!(leaf.file_index, Some(1));
    }

    #[test]
    fn filtered_tree_omits_directories_with_no_passing_descendants() {
        let all = files(&["src/a.rs", "other/b.rs"]);
        let tree = build_filtered_file_tree(&all, &[0]);
        let rows = flatten_tree(&tree, &BTreeSet::new());
        assert!(rows.iter().all(|r| r.path != "other"));
    }

    #[test]
    fn tree_rows_default_to_zero_comment_counts() {
        let tree = build_file_tree(&files(&["a.rs"]));
        let rows = flatten_tree(&tree, &BTreeSet::new());
        assert_eq!((rows[0].comments_todo, rows[0].comments_total), (0, 0));
    }

    #[test]
    fn empty_input_builds_empty_tree() {
        assert!(build_file_tree(&[]).is_empty());
    }

    #[test]
    fn duplicate_leading_slashes_and_segments_are_tolerated() {
        let tree = build_file_tree(&files(&["a//b.txt"]));
        assert_eq!(tree[0].name, "a");
        assert_eq!(tree[0].children[0].name, "b.txt");
    }
}
