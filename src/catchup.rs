//! Catching up on unread messages across rooms without marking them as read.
//!
//! Messages shown in the `:catchup` window only get marked as read when asked to, and those
//! read receipts are held back for a short while so that they can still be undone.
use std::collections::{BTreeSet, HashSet};

use serde::{Deserialize, Serialize};

use crate::prelude::*;

/// A read receipt that hasn't been sent yet.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingRead {
    pub room_id: OwnedRoomId,
    pub event_id: OwnedEventId,
    pub key: MessageKey,
    pub due: Instant,
    batch: u64,
}

/// Read receipts waiting for their undo window to pass.
#[derive(Debug, Default)]
pub struct PendingReads {
    reads: Vec<PendingRead>,
    next_batch: u64,
}

impl PendingReads {
    /// Queue a batch of read receipts, which can be undone together.
    pub fn add(&mut self, reads: Vec<(OwnedRoomId, OwnedEventId, MessageKey)>, due: Instant) {
        let batch = self.next_batch;
        self.next_batch += 1;

        self.reads.extend(
            reads
                .into_iter()
                .map(|(room_id, event_id, key)| PendingRead { room_id, event_id, key, due, batch }),
        );
    }

    /// Drop the most recently queued batch, returning how many receipts it had.
    pub fn undo(&mut self) -> usize {
        let Some(batch) = self.reads.iter().map(|r| r.batch).max() else {
            return 0;
        };

        let before = self.reads.len();
        self.reads.retain(|r| r.batch != batch);
        before - self.reads.len()
    }

    /// Remove and return the receipts that are ready to be sent.
    pub fn take_due(&mut self, now: Instant) -> Vec<PendingRead> {
        let (due, waiting) =
            std::mem::take(&mut self.reads).into_iter().partition(|r| r.due <= now);
        self.reads = waiting;
        due
    }

    /// Remove and return every queued receipt.
    pub fn take_all(&mut self) -> Vec<PendingRead> {
        std::mem::take(&mut self.reads)
    }

    /// Remove and return the newest queued receipt for each room.
    ///
    /// Use this when sending receipts concurrently, since two receipts for the same room can
    /// otherwise land out of order and leave the older one in place.
    pub fn take_newest(&mut self) -> Vec<PendingRead> {
        let mut newest: HashMap<OwnedRoomId, PendingRead> = HashMap::new();

        for read in self.take_all() {
            match newest.get(&read.room_id) {
                Some(current) if current.key >= read.key => {},
                _ => {
                    newest.insert(read.room_id.clone(), read);
                },
            }
        }

        newest.into_values().collect()
    }

    /// The newest message queued to be marked read in a room.
    pub fn read_up_to(&self, room_id: &RoomId) -> Option<&MessageKey> {
        self.reads.iter().filter(|r| r.room_id == room_id).map(|r| &r.key).max()
    }

    pub fn len(&self) -> usize {
        self.reads.len()
    }

    pub fn is_empty(&self) -> bool {
        self.reads.is_empty()
    }
}

/// How far the user has read into a room's main timeline.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReadPoint {
    /// The last read message is loaded.
    Loaded(MessageKey),
    /// The last read message hasn't been loaded yet.
    Unloaded(OwnedEventId),
    /// We don't know of any read receipt in this room.
    Unknown,
}

impl ReadPoint {
    /// Work out the read point from the receipt the SDK tracks, our own local receipts, and any
    /// receipt that's still waiting to be sent.
    pub fn new(
        info: &RoomInfo,
        sdk_receipt: Option<&EventId>,
        pending: Option<&MessageKey>,
        user_id: &UserId,
    ) -> Self {
        let local = [ReceiptThread::Main, ReceiptThread::Unthreaded]
            .iter()
            .filter_map(|thread| info.user_receipts.get(thread)?.get(user_id))
            .filter_map(|event_id| info.get_message_key(event_id));
        let sdk = sdk_receipt.and_then(|event_id| info.get_message_key(event_id));

        let newest = local.chain(sdk).chain(pending).max();

        match (newest, sdk_receipt) {
            (Some(key), _) => ReadPoint::Loaded(key.clone()),
            // A receipt on something that's loaded but isn't a message (like a reaction or an
            // edit) has no place in the timeline, so loading more history won't help.
            (None, Some(event_id)) if !info.keys.contains_key(event_id) => {
                ReadPoint::Unloaded(event_id.to_owned())
            },
            (None, _) => ReadPoint::Unknown,
        }
    }

    pub fn has_read(&self, key: &MessageKey) -> bool {
        matches!(self, ReadPoint::Loaded(read) if key <= read)
    }
}

