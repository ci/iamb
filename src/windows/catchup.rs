//! The `:catchup` window, which shows unread messages from every room.
//!
//! Messages stay in the window after being marked as read (dimmed) until it's refreshed, so that
//! the cursor doesn't jump around while reading through them.
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use modalkit::actions::{EditAction, EditorActions};
use modalkit_ratatui::list::{ListCursor, ListItem, ListState};
use serde::{Deserialize, Serialize};

use crate::base::CatchUpScope;
use crate::catchup::{ReadPoint, unread_messages};
use crate::prelude::*;

/// How long to wait between requests to load older messages for a room.
const LOAD_DEBOUNCE: Duration = Duration::from_secs(2);

/// How many times to load older messages for a room while looking for its read point.
const LOAD_ATTEMPTS: u8 = 30;

pub type CatchUpListState = ListState<CatchUpItem, IambInfo>;

/// The room heading shown above its messages.
#[derive(Clone, Debug, PartialEq)]
pub struct CatchUpSection {
    room_id: OwnedRoomId,
    title: String,
    unread: usize,
}

impl CatchUpSection {
    fn line(&self, marker: char, style: Style) -> Line<'static> {
        let heading = style.fg(Color::Cyan).add_modifier(StyleModifier::BOLD);
        let summary = match self.unread {
            0 => " · read".to_string(),
            n => format!(" · {n} unread"),
        };

        Line::from(vec![
            Span::styled(format!("{marker} {}", self.title), heading),
            Span::styled(summary, style.fg(Color::Gray)),
        ])
    }
}

impl From<CatchUpSection> for Line<'static> {
    fn from(section: CatchUpSection) -> Self {
        section.line('▾', Style::default())
    }
}

#[derive(Clone)]
pub struct CatchUpItem {
    room_id: OwnedRoomId,
    /// The message, or for a collapsed room, its newest message.
    event_id: OwnedEventId,
    key: MessageKey,
    /// The message shown before this one in the same room.
    prev: Option<OwnedEventId>,
    read: bool,
    /// How many unread messages this item stands for.
    unread: usize,
    /// Whether this item stands in for a whole collapsed room.
    collapsed: bool,
    /// Whether this is the last item for its room, to leave space before the next one.
    last: bool,
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
        let mut text = self.show_item(selected, vwctx, store);

        if self.last {
            text.push_line(Line::default());
        }

        text
    }

    fn get_word(&self) -> Option<String> {
        self.event_id.to_string().into()
    }

    fn get_section(&self) -> Option<&Self::Section> {
        // A collapsed room's row already names the room.
        (!self.collapsed).then_some(&self.section)
    }
}

impl CatchUpItem {
    fn show_item<'a>(
        &'a self,
        selected: bool,
        vwctx: &ViewportContext<ListCursor>,
        store: &'a ProgramStore,
    ) -> Text<'a> {
        let settings = &store.application.settings;
        let style = settings.theme.default;

        if self.collapsed {
            let style = if selected {
                style.add_modifier(StyleModifier::REVERSED)
            } else {
                style
            };
            return self.section.line('▸', style).into();
        }

        let Some(info) = store.application.rooms.get(&self.room_id) else {
            return Text::styled("Unable to load room information", style);
        };

        let Some(msg) = info.get_event(&self.event_id) else {
            return Span::styled("Message is no longer available", style.fg(Color::Gray)).into();
        };

        let prev = self.prev.as_ref().and_then(|event_id| info.get_event(event_id));
        // Lists can't draw images, so show attachments as text; they show in the room itself.
        let previews = PreviewManager::text_only();
        let text = msg.show(prev, selected, vwctx.get_width(), info, settings, previews);

        if self.read {
            text.patch_style(Style::default().add_modifier(StyleModifier::DIM))
        } else {
            text
        }
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
            PromptAction::Submit if self.collapsed => {
                let expand = IambAction::from(CatchUpAction::Toggle);
                Ok(vec![(expand.into(), ctx.clone())])
            },
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

