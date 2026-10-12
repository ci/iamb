//! The `:catchup` window, which shows unread messages from every room.
//!
//! Messages stay in the window after being marked as read (dimmed) until it's refreshed, so that
//! the cursor doesn't jump around while reading through them.
use std::collections::{BTreeMap, HashMap};

use modalkit::actions::{EditAction, EditorActions};
use modalkit_ratatui::list::{ListCursor, ListItem, ListState};

use crate::base::CatchUpScope;
use crate::catchup::{ReadPoint, unread_messages};
use crate::prelude::*;

/// How long to wait between requests to load older messages for a room.
const LOAD_DEBOUNCE: Duration = Duration::from_secs(2);

pub type CatchUpListState = ListState<CatchUpItem, IambInfo>;

/// The room heading shown above its messages.
#[derive(Clone, Debug, PartialEq)]
pub struct CatchUpSection {
    room_id: OwnedRoomId,
    title: String,
}

impl From<CatchUpSection> for Line<'static> {
    fn from(section: CatchUpSection) -> Self {
        Line::styled(section.title, Style::default().add_modifier(StyleModifier::BOLD))
    }
}

#[derive(Clone)]
pub struct CatchUpItem {
    room_id: OwnedRoomId,
    event_id: OwnedEventId,
    key: MessageKey,
    /// The message shown before this one in the same room.
    prev: Option<OwnedEventId>,
    read: bool,
    section: CatchUpSection,
}

impl CatchUpItem {
    pub fn room_id(&self) -> &RoomId {
        &self.room_id
    }
}

impl Display for CatchUpItem {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.event_id)
    }
}

impl ListItem<IambInfo> for CatchUpItem {
    type Section = CatchUpSection;

    fn show<'a>(
        &'a self,
        selected: bool,
        vwctx: &ViewportContext<ListCursor>,
        store: &'a ProgramStore,
    ) -> Text<'a> {
        let settings = &store.application.settings;
        let style = settings.theme.default;

        let Some(info) = store.application.rooms.get(&self.room_id) else {
            return Text::styled("Unable to load room information", style);
        };

        let Some(msg) = info.get_event(&self.event_id) else {
            return Span::styled("Message is no longer available", style.fg(Color::Gray)).into();
        };

        let prev = self.prev.as_ref().and_then(|event_id| info.get_event(event_id));
        let previews = &store.application.previews;
        let text = msg.show(prev, selected, vwctx.get_width(), info, settings, previews);

        if self.read {
            text.patch_style(Style::default().add_modifier(StyleModifier::DIM))
        } else {
            text
        }
    }

    fn get_word(&self) -> Option<String> {
        self.event_id.to_string().into()
    }

    fn get_section(&self) -> Option<&Self::Section> {
        Some(&self.section)
    }
}

impl Promptable<ProgramContext, ProgramStore, IambInfo> for CatchUpItem {
    fn prompt(
        &mut self,
        act: &PromptAction,
        ctx: &ProgramContext,
        _: &mut ProgramStore,
    ) -> EditResult<Vec<(ProgramAction, ProgramContext)>, IambInfo> {
        match act {
            PromptAction::Submit => {
                let room = IambId::Room(self.room_id.clone().into(), RoomView::Main);
                let open = WindowAction::Switch(OpenTarget::Application(room));
                let jump = IambAction::from(TimelineAction::GotoEvent(self.event_id.clone()));

                Ok(vec![(open.into(), ctx.clone()), (jump.into(), ctx.clone())])
            },
            PromptAction::Abort(_) => {
                let msg = "Cannot abort entry inside a list";
                let err = EditError::Failure(msg.into());

                Err(err)
            },
            PromptAction::Recall(..) => {
                let msg = "Cannot recall history inside a list";
                let err = EditError::Failure(msg.into());

                Err(err)
            },
        }
    }
}

/// The rooms and messages shown in the `:catchup` window.
#[derive(Default)]
pub struct CatchUpFeed {
    /// Rooms in the order they're shown.
    order: Vec<OwnedRoomId>,

    /// Every unread message seen since the last refresh, including those read since.
    seen: HashMap<OwnedRoomId, BTreeMap<MessageKey, OwnedEventId>>,

    /// When we last asked to load older messages for each room.
    loads: HashMap<OwnedRoomId, Instant>,
}

impl CatchUpFeed {
    /// Forget everything that's been read, and recompute the room order.
    pub fn refresh(&mut self) {
        self.order.clear();
        self.seen.clear();
    }

