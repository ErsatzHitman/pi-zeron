use super::*;
use zeron_proto::board::{BOARD_MAX_ENTRIES, BoardEntry};

impl RegistryDoc {
    /// Post `entry` to its tree's board, stamping its id (an HLC, so posts
    /// sort in order across devices) and time. The oldest posts beyond
    /// [`BOARD_MAX_ENTRIES`] for the tree are dropped.
    pub fn post_board_entry(&mut self, mut entry: BoardEntry) -> Result<BoardEntry, DocError> {
        entry.id = self.next_hlc();
        entry.at = Self::now_ms();
        let Value::Object(set) = serde_json::to_value(&entry)? else {
            return Err(DocError::Schema("board entry is not an object".into()));
        };
        let id = entry.id.clone();
        self.write(KIND_BOARD, &id, OpKind::Upsert, set.into_iter().collect());
        let posts = self.board_entries(&entry.root_id);
        if posts.len() > BOARD_MAX_ENTRIES {
            let old: Vec<&str> = posts[..posts.len() - BOARD_MAX_ENTRIES]
                .iter()
                .map(|post| post.id.as_str())
                .collect();
            let keys: Vec<(&str, &str)> = old.iter().map(|id| (KIND_BOARD, *id)).collect();
            self.delete_row_ops(&keys);
        }
        Ok(entry)
    }

    /// Every post of the tree rooted at `root_id`, oldest first.
    pub fn board_entries(&self, root_id: &str) -> Vec<BoardEntry> {
        let mut posts: Vec<BoardEntry> = self
            .read_kind::<BoardEntry>(KIND_BOARD)
            .into_iter()
            .filter(|post| post.root_id == root_id)
            .collect();
        posts.sort_by(|a, b| a.id.cmp(&b.id));
        posts
    }

    /// Drop a tree's board with its root.
    pub(super) fn delete_board(&mut self, root_id: &str) {
        let posts = self.board_entries(root_id);
        let keys: Vec<(&str, &str)> = posts
            .iter()
            .map(|post| (KIND_BOARD, post.id.as_str()))
            .collect();
        self.delete_row_ops(&keys);
    }
}
