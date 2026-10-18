//! The session panel's Board section: the message boards of an orchestrator's
//! tree as the selected chat sees them (`ListBoard`): every folder's for the
//! root, its own folder's for a session. Pure, so the grouping is unit-tested
//! without a window; the rows are drawn by `Shell::render_board`.

use chrono::{DateTime, Utc};
use zeron_proto::board::BoardEntry;
use zeron_proto::{Chat, Space};

/// Posts shown per folder; the header counts them all.
pub(super) const POSTS_PER_FOLDER: usize = 20;
/// A post's text past this is cut, the whole of it in the tooltip.
pub(super) const POST_PREVIEW_CHARS: usize = 300;

/// One folder's board.
pub(super) struct BoardGroup {
    pub label: String,
    /// Newest first, at most [`POSTS_PER_FOLDER`].
    pub entries: Vec<BoardEntry>,
    /// Older posts left out.
    pub hidden: usize,
}

pub(super) struct BoardView {
    /// The folder with the latest post first.
    pub groups: Vec<BoardGroup>,
    pub total: usize,
    /// When the first post went up: the section's place in the panel.
    pub first_at: Option<DateTime<Utc>>,
}

impl BoardView {
    /// `None` while nothing has been posted. `label` names a folder key.
    pub fn of(entries: &[BoardEntry], label: impl Fn(&str) -> String) -> Option<Self> {
        if entries.is_empty() {
            return None;
        }
        let mut folders: Vec<(&str, Vec<&BoardEntry>)> = Vec::new();
        for entry in entries {
            match folders.iter_mut().find(|(folder, _)| *folder == entry.folder) {
                Some((_, posts)) => posts.push(entry),
                None => folders.push((&entry.folder, vec![entry])),
            }
        }
        folders.sort_by_key(|(_, posts)| {
            std::cmp::Reverse(posts.iter().map(|post| post.id.clone()).max())
        });
        let groups = folders
            .into_iter()
            .map(|(folder, mut posts)| {
                posts.sort_by(|a, b| b.id.cmp(&a.id));
                let hidden = posts.len().saturating_sub(POSTS_PER_FOLDER);
                BoardGroup {
                    label: label(folder),
                    entries: posts
                        .into_iter()
                        .take(POSTS_PER_FOLDER)
                        .cloned()
                        .collect(),
                    hidden,
                }
            })
            .collect();
        let first_at = entries
            .iter()
            .map(|entry| entry.at)
            .min()
            .and_then(DateTime::<Utc>::from_timestamp_millis);
        Some(Self {
            groups,
            total: entries.len(),
            first_at,
        })
    }

    /// The header's trailing count: `· 12`.
    pub fn count_label(&self) -> String {
        format!("· {}", self.total)
    }

    /// Rows drawn, for the section's height estimate.
    pub fn rows(&self) -> usize {
        self.groups
            .iter()
            .map(|g| 1 + 2 * g.entries.len() + usize::from(g.hidden > 0))
            .sum()
    }
}

/// The posts whose folder still has a session at work. A folder's board is
/// over once every session of the tree in it has finished (`at_work` false
/// for each); posts from before folders existed follow the whole tree.
pub(super) fn live_posts(
    entries: &[BoardEntry],
    members: &[&Chat],
    at_work: impl Fn(&Chat) -> bool,
) -> Vec<BoardEntry> {
    let busy: std::collections::HashSet<String> = members
        .iter()
        .filter(|&&chat| at_work(chat))
        .map(|&chat| zeron_proto::board::folder_of(chat))
        .collect();
    entries
        .iter()
        .filter(|entry| {
            if entry.folder.is_empty() {
                !busy.is_empty()
            } else {
                busy.contains(&entry.folder)
            }
        })
        .cloned()
        .collect()
}

/// A folder key ([`zeron_proto::board::folder_of`]) as the user knows it:
/// the project's name, the directory, or the session.
pub(super) fn folder_label(folder: &str, spaces: &[Space], chats: &[Chat]) -> String {
    if let Some(id) = folder.strip_prefix("space:") {
        return spaces
            .iter()
            .find(|space| space.id == id)
            .map_or_else(|| "A project".to_owned(), |space| space.display_name().to_owned());
    }
    if let Some(dir) = folder.strip_prefix("dir:") {
        return super::compact_path(dir);
    }
    if let Some(id) = folder.strip_prefix("chat:") {
        return chats
            .iter()
            .find(|chat| chat.id == id)
            .and_then(|chat| chat.title.clone())
            .filter(|title| !title.trim().is_empty())
            .unwrap_or_else(|| "One session".to_owned());
    }
    "Whole tree (older posts)".to_owned()
}