    /// Add any rooms that have newly become unread, most important first.
    fn add_rooms(&mut self, store: &ChatStore) {
        let mut rooms = store
            .sync_info
            .rooms
            .iter()
            .chain(store.sync_info.dms.iter())
            .filter(|room| !self.seen.contains_key(room.room_id()))
            .filter(|room| room.num_unread_messages() > 0)
            .filter(|room| {
                store
                    .rooms
                    .get(room.room_id())
                    .is_none_or(|info| !info.flags.contains(RoomInfoFlags::MUTED))
            })
            .map(|room| {
                let latest = store
                    .rooms
                    .get(room.room_id())
                    .and_then(|info| info.get_thread(None))
                    .and_then(|msgs| msgs.last_key_value())
                    .map(|(key, _)| key.ts);

                (room.room_id().to_owned(), room.num_unread_mentions() > 0, latest)
            })
            .collect::<Vec<_>>();

        // Rooms with mentions first, then the most recently active.
        rooms.sort_by(|a, b| b.1.cmp(&a.1).then(b.2.cmp(&a.2)));

        for (room_id, _, _) in rooms {
            self.seen.insert(room_id.clone(), BTreeMap::new());
            self.order.push(room_id);
        }
    }

    /// Build the items to show, picking up any newly arrived messages.
    pub fn items(&mut self, store: &mut ChatStore) -> Vec<CatchUpItem> {
        self.add_rooms(store);

        let ChatStore {
            rooms, worker, settings, pending_reads, need_load, ..
        } = store;
        let user_id = &settings.profile.user_id;
        let mut items = vec![];

        for room_id in self.order.iter() {
            let Some(room) = worker.client.get_room(room_id) else {
                continue;
            };

            let info = rooms.get_or_default(room_id.clone());
            let receipts = room.read_receipts();
            let sdk_receipt = receipts.latest_active.as_ref().map(|r| r.event_id.as_ref());
            let read =
                ReadPoint::new(info, sdk_receipt, pending_reads.read_up_to(room_id), user_id);

            // Keep loading older messages until we find where the user stopped reading.
            if let ReadPoint::Unloaded(event_id) = &read &&
                !info.reached_timeline_start &&
                self.loads.get(room_id).is_none_or(|t| t.elapsed() >= LOAD_DEBOUNCE)
            {
                need_load.need_message(room_id.clone(), event_id.clone());
                self.loads.insert(room_id.clone(), Instant::now());
            }

            let seen = self.seen.entry(room_id.clone()).or_default();
            seen.extend(unread_messages(info, &read, room.num_unread_messages(), user_id));

            let title = info.name.clone().unwrap_or_else(|| "Untitled Matrix Room".into());
            let section = CatchUpSection { room_id: room_id.clone(), title };
            let mut prev = None;

            for (key, event_id) in seen.iter() {
                items.push(CatchUpItem {
                    room_id: room_id.clone(),
                    event_id: event_id.clone(),
                    key: key.clone(),
                    prev: prev.replace(event_id.clone()),
                    read: read.has_read(key),
                    section: section.clone(),
                });
            }
        }

        items
    }

    /// Number of messages that haven't been read yet.
    pub fn unread(state: &CatchUpListState) -> usize {
        state.get_items().iter().filter(|item| !item.read).count()
    }
}

fn selected_index(state: &CatchUpListState) -> Option<usize> {
    let selected = state.get()?;
    state
        .get_items()
        .iter()
        .position(|item| item.event_id == selected.event_id)
}

/// Move the cursor to the item at `idx`.
fn select(state: &mut CatchUpListState, idx: usize, store: &mut ProgramStore) {
    let count = Count::Exact(idx.saturating_add(1));
    let target = EditTarget::Motion(MoveType::BufferLineOffset, count);
    let _ = state.edit(&EditAction::Motion, &target, &ProgramContext::default(), store);
}

/// Replace the window's items, keeping the cursor on the same message.
pub fn update(state: &mut CatchUpListState, feed: &mut CatchUpFeed, store: &mut ProgramStore) {
    let selected = state.get().map(|item| item.event_id.clone());
    let items = feed.items(&mut store.application);
    let idx = selected.and_then(|sel| items.iter().position(|item| item.event_id == sel));

    state.set(items);

    if let Some(idx) = idx &&
        selected_index(state) != Some(idx)
    {
        select(state, idx, store);
    }
}

