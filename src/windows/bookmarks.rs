//! The `:bookmarks` window, listing messages saved for later from every room.
use modalkit_ratatui::list::{ListCursor, ListItem, ListState};

use crate::bookmarks::BookmarkEntry;
use crate::prelude::*;

pub type BookmarkListState = ListState<BookmarkItem, IambInfo>;

#[derive(Clone)]
pub struct BookmarkItem {
    entry: BookmarkEntry,
}

impl BookmarkItem {
    pub fn new(entry: BookmarkEntry) -> Self {
        Self { entry }
    }

    pub fn room_id(&self) -> &RoomId {
        &self.entry.room_id
    }

    pub fn event_id(&self) -> &EventId {
        &self.entry.event_id
    }
}

impl Display for BookmarkItem {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match &self.entry.bookmark.note {
            Some(note) => write!(f, "{} {note}", self.entry.event_id),
            None => write!(f, "{}", self.entry.event_id),
        }
    }
}

impl ListItem<IambInfo> for BookmarkItem {
    type Section = &'static str;

    fn show<'a>(
        &'a self,
        selected: bool,
        vwctx: &ViewportContext<ListCursor>,
        store: &'a ProgramStore,
    ) -> Text<'a> {
        let settings = &store.application.settings;
        let style = settings.theme.default;
        let header_style = if selected {
            style.add_modifier(StyleModifier::REVERSED)
        } else {
            style
        };

        let title = store.application.get_room_title(&self.entry.room_id);
        let saved = MessageTimeStamp::from(self.entry.bookmark.saved_at).show_datetime();
        let mut header = vec![
            Span::styled(title, header_style.add_modifier(StyleModifier::BOLD)),
            Span::styled(format!(" · saved {saved}"), header_style.fg(Color::Gray)),
        ];
        if let Some(note) = &self.entry.bookmark.note {
            header.push(Span::styled(" · ", header_style.fg(Color::Gray)));
            header.push(Span::styled(note.as_str(), header_style));
        }

        let mut text = Text::from(Line::from(header));

        let info = store.application.rooms.get(&self.entry.room_id);
        let msg = info.and_then(|info| info.get_pinned(&self.entry.event_id));

        match (info, msg) {
            (Some(info), Some(msg)) => {
                let previews = &store.application.previews;
                let width = vwctx.get_width();
                text.extend(msg.show(None, selected, width, info, settings, previews));
            },
            (Some(info), None) if info.pinned_unavailable(&self.entry.event_id) => {
                text.push_line(Span::styled("Unable to load message", style.fg(Color::Gray)));
            },
            _ => {
                text.push_line(Span::styled("Loading message...", style.fg(Color::Gray)));
            },
        }

        text
    }

    fn get_word(&self) -> Option<String> {
        self.entry.event_id.to_string().into()
    }
}

impl Promptable<ProgramContext, ProgramStore, IambInfo> for BookmarkItem {
    fn prompt(
        &mut self,
        act: &PromptAction,
        ctx: &ProgramContext,
        store: &mut ProgramStore,
    ) -> EditResult<Vec<(ProgramAction, ProgramContext)>, IambInfo> {
        match act {
            PromptAction::Submit => {
                let info = store.application.rooms.get_or_default(self.entry.room_id.clone());
                let thread = info
                    .get_message_location(&self.entry.event_id)
                    .and_then(|(thread, _)| thread)
                    .map(ToOwned::to_owned);

                let room = IambId::Room(self.entry.room_id.clone().into(), thread.into());
                let open = WindowAction::Switch(OpenTarget::Application(room));
                let jump = TimelineAction::GotoEvent(self.entry.event_id.clone());

                Ok(vec![
                    (open.into(), ctx.clone()),
                    (IambAction::from(jump).into(), ctx.clone()),
                ])
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

/// Request any bookmarked messages that aren't loaded yet.
pub fn fetch_missing(store: &mut ChatStore, items: &[BookmarkItem]) {
    let mut missing: HashMap<OwnedRoomId, Vec<OwnedEventId>> = HashMap::new();

    for item in items {
        let Some(info) = store.rooms.get(item.room_id()) else {
            continue;
        };

        if info.get_pinned(item.event_id()).is_none() && !info.pinned_unavailable(item.event_id()) {
            missing
                .entry(item.room_id().to_owned())
                .or_default()
                .push(item.event_id().to_owned());
        }
    }

    for (room_id, event_ids) in missing {
        store.need_load.need_events(room_id, event_ids);
    }
}