/// Who posted: the session's title, the orchestrator, or its short id.
pub(super) fn author(entry: &BoardEntry) -> String {
    if entry.author_depth == 0 {
        return "Orchestrator".to_owned();
    }
    entry
        .author_title
        .as_deref()
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| entry.author_chat_id.chars().take(8).collect())
}

/// The post's text, cut at [`POST_PREVIEW_CHARS`]; `true` when cut.
pub(super) fn preview(text: &str) -> (String, bool) {
    if text.chars().count() <= POST_PREVIEW_CHARS {
        return (text.to_owned(), false);
    }
    let cut: String = text.chars().take(POST_PREVIEW_CHARS).collect();
    (format!("{}…", cut.trim_end()), true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeron_proto::board::BoardKind;

    fn post(id: u32, folder: &str) -> BoardEntry {
        BoardEntry {
            id: format!("{id:013}-000000-dev"),
            root_id: "orchestrator-r".into(),
            folder: folder.into(),
            author_chat_id: "a1b2c3d4-0000".into(),
            author_title: Some("Fix the parser".into()),
            author_depth: 1,
            kind: BoardKind::Finding,
            text: format!("post {id}"),
            at: i64::from(id) * 1_000,
        }
    }

    fn member(id: &str, cwd: &str) -> Chat {
        serde_json::from_value(serde_json::json!({
            "id": id, "deviceId": "local", "archived": false,
            "createdAt": "2026-10-01T00:00:00Z", "cwd": cwd,
        }))
        .unwrap()
    }

    #[test]
    fn a_folder_whose_sessions_all_finished_drops_its_board() {
        let a1 = member("a1", "/a");
        let a2 = member("a2", "/a");
        let b1 = member("b1", "/b");
        let members = [&a1, &a2, &b1];
        let posts = [post(1, "dir:/a"), post(2, "dir:/b"), post(3, "")];
        let texts = |live: Vec<BoardEntry>| live.into_iter().map(|e| e.text).collect::<Vec<_>>();
        // One session of /a still works: /a keeps its board, /b's is over.
        let live = live_posts(&posts, &members, |chat| chat.id == "a2");
        assert_eq!(texts(live), ["post 1", "post 3"]);
        // Everything finished: no posts, so no section at all.
        let live = live_posts(&posts, &members, |_| false);
        assert!(live.is_empty());
        assert!(BoardView::of(&live, str::to_owned).is_none());
    }

    #[test]
    fn nothing_posted_has_no_section() {
        assert!(BoardView::of(&[], str::to_owned).is_none());
    }

    #[test]
    fn posts_group_by_folder_latest_folder_and_post_first() {
        let view = BoardView::of(
            &[post(1, "dir:/a"), post(2, "dir:/b"), post(3, "dir:/a")],
            str::to_owned,
        )
        .unwrap();
        assert_eq!(view.total, 3);
        let labels: Vec<_> = view.groups.iter().map(|g| g.label.as_str()).collect();
        assert_eq!(labels, ["dir:/a", "dir:/b"]);
        let a: Vec<_> = view.groups[0].entries.iter().map(|e| e.text.as_str()).collect();
        assert_eq!(a, ["post 3", "post 1"]);
        assert_eq!(view.first_at.unwrap().timestamp(), 1);
        assert_eq!(view.count_label(), "· 3");
    }

    #[test]
    fn a_long_board_shows_its_newest_posts() {
        let posts: Vec<_> = (1..=25).map(|id| post(id, "dir:/a")).collect();
        let view = BoardView::of(&posts, str::to_owned).unwrap();
        assert_eq!(view.groups[0].entries.len(), POSTS_PER_FOLDER);
        assert_eq!(view.groups[0].hidden, 5);
        assert_eq!(view.groups[0].entries[0].text, "post 25");
    }

    #[test]
    fn folders_and_authors_read_as_names() {
        assert_eq!(folder_label("chat:x", &[], &[]), "One session");
        assert_eq!(folder_label("space:gone", &[], &[]), "A project");
        assert_eq!(folder_label("", &[], &[]), "Whole tree (older posts)");
        let mut root = post(1, "");
        root.author_depth = 0;
        assert_eq!(author(&root), "Orchestrator");
        let mut untitled = post(1, "dir:/a");
        untitled.author_title = None;
        assert_eq!(author(&untitled), "a1b2c3d4");
        assert_eq!(preview("short"), ("short".to_owned(), false));
        let (cut, was_cut) = preview(&"x".repeat(400));
        assert!(was_cut && cut.ends_with('…') && cut.chars().count() == POST_PREVIEW_CHARS + 1);
    }
}