/// Mark messages in the window as read, once the undo delay passes.
pub fn mark_read(
    state: &mut CatchUpListState,
    scope: CatchUpScope,
    store: &mut ProgramStore,
) -> IambResult<EditInfo> {
    let Some(sel_idx) = selected_index(state) else {
        return Ok(Some("Nothing to mark as read".into()));
    };

    let items = state.get_items();
    let sel_room = items[sel_idx].room_id.clone();
    let in_scope = |idx: usize, item: &CatchUpItem| {
        match scope {
            CatchUpScope::Selected => item.room_id == sel_room && idx <= sel_idx,
            CatchUpScope::Room => item.room_id == sel_room,
            CatchUpScope::Above => idx <= sel_idx,
            CatchUpScope::All => true,
        }
    };

    // The newest unread message in scope for each room.
    let mut targets: HashMap<OwnedRoomId, (MessageKey, OwnedEventId)> = HashMap::new();
    let mut count = 0;

    for (idx, item) in items.iter().enumerate() {
        if item.read || !in_scope(idx, item) {
            continue;
        }

        count += 1;
        let target = (item.key.clone(), item.event_id.clone());
        targets
            .entry(item.room_id.clone())
            .and_modify(|t| *t = target.clone().max(t.clone()))
            .or_insert(target);
    }

    if targets.is_empty() {
        return Ok(Some("Already marked as read".into()));
    }

    // Move on to the next unread message.
    let next = items
        .iter()
        .enumerate()
        .skip(sel_idx + 1)
        .find(|(idx, item)| !item.read && !in_scope(*idx, item))
        .map(|(idx, _)| idx);

    let rooms = targets.len();
    let reads = targets
        .into_iter()
        .map(|(room_id, (key, event_id))| (room_id, event_id, key))
        .collect();

    let delay = store.application.settings.tunables.catchup_undo_delay;
    let due = Instant::now() + Duration::from_secs(delay);
    store.application.pending_reads.add(reads, due);

    if let Some(next) = next {
        select(state, next, store);
    }

    let messages = if count == 1 { "message" } else { "messages" };
    let in_rooms = if rooms == 1 {
        String::new()
    } else {
        format!(" in {rooms} rooms")
    };
    let msg = format!(
        "Marking {count} {messages}{in_rooms} as read in {delay}s (:catchup undo to cancel)"
    );

    Ok(Some(msg.into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::*;

    fn item(room: &str, event: &OwnedEventId, ts: u64, read: bool) -> CatchUpItem {
        let room_id = OwnedRoomId::try_from(room).unwrap();
        let ts = MilliSecondsSinceUnixEpoch(ts.try_into().unwrap());
        let key = MessageKey { ts: MessageTimeStamp(ts), id: event.clone().into() };
        let section = CatchUpSection { room_id: room_id.clone(), title: room.into() };

        CatchUpItem {
            room_id,
            event_id: event.clone(),
            key,
            prev: None,
            read,
            section,
        }
    }

    fn mock_state(read_first: bool) -> CatchUpListState {
        CatchUpListState::new(IambBufferId::CatchUp, vec![
            item("!a:example.com", &MSG1_EVID, 1, read_first),
            item("!a:example.com", &MSG2_EVID, 2, false),
            item("!a:example.com", &MSG3_EVID, 3, false),
            item("!b:example.com", &MSG4_EVID, 4, false),
            item("!b:example.com", &MSG5_EVID, 5, false),
        ])
    }

    fn pending(store: &mut ProgramStore) -> Vec<(String, OwnedEventId)> {
        let mut reads = store
            .application
            .pending_reads
            .take_all()
            .into_iter()
            .map(|r| (r.room_id.to_string(), r.event_id))
            .collect::<Vec<_>>();
        reads.sort();
        reads
    }

    #[tokio::test]
    async fn test_mark_read_scopes() {
        let mut store = mock_store().await;

        // Up to the selected message, only in its room.
        let mut state = mock_state(false);
        select(&mut state, 1, &mut store);
        mark_read(&mut state, CatchUpScope::Selected, &mut store).unwrap();
        assert_eq!(pending(&mut store), vec![("!a:example.com".into(), MSG2_EVID.clone())]);
        // The cursor moves to the next unread message.
        assert_eq!(selected_index(&state), Some(2));

        // The whole room, even below the cursor.
        let mut state = mock_state(false);
        mark_read(&mut state, CatchUpScope::Room, &mut store).unwrap();
        assert_eq!(pending(&mut store), vec![("!a:example.com".into(), MSG3_EVID.clone())]);
        assert_eq!(selected_index(&state), Some(3));

        // Everything above, across rooms.
        let mut state = mock_state(false);
        select(&mut state, 3, &mut store);
        mark_read(&mut state, CatchUpScope::Above, &mut store).unwrap();
        assert_eq!(pending(&mut store), vec![
            ("!a:example.com".into(), MSG3_EVID.clone()),
            ("!b:example.com".into(), MSG4_EVID.clone()),
        ]);
        assert_eq!(selected_index(&state), Some(4));

        // Everything.
        let mut state = mock_state(false);
        mark_read(&mut state, CatchUpScope::All, &mut store).unwrap();
        assert_eq!(pending(&mut store), vec![
            ("!a:example.com".into(), MSG3_EVID.clone()),
            ("!b:example.com".into(), MSG5_EVID.clone()),
        ]);
    }

    #[tokio::test]
    async fn test_mark_read_skips_read() {
        let mut store = mock_store().await;

        let mut state = mock_state(true);
        let res = mark_read(&mut state, CatchUpScope::Selected, &mut store).unwrap();
        assert_eq!(res.unwrap().to_string(), "Already marked as read");
        assert!(store.application.pending_reads.is_empty());
    }
}