/// Whether a message is something to catch up on.
///
/// This mirrors the events that the SDK counts as unread: other users' messages, stickers,
/// polls and encrypted events, but not state events or redacted messages.
pub fn is_catchup_message(msg: &Message, user_id: &UserId) -> bool {
    msg.sender != user_id &&
        matches!(
            msg.event,
            MessageEvent::Original(..) |
                MessageEvent::EncryptedOriginal(..) |
                MessageEvent::Sticker(..) |
                MessageEvent::Poll(..) |
                MessageEvent::UnstablePoll(..)
        )
}

/// The loaded messages in a room's main timeline that haven't been read.
///
/// When we can't tell where the user stopped reading, the `unread` count reported by the SDK
/// is used to pick the most recent messages instead.
pub fn unread_messages(
    info: &RoomInfo,
    read: &ReadPoint,
    unread: u64,
    user_id: &UserId,
) -> Vec<(MessageKey, OwnedEventId)> {
    let Some(messages) = info.get_thread(None) else {
        return vec![];
    };

    let msgs = messages
        .iter()
        .filter(|(key, _)| !read.has_read(key))
        .filter(|(_, msg)| is_catchup_message(msg, user_id))
        .filter_map(|(key, msg)| Some((key.clone(), msg.event.event_id()?.to_owned())));

    match read {
        ReadPoint::Loaded(_) => msgs.collect(),
        ReadPoint::Unloaded(_) | ReadPoint::Unknown => {
            let mut msgs = msgs.collect::<Vec<_>>();
            let skip = msgs.len().saturating_sub(usize::try_from(unread).unwrap_or(usize::MAX));
            msgs.drain(..skip);
            msgs
        },
    }
}

/// What's saved between sessions about the `:catchup` window.
#[derive(Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
struct SavedFolds {
    #[serde(default)]
    collapsed: BTreeSet<OwnedRoomId>,
}

pub fn load_folds(path: &Path) -> HashSet<OwnedRoomId> {
    let saved = match std::fs::read_to_string(path) {
        Ok(saved) => saved,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return HashSet::new(),
        Err(e) => {
            tracing::warn!(?path, "failed to read collapsed :catchup rooms: {e}");
            return HashSet::new();
        },
    };

    match serde_json::from_str::<SavedFolds>(&saved) {
        Ok(saved) => saved.collapsed.into_iter().collect(),
        Err(e) => {
            tracing::warn!(?path, "failed to parse collapsed :catchup rooms: {e}");
            HashSet::new()
        },
    }
}

