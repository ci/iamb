//! Messages saved for later, kept in each room's account data so they sync across devices.
//!
//! Only event identifiers and notes are stored: account data isn't end-to-end encrypted, so
//! message contents stay out of it.
use std::collections::BTreeMap;

use matrix_sdk::ruma::RoomOrAliasId;
use matrix_sdk::ruma::api::client::config::get_room_account_data;
use matrix_sdk::ruma::api::error::ErrorKind;
use matrix_sdk::ruma::events::StaticEventContent;
use matrix_sdk::ruma::events::macros::EventContent;
use serde::{Deserialize, Serialize};

use crate::base::NoteUpdate;
use crate::prelude::*;

/// The account data stored in each room with bookmarked messages.
#[derive(Clone, Debug, Default, Deserialize, Serialize, EventContent)]
#[ruma_event(type = "ir.ca7.bookmarks", kind = RoomAccountData)]
pub struct BookmarksEventContent {
    #[serde(default)]
    pub bookmarks: BTreeMap<OwnedEventId, Bookmark>,

    /// Fields from newer versions, kept so that saving doesn't drop them.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

impl BookmarksEventContent {
    /// Bookmark a message, or update the note of an existing bookmark.
    pub fn save(
        &mut self,
        event_id: OwnedEventId,
        note: NoteUpdate,
        now: MilliSecondsSinceUnixEpoch,
    ) {
        let bookmark = self.bookmarks.entry(event_id).or_insert_with(|| {
            Bookmark {
                note: None,
                saved_at: now,
                extra: Default::default(),
            }
        });

        match note {
            NoteUpdate::Keep => {},
            NoteUpdate::Set(note) => bookmark.note = Some(note),
            NoteUpdate::Clear => bookmark.note = None,
        }
    }

    /// Remove a bookmark, returning whether there was one.
    pub fn remove(&mut self, event_id: &EventId) -> bool {
        self.bookmarks.remove(event_id).is_some()
    }
}

/// A single bookmarked message.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Bookmark {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,

    pub saved_at: MilliSecondsSinceUnixEpoch,

    /// Fields from newer versions, kept so that saving doesn't drop them.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
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

    #[cfg(test)]
    pub fn get(&self, room_id: &RoomId, event_id: &EventId) -> Option<&Bookmark> {
        self.rooms.get(room_id)?.bookmarks.get(event_id)
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

    let user_id = &store.application.settings.profile.user_id;
    let mut content = fetch_content(&room, user_id).await?;

    let is_new = !content.bookmarks.contains_key(&event_id);

    match act {
        BookmarkAction::Save(note) => {
            content.save(event_id.clone(), note, MilliSecondsSinceUnixEpoch::now());
        },
        BookmarkAction::Remove => {
            if !content.remove(&event_id) {
                return Err(UIError::Failure("Message isn't bookmarked".into()));
            }
        },
    }

    room.set_account_data(content.clone()).await.map_err(IambError::from)?;

    let note = content.bookmarks.get(&event_id).and_then(|b| b.note.clone());
    let removed = !content.bookmarks.contains_key(&event_id);
    store.application.set_bookmarks(room_id.clone(), content);

    if removed {
        return Ok(Some("Removed bookmark".into()));
    }

    if !is_new {
        return Ok(Some("Updated bookmark".into()));
    }

    match mirror(&room, &event_id, note.as_deref(), store).await {
        Ok(()) => Ok(Some("Bookmarked message".into())),
        Err(e) => Ok(Some(format!("Bookmarked message, but couldn't post it: {e}").into())),
    }
}

/// Fetch a room's bookmarks from the homeserver, to modify and write back.
///
/// Neither our copy nor the SDK's will include our own recent writes until they come back
/// through a sync, so starting from either could drop bookmarks saved in quick succession.
async fn fetch_content(room: &MatrixRoom, user_id: &UserId) -> IambResult<BookmarksEventContent> {
    let event_type = BookmarksEventContent::TYPE.into();
    let request = get_room_account_data::v3::Request::new(
        user_id.to_owned(),
        room.room_id().to_owned(),
        event_type,
    );

    match room.client().send(request).await {
        Ok(resp) => {
            resp.account_data.deserialize_as_unchecked().map_err(|e| {
                let msg = format!("Not changing bookmarks, the saved ones couldn't be read: {e}");
                UIError::Failure(msg)
            })
        },
        Err(e) if matches!(e.client_api_error_kind(), Some(ErrorKind::NotFound)) => {
            Ok(BookmarksEventContent::default())
        },
        Err(e) => Err(IambError::from(e).into()),
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
        let mut content = BookmarksEventContent::default();
        let room = room_id!("!a:example.com");
        let ev1 = event_id!("$1:example.com");
        let ev2 = event_id!("$2:example.com");

        content.save(ev1.to_owned(), NoteUpdate::Keep, ts(10));
        content.save(ev2.to_owned(), NoteUpdate::Set("todo".into()), ts(20));
        bookmarks.set_room(room.to_owned(), content.clone());

        let entries = bookmarks.newest_first();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].event_id, ev2);
        assert_eq!(entries[0].bookmark.note.as_deref(), Some("todo"));

        // Saving again keeps when it was first saved, and the note unless it's changed.
        content.save(ev2.to_owned(), NoteUpdate::Keep, ts(30));
        let bookmark = &content.bookmarks[ev2];
        assert_eq!(bookmark.saved_at, ts(20));
        assert_eq!(bookmark.note.as_deref(), Some("todo"));

        content.save(ev2.to_owned(), NoteUpdate::Set("later".into()), ts(30));
        assert_eq!(content.bookmarks[ev2].note.as_deref(), Some("later"));
        content.save(ev2.to_owned(), NoteUpdate::Clear, ts(30));
        assert_eq!(content.bookmarks[ev2].note, None);

        assert!(content.remove(ev1));
        assert!(!content.remove(ev1));
        assert!(content.remove(ev2));

        bookmarks.set_room(room.to_owned(), content);
        assert!(bookmarks.newest_first().is_empty());
        assert!(bookmarks.get(room, ev2).is_none());
    }

    #[test]
    fn test_unknown_fields_kept() {
        let json = serde_json::json!({
            "bookmarks": {
                "$1:example.com": { "saved_at": 10, "due": 99 },
            },
            "version": 2,
        });
        let mut content: BookmarksEventContent = serde_json::from_value(json).unwrap();
        content.save(event_id!("$2:example.com").to_owned(), NoteUpdate::Keep, ts(20));

        let saved = serde_json::to_value(&content).unwrap();
        assert_eq!(saved["version"], 2);
        assert_eq!(saved["bookmarks"]["$1:example.com"]["due"], 99);
        assert_eq!(saved["bookmarks"]["$2:example.com"]["saved_at"], 20);
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
