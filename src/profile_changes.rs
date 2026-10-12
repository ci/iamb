//! Display name and avatar changes, collected across rooms.
//!
//! A user changing their profile produces an `m.room.member` event in every room they're in,
//! so these are deduplicated into a single entry per change for the `:nicks` window.
use std::collections::BTreeSet;

use matrix_sdk::ruma::events::room::member::MembershipChange;
use matrix_sdk::ruma::events::{AnyStateEventContentChange, StateEventContentChange};

use crate::prelude::*;

/// How far apart identical changes in different rooms can be and still be treated as one.
const MERGE_WINDOW_MS: u64 = 60 * 60 * 1000;

/// The maximum number of entries to remember.
const MAX_ENTRIES: usize = 500;

/// A change to a user's display name and/or avatar.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProfileChange {
    pub user_id: OwnedUserId,

    /// The previous and new display names, if the display name changed.
    pub displayname: Option<(Option<String>, Option<String>)>,

    /// Whether the avatar changed.
    pub avatar: bool,
}

impl ProfileChange {
    /// Extract a profile change from a state event, if it's a membership event that only changes
    /// the user's display name or avatar.
    pub fn from_state(ev: &AnySyncStateEvent) -> Option<Self> {
        let AnyStateEventContentChange::RoomMember(StateEventContentChange::Original {
            content,
            prev_content,
        }) = ev.content_change()
        else {
            return None;
        };

        let user_id = UserId::parse(ev.state_key()).ok()?;
        let prev_details = prev_content.as_ref().map(|p| p.details());

        let MembershipChange::ProfileChanged { displayname_change, avatar_url_change } =
            content.membership_change(prev_details, ev.sender(), &user_id)
        else {
            return None;
        };

        let displayname = displayname_change
            .map(|change| (change.old.map(ToOwned::to_owned), change.new.map(ToOwned::to_owned)));
        let avatar = avatar_url_change.is_some();

        Some(ProfileChange { user_id, displayname, avatar })
    }

    /// Describe the change, without the user who made it.
    pub fn describe(&self) -> String {
        let name = match &self.displayname {
            Some((Some(old), Some(new))) => Some(format!("{old:?} → {new:?}")),
            Some((None, Some(new))) => Some(format!("set display name to {new:?}")),
            Some((Some(old), None)) => Some(format!("removed display name {old:?}")),
            Some((None, None)) | None => None,
        };

        match (name, self.avatar) {
            (Some(name), true) => format!("{name} (and changed avatar)"),
            (Some(name), false) => name,
            (None, true) => "changed avatar".into(),
            (None, false) => "changed profile".into(),
        }
    }
}

/// A profile change, and every room it was seen in.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProfileChangeEntry {
    pub change: ProfileChange,
    pub first_ts: MilliSecondsSinceUnixEpoch,
    pub last_ts: MilliSecondsSinceUnixEpoch,
    pub rooms: BTreeSet<OwnedRoomId>,
}

impl ProfileChangeEntry {
    fn overlaps(&self, ts: MilliSecondsSinceUnixEpoch) -> bool {
        let ts = u64::from(ts.get());
        let first = u64::from(self.first_ts.get()).saturating_sub(MERGE_WINDOW_MS);
        let last = u64::from(self.last_ts.get()).saturating_add(MERGE_WINDOW_MS);

        (first..=last).contains(&ts)
    }
}

/// Deduplicated profile changes seen across all rooms.
#[derive(Debug, Default)]
pub struct ProfileChanges {
    entries: Vec<ProfileChangeEntry>,
}

impl ProfileChanges {
    /// Record a profile change seen in a room.
    pub fn insert(
        &mut self,
        room_id: OwnedRoomId,
        change: ProfileChange,
        ts: MilliSecondsSinceUnixEpoch,
    ) {
        if let Some(entry) = self.entries.iter_mut().find(|e| e.change == change && e.overlaps(ts))
        {
            entry.first_ts = entry.first_ts.min(ts);
            entry.last_ts = entry.last_ts.max(ts);
            entry.rooms.insert(room_id);
            return;
        }

        self.entries.push(ProfileChangeEntry {
            change,
            first_ts: ts,
            last_ts: ts,
            rooms: BTreeSet::from([room_id]),
        });

        if self.entries.len() > MAX_ENTRIES &&
            let Some((idx, _)) = self.entries.iter().enumerate().min_by_key(|(_, e)| e.last_ts)
        {
            self.entries.swap_remove(idx);
        }
    }

