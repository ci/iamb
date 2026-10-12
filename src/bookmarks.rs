//! Messages saved for later, kept in each room's account data so they sync across devices.
//!
//! Only event identifiers and notes are stored: account data isn't end-to-end encrypted, so
//! message contents stay out of it.
use std::collections::BTreeMap;

use matrix_sdk::ruma::RoomOrAliasId;
use matrix_sdk::ruma::events::macros::EventContent;
use serde::{Deserialize, Serialize};

use crate::prelude::*;

/// The account data stored in each room with bookmarked messages.
#[derive(Clone, Debug, Default, Deserialize, Serialize, EventContent)]
#[ruma_event(type = "ir.ca7.bookmarks", kind = RoomAccountData)]
pub struct BookmarksEventContent {
    #[serde(default)]
    pub bookmarks: BTreeMap<OwnedEventId, Bookmark>,
}

/// A single bookmarked message.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Bookmark {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,

    pub saved_at: MilliSecondsSinceUnixEpoch,
}

/// A bookmark, along with where it lives.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BookmarkEntry {
    pub room_id: OwnedRoomId,
    pub event_id: OwnedEventId,
    pub bookmark: Bookmark,
}

/// Every room's bookmarks.
#[derive(Debug, Default)]
pub struct Bookmarks {
    rooms: HashMap<OwnedRoomId, BookmarksEventContent>,
}

impl Bookmarks {
    /// Replace a room's bookmarks with what's in its account data.
    pub fn set_room(&mut self, room_id: OwnedRoomId, content: BookmarksEventContent) {
        if content.bookmarks.is_empty() {
            self.rooms.remove(&room_id);
        } else {
            self.rooms.insert(room_id, content);
        }
    }

    pub fn get(&self, room_id: &RoomId, event_id: &EventId) -> Option<&Bookmark> {
        self.rooms.get(room_id)?.bookmarks.get(event_id)
    }

    /// The room's bookmarks after saving a message, to store in its account data.
    pub fn with_saved(
        &self,
        room_id: &RoomId,
        event_id: OwnedEventId,
        note: Option<String>,
        now: MilliSecondsSinceUnixEpoch,
    ) -> BookmarksEventContent {
        let mut content = self.rooms.get(room_id).cloned().unwrap_or_default();
        let saved_at = content.bookmarks.get(&event_id).map(|b| b.saved_at).unwrap_or(now);
        content.bookmarks.insert(event_id, Bookmark { note, saved_at });
        content
    }

    /// The room's bookmarks after removing a message, or `None` if it wasn't bookmarked.
    pub fn without(&self, room_id: &RoomId, event_id: &EventId) -> Option<BookmarksEventContent> {
        let mut content = self.rooms.get(room_id)?.clone();
        content.bookmarks.remove(event_id)?;
        Some(content)
    }

    /// All bookmarks, most recently saved first.
    pub fn newest_first(&self) -> Vec<BookmarkEntry> {
        let mut entries = self
            .rooms
            .iter()
            .flat_map(|(room_id, content)| {
                content.bookmarks.iter().map(|(event_id, bookmark)| {
                    BookmarkEntry {
                        room_id: room_id.clone(),
                        event_id: event_id.clone(),
                        bookmark: bookmark.clone(),
                    }
                })
            })
            .collect::<Vec<_>>();

        entries.sort_by(|a, b| {
            b.bookmark
                .saved_at
                .cmp(&a.bookmark.saved_at)
                .then(a.event_id.cmp(&b.event_id))
        });
        entries
    }
}

/// The text to post in the bookmarks room for a newly saved message.
///
/// The message's contents are only included when `excerpt` is given, so that callers can leave
/// it out when posting it would expose an encrypted message in an unencrypted room.
pub fn mirror_text(
    room_title: &str,
    sender: &str,
    excerpt: Option<&str>,
    note: Option<&str>,
    room_id: &RoomId,
    event_id: &EventId,
) -> String {
    let mut text = format!("🔖 {room_title} · {sender}");

    if let Some(excerpt) = excerpt {
        let mut excerpt = excerpt.lines().next().unwrap_or_default().to_string();
        if excerpt.chars().count() > 200 {
            excerpt = excerpt.chars().take(200).collect::<String>() + "…";
        }
        text.push_str("\n> ");
        text.push_str(&excerpt);
    }

    if let Some(note) = note {
        text.push('\n');
        text.push_str(note);
    }

    text.push('\n');
    text.push_str(&room_id.matrix_to_event_uri(event_id).to_string());
    text
}

/// Save or remove the bookmark for a message, storing it in the room's account data.
pub async fn bookmark_command(
    act: BookmarkAction,
    room_id: OwnedRoomId,
    event_id: OwnedEventId,
    store: &mut ProgramStore,
) -> IambResult<EditInfo> {
    let room = store.application.get_joined_room(&room_id).ok_or(IambError::NotJoined)?;
    let bookmarks = &store.application.bookmarks;
    let is_new = bookmarks.get(&room_id, &event_id).is_none();

    let (content, note) = match act {
        BookmarkAction::Save(note) => {
            let now = MilliSecondsSinceUnixEpoch::now();
            let content = bookmarks.with_saved(&room_id, event_id.clone(), note.clone(), now);
            (content, note)
        },
        BookmarkAction::Remove => {
            let Some(content) = bookmarks.without(&room_id, &event_id) else {
                return Err(UIError::Failure("Message isn't bookmarked".into()));
            };

            room.set_account_data(content.clone()).await.map_err(IambError::from)?;
            store.application.bookmarks.set_room(room_id, content);

            return Ok(Some("Removed bookmark".into()));
        },
    };

    room.set_account_data(content.clone()).await.map_err(IambError::from)?;
    store.application.bookmarks.set_room(room_id.clone(), content);

    if !is_new {
        return Ok(Some("Updated bookmark".into()));
    }

    match mirror(&room, &event_id, note.as_deref(), store).await {
        Ok(()) => Ok(Some("Bookmarked message".into())),
        Err(e) => Ok(Some(format!("Bookmarked message, but couldn't post it: {e}").into())),
    }
}