pub fn save_folds(path: &Path, collapsed: &HashSet<OwnedRoomId>) {
    let saved = SavedFolds { collapsed: collapsed.iter().cloned().collect() };
    let res = path
        .parent()
        .map(std::fs::create_dir_all)
        .transpose()
        .and_then(|_| std::fs::write(path, serde_json::to_string(&saved)?));

    if let Err(e) = res {
        tracing::warn!(?path, "failed to save collapsed :catchup rooms: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::base::EventLocation;
    use crate::tests::*;
    use matrix_sdk::ruma::{event_id, room_id, user_id};

    fn key(ts: u64, event_id: &OwnedEventId) -> MessageKey {
        let ts = MilliSecondsSinceUnixEpoch(ts.try_into().unwrap());
        MessageKey {
            ts: MessageTimeStamp(ts),
            id: event_id.clone().into(),
        }
    }

    #[test]
    fn test_pending_undo() {
        let mut pending = PendingReads::default();
        let now = Instant::now();
        let room1 = room_id!("!room1:example.com");
        let room2 = room_id!("!room2:example.com");
        let key1 = key(1, &MSG1_EVID);
        let key2 = key(2, &MSG2_EVID);

        pending.add(vec![(room1.to_owned(), MSG1_EVID.clone(), key1.clone())], now);
        pending.add(
            vec![
                (room1.to_owned(), MSG2_EVID.clone(), key2.clone()),
                (room2.to_owned(), MSG3_EVID.clone(), key(3, &MSG3_EVID)),
            ],
            now,
        );

        assert_eq!(pending.read_up_to(room1), Some(&key2));
        assert_eq!(pending.undo(), 2);
        assert_eq!(pending.read_up_to(room1), Some(&key1));
        assert_eq!(pending.read_up_to(room2), None);
        assert_eq!(pending.undo(), 1);
        assert_eq!(pending.undo(), 0);
        assert!(pending.is_empty());
    }

    #[test]
    fn test_pending_take_due() {
        let mut pending = PendingReads::default();
        let now = Instant::now();
        let later = now + Duration::from_secs(5);
        let room = room_id!("!room1:example.com");

        pending.add(vec![(room.to_owned(), MSG1_EVID.clone(), key(1, &MSG1_EVID))], now);
        pending.add(vec![(room.to_owned(), MSG2_EVID.clone(), key(2, &MSG2_EVID))], later);

        let due = pending.take_due(now);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].event_id, *MSG1_EVID);
        assert_eq!(pending.len(), 1);

        assert_eq!(pending.take_all().len(), 1);
        assert!(pending.is_empty());
    }

    #[test]
    fn test_pending_take_newest() {
        let mut pending = PendingReads::default();
        let now = Instant::now();
        let room1 = room_id!("!room1:example.com");
        let room2 = room_id!("!room2:example.com");

        pending.add(vec![(room1.to_owned(), MSG2_EVID.clone(), key(2, &MSG2_EVID))], now);
        pending.add(vec![(room1.to_owned(), MSG1_EVID.clone(), key(1, &MSG1_EVID))], now);
        pending.add(vec![(room2.to_owned(), MSG3_EVID.clone(), key(3, &MSG3_EVID))], now);

        let mut newest = pending
            .take_newest()
            .into_iter()
            .map(|r| (r.room_id, r.event_id))
            .collect::<Vec<_>>();
        newest.sort();

        assert_eq!(newest, vec![
            (room1.to_owned(), MSG2_EVID.clone()),
            (room2.to_owned(), MSG3_EVID.clone()),
        ]);
        assert!(pending.is_empty());
    }

    #[test]
    fn test_unread_messages() {
        let info = mock_room();
        let user_id = user_id!("@someone:example.com");

        let all = unread_messages(&info, &ReadPoint::Unknown, 100, user_id);
        assert!(all.len() > 2);

        // Without a known read point, only the reported number of unread messages are used.
        let last_two = unread_messages(&info, &ReadPoint::Unknown, 2, user_id);
        assert_eq!(last_two, all[all.len() - 2..].to_vec());

        // A loaded read point hides everything before and including it.
        let (first_key, _) = &all[0];
        let read = ReadPoint::Loaded(first_key.clone());
        assert_eq!(unread_messages(&info, &read, 0, user_id), all[1..].to_vec());

        // Our own messages are never unread.
        let sender = info.get_thread(None).unwrap().get(first_key).unwrap().sender.clone();
        let mine = unread_messages(&info, &ReadPoint::Unknown, 100, &sender);
        assert!(mine.iter().all(|(key, _)| key != first_key));
    }

    #[test]
    fn test_read_point() {
        let mut info = mock_room();
        let user_id = user_id!("@someone:example.com");
        let all = unread_messages(&info, &ReadPoint::Unknown, 100, user_id);
        let (key1, ev1) = all[0].clone();
        let (key2, ev2) = all[1].clone();
        let unloaded = event_id!("$unloaded:example.com");

        assert_eq!(ReadPoint::new(&info, None, None, user_id), ReadPoint::Unknown);
        assert_eq!(
            ReadPoint::new(&info, Some(unloaded), None, user_id),
            ReadPoint::Unloaded(unloaded.to_owned())
        );
        assert_eq!(
            ReadPoint::new(&info, Some(&ev1), None, user_id),
            ReadPoint::Loaded(key1.clone())
        );

        // Pending and local receipts win when they're newer.
        assert_eq!(
            ReadPoint::new(&info, Some(&ev1), Some(&key2), user_id),
            ReadPoint::Loaded(key2.clone())
        );
        // A receipt on a loaded event that isn't a message, like a reaction, isn't worth loading
        // more history for.
        let reaction = event_id!("$reaction:example.com");
        info.keys.insert(reaction.to_owned(), EventLocation::Reaction(ev1.clone()));
        assert_eq!(ReadPoint::new(&info, Some(reaction), None, user_id), ReadPoint::Unknown);

        info.set_receipt(ReceiptThread::Main, user_id.to_owned(), ev2.clone());
        assert_eq!(
            ReadPoint::new(&info, Some(unloaded), None, user_id),
            ReadPoint::Loaded(key2.clone())
        );
    }

    #[test]
    fn test_saved_folds() {
        let dir = std::env::temp_dir().join(format!("iamb-catchup-test-{}", std::process::id()));
        let path = dir.join("nested").join("catchup.json");
        let a = OwnedRoomId::try_from("!a:example.com").unwrap();
        let b = OwnedRoomId::try_from("!b:example.com").unwrap();

        // Nothing saved yet.
        assert!(load_folds(&path).is_empty());

        let collapsed = HashSet::from([a.clone(), b.clone()]);
        save_folds(&path, &collapsed);
        assert_eq!(load_folds(&path), collapsed);

        save_folds(&path, &HashSet::from([a.clone()]));
        assert_eq!(load_folds(&path), HashSet::from([a]));

        // A broken file is ignored rather than failing.
        std::fs::write(&path, "not json").unwrap();
        assert!(load_folds(&path).is_empty());

        let _ = std::fs::remove_dir_all(dir);
    }
}