    /// Record the state event if it's a profile change, returning whether it was one.
    pub fn insert_state(&mut self, room_id: &RoomId, ev: &AnySyncStateEvent) -> bool {
        let Some(change) = ProfileChange::from_state(ev) else {
            return false;
        };

        self.insert(room_id.to_owned(), change, ev.origin_server_ts());

        true
    }

    /// All entries, most recent first.
    pub fn newest_first(&self) -> Vec<&ProfileChangeEntry> {
        let mut entries = self.entries.iter().collect::<Vec<_>>();
        entries.sort_by_key(|e| std::cmp::Reverse(e.last_ts));
        entries
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use matrix_sdk::ruma::{room_id, uint, user_id};

    fn ts(ms: u64) -> MilliSecondsSinceUnixEpoch {
        MilliSecondsSinceUnixEpoch(ms.try_into().unwrap())
    }

    fn rename(old: &str, new: &str) -> ProfileChange {
        ProfileChange {
            user_id: user_id!("@alice:example.com").to_owned(),
            displayname: Some((Some(old.into()), Some(new.into()))),
            avatar: false,
        }
    }

    fn member_event(prev_name: &str, name: &str, membership: &str) -> AnySyncStateEvent {
        serde_json::from_value(serde_json::json!({
            "type": "m.room.member",
            "event_id": "$member:example.com",
            "sender": "@alice:example.com",
            "state_key": "@alice:example.com",
            "origin_server_ts": 10,
            "content": { "membership": membership, "displayname": name },
            "unsigned": {
                "prev_content": { "membership": "join", "displayname": prev_name },
            },
        }))
        .unwrap()
    }

    #[test]
    fn test_from_state() {
        let ev = member_event("Alice", "Alice (OOO)", "join");
        let change = ProfileChange::from_state(&ev).unwrap();
        assert_eq!(change, rename("Alice", "Alice (OOO)"));
        assert_eq!(change.describe(), "\"Alice\" → \"Alice (OOO)\"");

        // Leaving isn't a profile change, even if the display name differs.
        let ev = member_event("Alice", "Alice (OOO)", "leave");
        assert_eq!(ProfileChange::from_state(&ev), None);
    }

    #[test]
    fn test_merge_across_rooms() {
        let mut changes = ProfileChanges::default();
        let room1 = room_id!("!a:example.com").to_owned();
        let room2 = room_id!("!b:example.com").to_owned();
        let room3 = room_id!("!c:example.com").to_owned();

        changes.insert(room1.clone(), rename("Alice", "Alice (OOO)"), ts(1_000));
        changes.insert(room2.clone(), rename("Alice", "Alice (OOO)"), ts(5_000));
        // Re-inserting the same room is a no-op.
        changes.insert(room2.clone(), rename("Alice", "Alice (OOO)"), ts(5_000));

        assert_eq!(changes.len(), 1);
        let entry = changes.newest_first()[0];
        assert_eq!(entry.rooms, BTreeSet::from([room1.clone(), room2.clone()]));
        assert_eq!(entry.first_ts, ts(1_000));
        assert_eq!(entry.last_ts, ts(5_000));

        // Changing back is a different entry.
        changes.insert(room3.clone(), rename("Alice (OOO)", "Alice"), ts(9_000));
        assert_eq!(changes.len(), 2);
        assert_eq!(changes.newest_first()[0].change, rename("Alice (OOO)", "Alice"));

        // The same change much later on is a separate entry.
        let later = MERGE_WINDOW_MS * 3;
        changes.insert(room1, rename("Alice", "Alice (OOO)"), ts(later));
        assert_eq!(changes.len(), 3);
        assert_eq!(changes.newest_first()[0].last_ts.get(), uint!(10_800_000));
    }

    #[test]
    fn test_capacity() {
        let mut changes = ProfileChanges::default();
        let room = room_id!("!a:example.com");

        for i in 0..(MAX_ENTRIES as u64 + 10) {
            let change = rename("Alice", &format!("Alice {i}"));
            changes.insert(room.to_owned(), change, ts(i));
        }

        assert_eq!(changes.len(), MAX_ENTRIES);
        // The oldest entries are the ones that got dropped.
        assert!(changes.newest_first().iter().all(|e| e.last_ts >= ts(10)));
    }
}