/// Post a newly bookmarked message to the configured bookmarks room, if there is one.
async fn mirror(
    room: &MatrixRoom,
    event_id: &EventId,
    note: Option<&str>,
    store: &mut ProgramStore,
) -> IambResult<()> {
    let ChatStore { settings, aliases, rooms, worker, .. } = &store.application;

    let Some(target) = &settings.tunables.bookmarks_room else {
        return Ok(());
    };

    let target: &RoomOrAliasId = target;
    let target_id = match <&RoomId>::try_from(target) {
        Ok(room_id) => room_id.to_owned(),
        Err(alias) => {
            match aliases.get(alias) {
                Some(room_id) => room_id.to_owned(),
                None => {
                    let resp = worker
                        .client
                        .resolve_room_alias(alias)
                        .await
                        .map_err(|e| IambError::from(matrix_sdk::Error::from(e)))?;
                    resp.room_id
                },
            }
        },
    };

    let notes = worker.client.get_room(&target_id).ok_or(IambError::NotJoined)?;
    let info = rooms.get(room.room_id());
    let msg = info.and_then(|info| info.get_event(event_id));

    // Don't copy an encrypted message's contents into an unencrypted room.
    let exposes =
        room.encryption_state().is_encrypted() && !notes.encryption_state().is_encrypted();
    let body = msg.filter(|_| !exposes).map(|msg| msg.event.body());

    let title = info
        .and_then(|info| info.name.clone())
        .unwrap_or_else(|| room.room_id().to_string());
    let sender = msg
        .map(|msg| settings.get_user_span_maybe(&msg.sender, info).content.into_owned())
        .unwrap_or_default();

    let text = mirror_text(&title, &sender, body.as_deref(), note, room.room_id(), event_id);
    notes
        .send(RoomMessageEventContent::text_plain(text))
        .await
        .map_err(IambError::from)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use matrix_sdk::ruma::{event_id, room_id};

    fn ts(ms: u64) -> MilliSecondsSinceUnixEpoch {
        MilliSecondsSinceUnixEpoch(ms.try_into().unwrap())
    }

    #[test]
    fn test_save_and_remove() {
        let mut bookmarks = Bookmarks::default();
        let room = room_id!("!a:example.com");
        let ev1 = event_id!("$1:example.com");
        let ev2 = event_id!("$2:example.com");

        let content = bookmarks.with_saved(room, ev1.to_owned(), None, ts(10));
        bookmarks.set_room(room.to_owned(), content);
        let content = bookmarks.with_saved(room, ev2.to_owned(), Some("todo".into()), ts(20));
        bookmarks.set_room(room.to_owned(), content);

        let entries = bookmarks.newest_first();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].event_id, ev2);
        assert_eq!(entries[0].bookmark.note.as_deref(), Some("todo"));

        // Updating a note keeps when it was first saved.
        let content = bookmarks.with_saved(room, ev1.to_owned(), Some("later".into()), ts(30));
        bookmarks.set_room(room.to_owned(), content);
        let bookmark = bookmarks.get(room, ev1).unwrap();
        assert_eq!(bookmark.saved_at, ts(10));
        assert_eq!(bookmark.note.as_deref(), Some("later"));

        let content = bookmarks.without(room, ev1).unwrap();
        bookmarks.set_room(room.to_owned(), content);
        assert!(bookmarks.without(room, ev1).is_none());

        let content = bookmarks.without(room, ev2).unwrap();
        bookmarks.set_room(room.to_owned(), content);
        assert!(bookmarks.newest_first().is_empty());
    }

    #[test]
    fn test_content_format() {
        let json = serde_json::json!({
            "bookmarks": {
                "$1:example.com": { "saved_at": 10 },
                "$2:example.com": { "saved_at": 20, "note": "reply to this" },
            },
        });
        let content: BookmarksEventContent = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(content.bookmarks.len(), 2);
        assert_eq!(serde_json::to_value(&content).unwrap(), json);

        // Unknown or missing fields don't break parsing.
        let content: BookmarksEventContent = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(content.bookmarks.is_empty());
    }

    #[test]
    fn test_mirror_text() {
        let room = room_id!("!a:example.com");
        let ev = event_id!("$1:example.com");

        let text = mirror_text("Room", "Alice", Some("hello\nworld"), Some("note"), room, ev);
        assert_eq!(
            text,
            "🔖 Room · Alice\n> hello\nnote\nhttps://matrix.to/#/!a:example.com/$1:example.com"
        );

        let text = mirror_text("Room", "Alice", None, None, room, ev);
        assert_eq!(text, "🔖 Room · Alice\nhttps://matrix.to/#/!a:example.com/$1:example.com");
    }
}