/// What's saved between sessions about the `:catchup` window.
#[derive(Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
struct SavedFolds {
    #[serde(default)]
    collapsed: BTreeSet<OwnedRoomId>,
}

fn load_folds(path: &Path) -> HashSet<OwnedRoomId> {
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

fn save_folds(path: &Path, collapsed: &HashSet<OwnedRoomId>) {
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

/// The rooms and messages shown in the `:catchup` window.
#[derive(Default)]
pub struct CatchUpFeed {
    /// Rooms in the order they're shown.
    order: Vec<OwnedRoomId>,

    /// Every unread message seen since the last refresh, including those read since.
    seen: HashMap<OwnedRoomId, BTreeMap<MessageKey, OwnedEventId>>,

    /// When we last asked to load older messages for each room.
    loads: HashMap<OwnedRoomId, (Instant, u8)>,

    /// Rooms shown as a single row.
    collapsed: HashSet<OwnedRoomId>,
}

impl CatchUpFeed {
    /// Forget everything that's been read, and recompute the room order.
    /// A feed with the rooms that were collapsed last time.
    pub fn new(settings: &ApplicationSettings) -> Self {
        Self {
            collapsed: load_folds(&settings.catchup_json),
            ..Default::default()
        }
    }

    /// A new feed for another window, with the same rooms collapsed.
    pub fn dup(&self) -> Self {
        Self {
            collapsed: self.collapsed.clone(),
            ..Default::default()
        }
    }

    /// Forget everything that's been read, and recompute the room order. Collapsed rooms stay
    /// collapsed.
    pub fn refresh(&mut self) {
        self.order.clear();
        self.seen.clear();
    }

    /// Collapse or expand rooms.
    fn fold(&mut self, act: &CatchUpAction, room_id: Option<&RoomId>) {
        match act {
            CatchUpAction::Toggle => {
                if let Some(room_id) = room_id &&
                    !self.collapsed.remove(room_id)
                {
                    self.collapsed.insert(room_id.to_owned());
                }
            },
            CatchUpAction::CollapseAll => self.collapsed.extend(self.order.iter().cloned()),
            CatchUpAction::ExpandAll => self.collapsed.clear(),
            CatchUpAction::ToggleAll => {
                if self.order.iter().all(|room_id| self.collapsed.contains(room_id)) {
                    self.collapsed.clear();
                } else {
                    self.collapsed.extend(self.order.iter().cloned());
                }
            },
            _ => {},
        }
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
        // Forget rooms that have since been left.
        let client = &store.worker.client;
        let joined = |room_id: &OwnedRoomId| {
            client
                .get_room(room_id)
                .is_some_and(|r| r.state() == MatrixRoomState::Joined)
        };
        self.order.retain(joined);
        self.seen.retain(|room_id, _| joined(room_id));

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

            let unread_count = room.num_unread_messages();
            let unread = unread_messages(info, &read, unread_count, user_id);

            // Load older messages until we find where the user stopped reading, or at least
            // until there are as many as the SDK says are unread.
            let load = self.loads.get(room_id);
            let attempts = load.map(|(_, n)| *n).unwrap_or_default();
            if let ReadPoint::Unloaded(event_id) = &read &&
                !info.reached_timeline_start &&
                (unread.len() as u64) < unread_count &&
                attempts < LOAD_ATTEMPTS &&
                load.is_none_or(|(t, _)| t.elapsed() >= LOAD_DEBOUNCE)
            {
                need_load.need_message(room_id.clone(), event_id.clone());
                self.loads.insert(room_id.clone(), (Instant::now(), attempts + 1));
            }

            let seen = self.seen.entry(room_id.clone()).or_default();
            seen.extend(unread);

            let title = info.name.clone().unwrap_or_else(|| "Untitled Matrix Room".into());
            let unread = seen.keys().filter(|key| !read.has_read(key)).count();
            let section = CatchUpSection { room_id: room_id.clone(), title, unread };

            if self.collapsed.contains(room_id) {
                items.extend(collapsed_item(seen, &read, section));
                continue;
            }

            let mut prev = None;

            for (key, event_id) in seen.iter() {
                let is_read = read.has_read(key);
                items.push(CatchUpItem {
                    room_id: room_id.clone(),
                    event_id: event_id.clone(),
                    key: key.clone(),
                    prev: prev.replace(event_id.clone()),
                    read: is_read,
                    unread: usize::from(!is_read),
                    collapsed: false,
                    last: false,
                    section: section.clone(),
                });
            }

            if let Some(item) = items.last_mut() &&
                item.room_id == *room_id
            {
                item.last = true;
            }
        }

        items
    }

    /// Number of messages that haven't been read yet.
    pub fn unread(state: &CatchUpListState) -> usize {
        state.get_items().iter().map(|item| item.unread).sum()
    }
}

/// The single row standing in for a collapsed room, pointing at its newest message.
fn collapsed_item(
    seen: &BTreeMap<MessageKey, OwnedEventId>,
    read: &ReadPoint,
    section: CatchUpSection,
) -> Option<CatchUpItem> {
    let (key, event_id) = seen.last_key_value()?;
    let unread = seen.keys().filter(|key| !read.has_read(key)).count();

    Some(CatchUpItem {
        room_id: section.room_id.clone(),
        event_id: event_id.clone(),
        key: key.clone(),
        prev: None,
        read: unread == 0,
        unread,
        collapsed: true,
        last: false,
        section,
    })
}

/// Where to put the cursor in a room after folding it: on its collapsed row, or else on its
/// first unread message.
fn fold_cursor(items: &[CatchUpItem], room_id: &RoomId) -> Option<usize> {
    let in_room = || items.iter().enumerate().filter(|(_, item)| item.room_id == room_id);

    in_room()
        .find(|(_, item)| item.collapsed || !item.read)
        .or_else(|| in_room().next())
        .map(|(idx, _)| idx)
}

/// Where moving `count` rooms forward or back from `idx` lands: the first item of that room.
///
/// Moving back first goes to the start of the current room, like Vim's `[[`.
fn room_jump(items: &[CatchUpItem], idx: usize, dir: MoveDir1D, count: usize) -> Option<usize> {
    let starts = (0..items.len())
        .filter(|&i| i == 0 || items[i].room_id != items[i - 1].room_id)
        .collect::<Vec<_>>();

    match dir {
        MoveDir1D::Next => {
            let next = starts.iter().filter(|&&start| start > idx).nth(count.saturating_sub(1));
            next.or_else(|| starts.last()).copied()
        },
        MoveDir1D::Previous => {
            let prev = starts
                .iter()
                .rev()
                .filter(|&&start| start < idx)
                .nth(count.saturating_sub(1));
            prev.or_else(|| starts.first()).copied()
        },
    }
}

/// Move the cursor to the start of the next or previous room.
pub fn jump_room(
    state: &mut CatchUpListState,
    dir: MoveDir1D,
    count: usize,
    store: &mut ProgramStore,
) -> EditResult<EditInfo, IambInfo> {
    let idx = selected_index(state).unwrap_or_default();

    if let Some(target) = room_jump(state.get_items(), idx, dir, count) {
        select(state, target, store);
    }

    Ok(None)
}

/// Collapse or expand rooms, keeping the cursor on the room it was in.
pub fn fold(
    state: &mut CatchUpListState,
    feed: &mut CatchUpFeed,
    act: &CatchUpAction,
    store: &mut ProgramStore,
) -> IambResult<EditInfo> {
    let room_id = state.get().map(|item| item.room_id.clone());
    feed.fold(act, room_id.as_deref());
    save_folds(&store.application.settings.catchup_json, &feed.collapsed);

    let items = feed.items(&mut store.application);
    let idx = room_id.and_then(|room_id| fold_cursor(&items, &room_id));

    state.set(items);

    if let Some(idx) = idx {
        select(state, idx, store);
    }

    Ok(None)
}

/// Whether two items are the same row: the same message, or the same collapsed room, whose row
/// points at a different message whenever the room gets a new one.
fn same_row(a: &CatchUpItem, b: &CatchUpItem) -> bool {
    if a.collapsed || b.collapsed {
        a.collapsed && b.collapsed && a.room_id == b.room_id
    } else {
        a.event_id == b.event_id
    }
}

fn selected_index(state: &CatchUpListState) -> Option<usize> {
    let selected = state.get()?;
    state.get_items().iter().position(|item| same_row(item, selected))
}

/// Move the cursor to the item at `idx`.
fn select(state: &mut CatchUpListState, idx: usize, store: &mut ProgramStore) {
    let count = Count::Exact(idx.saturating_add(1));
    let target = EditTarget::Motion(MoveType::BufferLineOffset, count);
    let _ = state.edit(&EditAction::Motion, &target, &ProgramContext::default(), store);
}

/// Replace the window's items, keeping the cursor on the same message.
pub fn update(state: &mut CatchUpListState, feed: &mut CatchUpFeed, store: &mut ProgramStore) {
    let selected = state.get().cloned();
    let items = feed.items(&mut store.application);
    let idx = selected.and_then(|sel| items.iter().position(|item| same_row(item, &sel)));

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

        count += item.unread;
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
    use matrix_sdk::ruma::room_id;

    fn item(room: &str, event: &OwnedEventId, ts: u64, read: bool) -> CatchUpItem {
        let room_id = OwnedRoomId::try_from(room).unwrap();
        let ts = MilliSecondsSinceUnixEpoch(ts.try_into().unwrap());
        let key = MessageKey { ts: MessageTimeStamp(ts), id: event.clone().into() };
        let section = CatchUpSection {
            room_id: room_id.clone(),
            title: room.into(),
            unread: 0,
        };

        CatchUpItem {
            room_id,
            event_id: event.clone(),
            key,
            prev: None,
            read,
            unread: usize::from(!read),
            collapsed: false,
            last: false,
            section,
        }
    }

    fn key(ts: u64, event: &OwnedEventId) -> MessageKey {
        let ts = MilliSecondsSinceUnixEpoch(ts.try_into().unwrap());
        MessageKey { ts: MessageTimeStamp(ts), id: event.clone().into() }
    }

    fn collapsed_room(room: &str, read_up_to: Option<u64>) -> CatchUpItem {
        let room_id = OwnedRoomId::try_from(room).unwrap();
        let section = CatchUpSection { room_id, title: room.into(), unread: 0 };
        let seen = BTreeMap::from([
            (key(1, &MSG1_EVID), MSG1_EVID.clone()),
            (key(2, &MSG2_EVID), MSG2_EVID.clone()),
            (key(3, &MSG3_EVID), MSG3_EVID.clone()),
        ]);
        let read = match read_up_to {
            Some(ts) => {
                let read = seen.keys().find(|key| u64::from(key.ts.0.get()) == ts).unwrap();
                ReadPoint::Loaded(read.clone())
            },
            None => ReadPoint::Unknown,
        };

        collapsed_item(&seen, &read, section).unwrap()
    }

    #[test]
    fn test_collapsed_item() {
        let row = collapsed_room("!a:example.com", Some(1));
        assert!(row.collapsed);
        assert_eq!(row.unread, 2);
        assert!(!row.read);
        // It points at the newest message, so marking it read covers the whole room.
        assert_eq!(row.event_id, *MSG3_EVID);
        assert!(row.get_section().is_none());

        let row = collapsed_room("!a:example.com", Some(3));
        assert_eq!(row.unread, 0);
        assert!(row.read);

        assert!(collapsed_item(&BTreeMap::new(), &ReadPoint::Unknown, row.section).is_none());
    }

    #[tokio::test]
    async fn test_mark_read_collapsed() {
        let mut store = mock_store().await;
        let mut state = CatchUpListState::new(IambBufferId::CatchUp, vec![
            collapsed_room("!a:example.com", None),
            item("!b:example.com", &MSG4_EVID, 4, false),
        ]);
        assert_eq!(CatchUpFeed::unread(&state), 4);

        let res = mark_read(&mut state, CatchUpScope::Selected, &mut store).unwrap();
        assert!(res.unwrap().to_string().starts_with("Marking 3 messages as read"));
        assert_eq!(pending(&mut store), vec![("!a:example.com".into(), MSG3_EVID.clone())]);
        assert_eq!(selected_index(&state), Some(1));
    }

    #[test]
    fn test_section_line() {
        let room_id = OwnedRoomId::try_from("!a:example.com").unwrap();
        let mut section = CatchUpSection { room_id, title: "general".into(), unread: 3 };
        let text =
            |line: Line| line.spans.iter().map(|s| s.content.to_string()).collect::<String>();

        assert_eq!(text(section.clone().into()), "▾ general · 3 unread");
        assert_eq!(text(section.line('▸', Style::default())), "▸ general · 3 unread");

        section.unread = 0;
        assert_eq!(text(section.into()), "▾ general · read");

        // Lists show attachments as text, without image previews.
        let source = MediaSource::Plain("mxc://example.com/image".into());
        assert!(PreviewManager::text_only().get(&source, PreviewKind::Message).is_none());
    }

    #[test]
    fn test_same_row() {
        let a1 = collapsed_room("!a:example.com", None);
        let mut a2 = collapsed_room("!a:example.com", None);
        // The room got a newer message since.
        a2.event_id = MSG4_EVID.clone();
        let b = collapsed_room("!b:example.com", None);
        let msg = item("!a:example.com", &MSG3_EVID, 3, false);

        assert!(same_row(&a1, &a2));
        assert!(!same_row(&a1, &b));
        // A collapsed room isn't the same row as its newest message once expanded.
        assert!(!same_row(&a1, &msg));
        assert!(same_row(&msg, &msg.clone()));

        // The cursor stays on the collapsed room when rows above it shift.
        let mut state = CatchUpListState::new(IambBufferId::CatchUp, vec![
            item("!b:example.com", &MSG1_EVID, 1, false),
            a1,
        ]);
        state.set(vec![
            item("!b:example.com", &MSG1_EVID, 1, false),
            item("!b:example.com", &MSG2_EVID, 2, false),
            a2.clone(),
        ]);
        let selected = state.get_items()[2].clone();
        assert!(state.get_items().iter().position(|i| same_row(i, &selected)) == Some(2));
    }

    #[test]
    fn test_fold_cursor() {
        let collapsed = vec![
            item("!a:example.com", &MSG1_EVID, 1, false),
            collapsed_room("!b:example.com", None),
        ];
        let b = room_id!("!b:example.com");
        assert_eq!(fold_cursor(&collapsed, b), Some(1));

        // Expanded, the cursor goes to the room's first unread message.
        let expanded = vec![
            item("!a:example.com", &MSG1_EVID, 1, false),
            item("!b:example.com", &MSG2_EVID, 2, true),
            item("!b:example.com", &MSG3_EVID, 3, false),
        ];
        assert_eq!(fold_cursor(&expanded, b), Some(2));

        // With everything read, it goes to the room's first message.
        let read = vec![item("!b:example.com", &MSG2_EVID, 2, true)];
        assert_eq!(fold_cursor(&read, b), Some(0));
        assert_eq!(fold_cursor(&read, room_id!("!c:example.com")), None);
    }

    #[tokio::test]
    async fn test_room_motion_keys() {
        use crate::keybindings::setup_keybindings;
        use crate::windows::IambWindow;
        use modalkit::keybindings::BindingMachine;

        let mut store = mock_store().await;
        let mut win = IambWindow::CatchUp(mock_state(false), CatchUpFeed::default());
        let mut bindings = setup_keybindings(&store.application.settings.tunables);

        let mut press = |keys: &[&str], win: &mut IambWindow| {
            for key in keys {
                bindings.input_key(key.parse().unwrap());
            }
            while let Some((act, ctx)) = bindings.pop() {
                if let Action::Editor(act) = act {
                    win.editor_command(&act, &ctx, &mut store).unwrap();
                }
            }
            let IambWindow::CatchUp(state, _) = win else {
                unreachable!()
            };
            selected_index(state)
        };

        // The first room has three messages, so the second starts at the fourth item.
        assert_eq!(press(&["]", "]"], &mut win), Some(3));
        assert_eq!(press(&["j"], &mut win), Some(4));
        assert_eq!(press(&["[", "["], &mut win), Some(3));
        assert_eq!(press(&["{"], &mut win), Some(0));
        assert_eq!(press(&["}"], &mut win), Some(3));
    }

    #[test]
    fn test_room_jump() {
        let items = vec![
            item("!a:example.com", &MSG1_EVID, 1, false),
            item("!a:example.com", &MSG2_EVID, 2, false),
            collapsed_room("!b:example.com", None),
            item("!c:example.com", &MSG3_EVID, 3, false),
            item("!c:example.com", &MSG4_EVID, 4, false),
        ];
        let next = MoveDir1D::Next;
        let prev = MoveDir1D::Previous;

        assert_eq!(room_jump(&items, 0, next, 1), Some(2));
        assert_eq!(room_jump(&items, 1, next, 1), Some(2));
        assert_eq!(room_jump(&items, 0, next, 2), Some(3));
        // Going past the last room stops at it.
        assert_eq!(room_jump(&items, 3, next, 1), Some(3));
        assert_eq!(room_jump(&items, 0, next, 9), Some(3));

        // Back from inside a room goes to its start first.
        assert_eq!(room_jump(&items, 4, prev, 1), Some(3));
        assert_eq!(room_jump(&items, 3, prev, 1), Some(2));
        assert_eq!(room_jump(&items, 4, prev, 2), Some(2));
        assert_eq!(room_jump(&items, 1, prev, 5), Some(0));
        assert_eq!(room_jump(&[], 0, next, 1), None);
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

    #[test]
    fn test_fold() {
        let mut feed = CatchUpFeed::default();
        let a = OwnedRoomId::try_from("!a:example.com").unwrap();
        let b = OwnedRoomId::try_from("!b:example.com").unwrap();
        feed.order = vec![a.clone(), b.clone()];

        feed.fold(&CatchUpAction::Toggle, Some(&a));
        assert_eq!(feed.collapsed, HashSet::from([a.clone()]));
        feed.fold(&CatchUpAction::Toggle, Some(&a));
        assert!(feed.collapsed.is_empty());

        feed.fold(&CatchUpAction::CollapseAll, None);
        assert_eq!(feed.collapsed, HashSet::from([a, b]));
        feed.fold(&CatchUpAction::ExpandAll, None);
        assert!(feed.collapsed.is_empty());

        // Toggling all collapses everything unless it's all collapsed already.
        let first = feed.order[0].clone();
        feed.fold(&CatchUpAction::Toggle, Some(&first));
        feed.fold(&CatchUpAction::ToggleAll, None);
        assert_eq!(feed.collapsed.len(), 2);
        feed.fold(&CatchUpAction::ToggleAll, None);
        assert!(feed.collapsed.is_empty());

        // Refreshing keeps rooms collapsed.
        let first = feed.order[0].clone();
        feed.fold(&CatchUpAction::Toggle, Some(&first));
        feed.refresh();
        assert_eq!(feed.collapsed, HashSet::from([first.clone()]));
        assert_eq!(feed.dup().collapsed, HashSet::from([first]));
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
