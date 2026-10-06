use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use fuser::INodeNo;

#[derive(Debug, Clone)]
struct InodeNode {
    paths: BTreeSet<PathBuf>,
    identity: (u64, u64),
    lookups: u64,
    opens: u64,
    pins: u64,
}

#[derive(Debug)]
pub(super) struct InodeTable {
    next: u64,
    by_path: HashMap<PathBuf, INodeNo>,
    by_identity: HashMap<(u64, u64), INodeNo>,
    nodes: HashMap<INodeNo, InodeNode>,
}

impl InodeTable {
    pub(super) fn new(root: PathBuf, identity: (u64, u64)) -> Self {
        let root_ino = INodeNo::ROOT;
        let mut paths = BTreeSet::new();
        paths.insert(root.clone());
        Self {
            next: 2,
            by_path: HashMap::from([(root, root_ino)]),
            by_identity: HashMap::from([(identity, root_ino)]),
            nodes: HashMap::from([(
                root_ino,
                InodeNode {
                    paths,
                    identity,
                    lookups: 0,
                    opens: 0,
                    pins: 0,
                },
            )]),
        }
    }

    pub(super) fn path(&self, ino: INodeNo) -> Option<PathBuf> {
        self.nodes.get(&ino)?.paths.iter().next().cloned()
    }

    pub(super) fn get_or_insert(&mut self, path: PathBuf, identity: (u64, u64)) -> INodeNo {
        if let Some(ino) = self.by_path.get(&path).copied() {
            if let Some(node) = self.nodes.get_mut(&ino)
                && node.identity != identity
            {
                self.by_identity.remove(&node.identity);
                node.identity = identity;
                self.by_identity.insert(identity, ino);
            }
            return ino;
        }
        if let Some(ino) = self.by_identity.get(&identity).copied() {
            if let Some(node) = self.nodes.get_mut(&ino) {
                node.paths.insert(path.clone());
            }
            self.by_path.insert(path, ino);
            return ino;
        }
        let ino = INodeNo(self.next);
        self.next = self.next.saturating_add(1);
        let mut paths = BTreeSet::new();
        paths.insert(path.clone());
        self.by_path.insert(path, ino);
        self.by_identity.insert(identity, ino);
        self.nodes.insert(
            ino,
            InodeNode {
                paths,
                identity,
                lookups: 0,
                opens: 0,
                pins: 0,
            },
        );
        ino
    }

    pub(super) fn remember(&mut self, ino: INodeNo) {
        if let Some(node) = self.nodes.get_mut(&ino) {
            node.lookups = node.lookups.saturating_add(1);
        }
    }

    pub(super) fn open(&mut self, ino: INodeNo) {
        if let Some(node) = self.nodes.get_mut(&ino) {
            node.opens = node.opens.saturating_add(1);
        }
    }

    pub(super) fn close(&mut self, ino: INodeNo) {
        if let Some(node) = self.nodes.get_mut(&ino) {
            node.opens = node.opens.saturating_sub(1);
        }
        self.cleanup_if_unused(ino);
    }

    pub(super) fn pin(&mut self, ino: INodeNo) {
        if let Some(node) = self.nodes.get_mut(&ino) {
            node.pins = node.pins.saturating_add(1);
        }
    }

    pub(super) fn unpin(&mut self, ino: INodeNo) {
        if let Some(node) = self.nodes.get_mut(&ino) {
            node.pins = node.pins.saturating_sub(1);
        }
        self.cleanup_if_unused(ino);
    }

    pub(super) fn forget(&mut self, ino: INodeNo, nlookup: u64) {
        if ino == INodeNo::ROOT {
            return;
        }
        if let Some(node) = self.nodes.get_mut(&ino) {
            node.lookups = node.lookups.saturating_sub(nlookup);
        }
        self.cleanup_if_unused(ino);
    }

    pub(super) fn cleanup_if_unused(&mut self, ino: INodeNo) {
        if ino == INodeNo::ROOT
            || !self
                .nodes
                .get(&ino)
                .is_some_and(|node| node.lookups == 0 && node.opens == 0 && node.pins == 0)
        {
            return;
        }
        let Some(node) = self.nodes.remove(&ino) else {
            return;
        };
        if self.by_identity.get(&node.identity).copied() == Some(ino) {
            self.by_identity.remove(&node.identity);
        }
        for path in node.paths {
            if self.by_path.get(&path).copied() == Some(ino) {
                self.by_path.remove(&path);
            }
        }
    }

    pub(super) fn alias(&mut self, ino: INodeNo, path: PathBuf) {
        if let Some(node) = self.nodes.get_mut(&ino) {
            node.paths.insert(path.clone());
            self.by_path.insert(path, ino);
        }
    }

    pub(super) fn remove_path(&mut self, path: &Path) {
        let Some(ino) = self.by_path.remove(path) else {
            return;
        };
        if let Some(node) = self.nodes.get_mut(&ino) {
            node.paths.remove(path);
        }
        self.cleanup_if_unused(ino);
    }

    pub(super) fn rename_prefix(&mut self, old: &Path, new: &Path) {
        let affected = self
            .by_path
            .keys()
            .filter(|path| path.starts_with(old))
            .cloned()
            .collect::<Vec<_>>();
        for old_path in affected {
            let Some(ino) = self.by_path.remove(&old_path) else {
                continue;
            };
            let suffix = old_path.strip_prefix(old).unwrap_or_else(|_| Path::new(""));
            let new_path = new.join(suffix);
            if let Some(node) = self.nodes.get_mut(&ino) {
                node.paths.remove(&old_path);
                node.paths.insert(new_path.clone());
            }
            self.by_path.insert(new_path, ino);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::InodeTable;
    use std::path::PathBuf;

    #[test]
    fn inode_forget_waits_for_open_and_directory_pins() {
        let root = PathBuf::from("/source");
        let child = root.join("child");
        let mut table = InodeTable::new(root, (1, 1));
        let ino = table.get_or_insert(child.clone(), (1, 2));

        table.remember(ino);
        table.open(ino);
        table.pin(ino);
        table.forget(ino, 1);
        assert!(table.nodes.contains_key(&ino));
        assert_eq!(table.by_path.get(&child).copied(), Some(ino));

        table.close(ino);
        assert!(table.nodes.contains_key(&ino));
        table.unpin(ino);
        assert!(!table.nodes.contains_key(&ino));
        assert!(!table.by_path.contains_key(&child));
        assert!(!table.by_identity.contains_key(&(1, 2)));
    }

    #[test]
    fn unlink_keeps_inode_until_open_handle_closes() {
        let root = PathBuf::from("/source");
        let child = root.join("child");
        let mut table = InodeTable::new(root, (1, 1));
        let ino = table.get_or_insert(child.clone(), (1, 2));

        table.open(ino);
        table.remove_path(&child);
        assert!(table.nodes.contains_key(&ino));
        assert!(!table.by_path.contains_key(&child));

        table.close(ino);
        assert!(!table.nodes.contains_key(&ino));
        assert!(!table.by_identity.contains_key(&(1, 2)));
    }
}
