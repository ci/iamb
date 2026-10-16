//! The `:goto` window, for jumping to a room, DM, space or list by typing part of its name.
use modalkit::actions::{EditAction, EditorActions, MacroAction};
use modalkit_ratatui::list::{List, ListCursor, ListItem, ListState};
use modalkit_ratatui::textbox::{TextBox, TextBoxState};
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};

use super::{
    GenericRoomItem,
    ListSectionHeader,
    RoomLikeItem,
    append_tags,
    room_fields_cmp,
    room_prompt,
    selected_style,
};
use crate::prelude::*;

/// The lists that `:goto` can open: what they're called, and the command that opens them.
const VIEWS: [(&str, &str); 12] = [
    ("Catch Up", "catchup"),
    ("Bookmarks", "bookmarks"),
    ("Unread Messages", "unreads"),
    ("Unread Mentions", "mentions"),
    ("Open Invites", "invites"),
    ("Profile Changes", "nicks"),
    ("DMs & Rooms", "chats"),
    ("Rooms", "rooms"),
    ("Direct Messages", "dms"),
    ("Spaces", "spaces"),
    ("Verifications", "verify"),
    ("Welcome", "welcome"),
];

fn view_id(command: &str) -> IambId {
    match command {
        "catchup" => IambId::CatchUp,
        "bookmarks" => IambId::BookmarkList,
        "unreads" => IambId::UnreadList,
        "mentions" => IambId::MentionsList,
        "invites" => IambId::InvitesList,
        "nicks" => IambId::ProfileChangeList,
        "chats" => IambId::ChatList,
        "rooms" => IambId::RoomList,
        "dms" => IambId::DirectList,
        "spaces" => IambId::SpaceList,
        "verify" => IambId::VerifyList,
        _ => IambId::Welcome,
    }
}

#[derive(Clone, Debug)]
enum GotoTarget {
    Room(GenericRoomItem),
    View(&'static str, &'static str),
}

/// A place that `:goto` can open.
#[derive(Clone, Debug)]
pub struct GotoItem {
    target: GotoTarget,

    /// Which graphemes of the name matched what was typed, to highlight them.
    matched: Vec<u32>,
}

impl GotoItem {
    fn name(&self) -> &str {
        match &self.target {
            GotoTarget::Room(room) => room.name(),
            GotoTarget::View(name, _) => name,
        }
    }

    /// Another name it can be found by: a room's alias, or a list's command.
    fn other_name(&self) -> Option<&str> {
        match &self.target {
            GotoTarget::Room(room) => room.alias().map(|alias| alias.as_str()),
            GotoTarget::View(_, command) => Some(command),
        }
    }

    /// The window this opens.
    pub fn id(&self) -> IambId {
        match &self.target {
            GotoTarget::Room(room) => {
                IambId::Room(room.room_id().to_owned().into(), RoomView::Main)
            },
            GotoTarget::View(_, command) => view_id(command),
        }
    }
}

impl Display for GotoItem {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.name())
    }
}

impl ListItem<IambInfo> for GotoItem {
    type Section = ListSectionHeader;

    fn show<'a>(
        &'a self,
        selected: bool,
        viewctx: &ViewportContext<ListCursor>,
        store: &'a ProgramStore,
    ) -> Text<'a> {
        match &self.target {
            GotoTarget::Room(room) => room.show_matched(selected, viewctx, store, &self.matched),
            GotoTarget::View(name, command) => {
                let theme = &store.application.settings.theme.rooms;
                let name_style = selected_style(selected, theme.default);
                let tags_style = if selected { name_style } else { theme.labels };

                let mut spans = vec![Span::styled("    ", theme.default)];
                spans.extend(matched_spans(name, &self.matched, name_style));

                let tags = vec![vec![Span::styled("List", tags_style)], vec![Span::styled(
                    format!(":{command}"),
                    tags_style,
                )]];
                append_tags(tags, &mut spans, tags_style);

                Text::from(Line::from(spans))
            },
        }
    }

    fn get_word(&self) -> Option<String> {
        match &self.target {
            GotoTarget::Room(room) => room.room_id().to_string().into(),
            GotoTarget::View(..) => None,
        }
    }
}

impl Promptable<ProgramContext, ProgramStore, IambInfo> for GotoItem {
    fn prompt(
        &mut self,
        act: &PromptAction,
        ctx: &ProgramContext,
        _: &mut ProgramStore,
    ) -> EditResult<Vec<(ProgramAction, ProgramContext)>, IambInfo> {
        let mut acts = match &self.target {
            GotoTarget::Room(room) => room_prompt(room.room_id(), act, ctx)?,
            GotoTarget::View(..) => {
                if act != &PromptAction::Submit {
                    let msg = "Can only open places from :goto";
                    return Err(EditError::Failure(msg.into()));
                }

                let open = WindowAction::Switch(OpenTarget::Application(self.id()));
                vec![(open.into(), ctx.clone())]
            },
        };

        // Typing a name leaves us in Insert mode, which the opened window shouldn't start in.
        let normal = MacroAction::Run("<Esc>".into(), Count::Exact(1));
        acts.push((normal.into(), ctx.clone()));

        Ok(acts)
    }
}

/// Split some text into spans, highlighting the graphemes at the given indices.
pub(super) fn matched_spans<'a>(text: &'a str, matched: &[u32], style: Style) -> Vec<Span<'a>> {
    let highlight = style.add_modifier(StyleModifier::BOLD | StyleModifier::UNDERLINED);
    let mut spans = vec![];
    let mut start = 0;
    let mut in_match = false;

    for (n, (idx, _)) in text.grapheme_indices(true).enumerate() {
        let is_match = u32::try_from(n).is_ok_and(|n| matched.binary_search(&n).is_ok());

        if is_match != in_match {
            if idx > start {
                let style = if in_match { highlight } else { style };
                spans.push(Span::styled(&text[start..idx], style));
            }

            start = idx;
            in_match = is_match;
        }
    }

    if start < text.len() || spans.is_empty() {
        let style = if in_match { highlight } else { style };
        spans.push(Span::styled(&text[start..], style));
    }

    spans
}

/// Text to match against, as one character per grapheme so match indices are grapheme indices.
///
/// [Utf32Str::new] also does this, but treats text as ASCII when each grapheme starts with an
/// ASCII character, giving byte indices for names with combining marks, like a decomposed "é".
fn haystack<'a>(text: &'a str, buf: &'a mut Vec<char>) -> Utf32Str<'a> {
    if text.is_ascii() {
        return Utf32Str::Ascii(text.as_bytes());
    }

    buf.clear();
    buf.extend(text.graphemes(true).filter_map(|grapheme| grapheme.chars().next()));
    Utf32Str::Unicode(buf)
}

/// Every place `:goto` can open, in the order to show them in before anything's typed: rooms,
/// DMs and spaces as `:chats` sorts them, and then the lists.
fn places(store: &mut ProgramStore) -> Vec<GotoItem> {
    let ChatStore {
        collator,
        rooms,
        spaces,
        settings,
        sync_info,
        worker,
        ..
    } = &mut store.application;
    let client = &worker.client;

    let mut chats = sync_info
        .rooms
        .iter()
        .chain(sync_info.dms.iter())
        .chain(sync_info.spaces.iter())
        .map(|room| {
            let info = rooms.get_or_default(room.room_id().to_owned());
            GenericRoomItem::new(room, info, client, spaces)
        })
        .collect::<Vec<_>>();

    let fields = &settings.tunables.sort.chats;
    chats.sort_by(|a, b| room_fields_cmp(a, b, fields, collator));

    let chats = chats.into_iter().map(GotoTarget::Room);
    let views = VIEWS.into_iter().map(|(name, command)| GotoTarget::View(name, command));

    chats
        .chain(views)
        .map(|target| GotoItem { target, matched: vec![] })
        .collect()
}

/// The places matching a query, best first, keeping their order among equally good matches.
fn filter(places: Vec<GotoItem>, query: &str) -> Vec<GotoItem> {
    let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
    if pattern.atoms.is_empty() {
        return places;
    }

    let mut config = Config::DEFAULT;
    config.prefer_prefix = true;
    let mut matcher = Matcher::new(config);
    let mut buf = vec![];

    let mut scored = places
        .into_iter()
        .filter_map(|mut place| {
            let mut matched = vec![];
            let name = haystack(place.name(), &mut buf);
            let name_score = pattern.indices(name, &mut matcher, &mut matched);

            let other_score = place
                .other_name()
                .and_then(|other| pattern.score(haystack(other, &mut buf), &mut matcher));

            let score = match (name_score, other_score) {
                (Some(name), Some(other)) if other > name => {
                    matched.clear();
                    other
                },
                (Some(name), _) => name,
                (None, Some(other)) => {
                    matched.clear();
                    other
                },
                (None, None) => return None,
            };

            matched.sort_unstable();
            matched.dedup();
            place.matched = matched;

            Some((score, place))
        })
        .collect::<Vec<_>>();

    scored.sort_by(|(a, _), (b, _)| b.cmp(a));
    scored.into_iter().map(|(_, place)| place).collect()
}

/// Whether a motion moves between lines, which in `:goto` means between places, since the
/// query is a single line.
fn is_line_motion(act: &EditorAction, ctx: &ProgramContext) -> bool {
    let EditorAction::Edit(op, EditTarget::Motion(motion, _)) = act else {
        return false;
    };

    ctx.resolve(op) == EditAction::Motion &&
        matches!(
            motion,
            MoveType::Line(_) |
                MoveType::ScreenLine(_) |
                MoveType::BufferPos(_) |
                MoveType::BufferLineOffset |
                MoveType::BufferLinePercent
        )
}

pub type GotoListState = ListState<GotoItem, IambInfo>;

/// The `:goto` window: what's been typed, and the places that match it.
pub struct GotoState {
    tbox: TextBoxState<IambInfo>,
    list: GotoListState,

    /// The query that the list was last filtered with.
    query: Option<String>,
}

impl GotoState {
    pub fn new(store: &mut ProgramStore) -> Self {
        let buf = store.load_buffer(IambBufferId::GotoQuery);
        let mut tbox = TextBoxState::new(buf);
        let list = ListState::new(IambBufferId::Goto, vec![]);

        // Start afresh each time it's opened.
        tbox.set_text("");

        GotoState { tbox, list, query: None }
    }

    fn query(&self) -> String {
        // The text box always ends its text with a newline.
        let mut text = self.tbox.get_text();
        if text.ends_with('\n') {
            text.pop();
        }
        text
    }

    /// Refresh the matching places, selecting the best one when the query has changed, and
    /// otherwise keeping the same one selected.
    pub fn update(&mut self, store: &mut ProgramStore) {
        let query = self.query();
        let places = filter(places(store), &query);

        if self.query.as_ref() == Some(&query) {
            let selected = self.list.get().map(GotoItem::id);
            let idx = selected.and_then(|id| places.iter().position(|place| place.id() == id));
            self.list.set(places);

            if let Some(idx) = idx {
                self.select(idx, store);
            }
        } else {
            self.list.set(places);
            self.select(0, store);
            self.query = Some(query);
        }
    }

    fn select(&mut self, idx: usize, store: &mut ProgramStore) {
        let count = Count::Exact(idx.saturating_add(1));
        let target = EditTarget::Motion(MoveType::BufferLineOffset, count);
        let _ = self
            .list
            .edit(&EditAction::Motion, &target, &ProgramContext::default(), store);
    }

    fn move_selection(
        &mut self,
        dir: MoveDir1D,
        ctx: &ProgramContext,
        store: &mut ProgramStore,
    ) -> EditResult<EditInfo, IambInfo> {
        let target = EditTarget::Motion(MoveType::Line(dir), Count::Exact(1));
        self.list.edit(&EditAction::Motion, &target, ctx, store)
    }

    pub fn draw(&mut self, area: Rect, buf: &mut Buffer, focused: bool, store: &mut ProgramStore) {
        self.update(store);

        let settings = &store.application.settings;
        let theme = &settings.theme;

        let queryh = area.height.min(1);
        let queryarea = Rect::new(area.x, area.y, area.width, queryh);
        let listarea = Rect::new(area.x, area.y + queryh, area.width, area.height - queryh);

        // Show how many places match, after what's been typed.
        let total = sync_count(store) + VIEWS.len();
        let count = format!(" {}/{total} ", self.list.len());
        let countw = (count.width() as u16).min(queryarea.width / 2);
        let textarea =
            Rect::new(queryarea.x, queryarea.y, queryarea.width - countw, queryarea.height);
        let countarea = Rect::new(textarea.x + textarea.width, queryarea.y, countw, queryh);

        let style = store.application.settings.theme.msgbar.default;
        self.tbox.set_ignorecase(settings.tunables.ignorecase);
        TextBox::new()
            .style(style)
            .prompt(Line::from("> "))
            .render(textarea, buf, &mut self.tbox);
        Paragraph::new(Span::styled(count, theme.rooms.labels))
            .alignment(Alignment::Right)
            .render(countarea, buf);

        let style = store.application.settings.theme.rooms.default;
        List::new(store)
            .empty_message("Nothing matches")
            .empty_alignment(Alignment::Center)
            .focus(focused)
            .style(style)
            .render(listarea, buf, &mut self.list);
    }

    pub fn dup(&self, store: &mut ProgramStore) -> Self {
        let buf = store.load_buffer(IambBufferId::GotoQuery);
        let tbox = TextBoxState::new(buf);

        GotoState {
            tbox,
            list: self.list.dup(store),
            query: self.query.clone(),
        }
    }
}

/// How many rooms, DMs and spaces there are.
fn sync_count(store: &ProgramStore) -> usize {
    let sync_info = &store.application.sync_info;
    sync_info.rooms.len() + sync_info.dms.len() + sync_info.spaces.len()
}

impl Editable<ProgramContext, ProgramStore, IambInfo> for GotoState {
    fn editor_command(
        &mut self,
        act: &EditorAction,
        ctx: &ProgramContext,
        store: &mut ProgramStore,
    ) -> EditResult<EditInfo, IambInfo> {
        // Moving up and down, or completing, picks a place; everything else edits the query.
        match act {
            EditorAction::Complete(CompletionStyle::List(dir, _), _, _) => {
                self.move_selection(*dir, ctx, store)
            },
            act if is_line_motion(act, ctx) => self.list.editor_command(act, ctx, store),

            // The query is a single line, so newlines typed into it, like from <Enter> in Insert
            // mode when `send_on_enter` is off, are left out.
            EditorAction::InsertText(InsertTextAction::Type(
                Specifier::Exact(Char::Single('\n')),
                _,
                _,
            )) => Ok(None),

            // And pasted ones become spaces.
            EditorAction::InsertText(InsertTextAction::Transcribe(text, dir, count))
                if text.contains('\n') =>
            {
                let text = text.replace('\n', " ");
                let act = InsertTextAction::Transcribe(text, *dir, count.clone());
                self.tbox.editor_command(&act.into(), ctx, store)
            },

            act => {
                let res = self.tbox.editor_command(act, ctx, store);

                // Anything else adding lines, like pasting a register, is joined back up.
                let query = self.query();
                if query.contains('\n') {
                    self.tbox.set_text(query.replace('\n', " "));
                }

                res
            },
        }
    }
}

impl Jumpable<ProgramContext, IambInfo> for GotoState {
    fn jump(
        &mut self,
        _: PositionList,
        _: MoveDir1D,
        count: usize,
        _: &ProgramContext,
    ) -> IambResult<usize> {
        // Picking a place moves the list on every redraw, which isn't worth jumping back through,
        // so leave jumps to the window history, like going back to where :goto was opened from.
        Ok(count)
    }
}

impl Scrollable<ProgramContext, ProgramStore, IambInfo> for GotoState {
    fn scroll(
        &mut self,
        style: &ScrollStyle,
        ctx: &ProgramContext,
        store: &mut ProgramStore,
    ) -> EditResult<EditInfo, IambInfo> {
        self.list.scroll(style, ctx, store)
    }
}

impl Promptable<ProgramContext, ProgramStore, IambInfo> for GotoState {
    fn prompt(
        &mut self,
        act: &PromptAction,
        ctx: &ProgramContext,
        store: &mut ProgramStore,
    ) -> EditResult<Vec<(ProgramAction, ProgramContext)>, IambInfo> {
        // The list may not have been drawn yet, like when typing quickly or running a macro.
        self.update(store);
        self.list.prompt(act, ctx, store)
    }
}

impl TerminalCursor for GotoState {
    fn get_term_cursor(&self) -> Option<TermOffset> {
        self.tbox.get_term_cursor()
    }

    fn hide_term_cursor(&self) -> bool {
        false
    }
}

impl WindowOps<IambInfo> for GotoState {
    fn draw(&mut self, area: Rect, buf: &mut Buffer, focused: bool, store: &mut ProgramStore) {
        GotoState::draw(self, area, buf, focused, store)
    }

    fn dup(&self, store: &mut ProgramStore) -> Self {
        GotoState::dup(self, store)
    }

    fn close(&mut self, _: CloseFlags, _: &mut ProgramStore) -> bool {
        true
    }

    fn write(
        &mut self,
        _: Option<&str>,
        _: WriteFlags,
        _: &mut ProgramStore,
    ) -> IambResult<EditInfo> {
        Ok(None)
    }

    fn get_completions(&self) -> Option<CompletionList> {
        None
    }

    fn get_cursor_word(&self, style: &WordStyle) -> Option<String> {
        self.tbox.get_cursor_word(style)
    }

    fn get_selected_word(&self) -> Option<String> {
        self.tbox.get_selected_word()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::*;

    fn views() -> Vec<GotoItem> {
        VIEWS
            .into_iter()
            .map(|(name, command)| {
                GotoItem {
                    target: GotoTarget::View(name, command),
                    matched: vec![],
                }
            })
            .collect()
    }

    fn names(places: &[GotoItem]) -> Vec<&str> {
        places.iter().map(GotoItem::name).collect()
    }

    #[test]
    fn test_filter() {
        // Nothing typed keeps everything, in order.
        assert_eq!(names(&filter(views(), "")), names(&views()));
        assert_eq!(names(&filter(views(), "  ")), names(&views()));

        // Letters can be spread out, and case only matters when typed.
        assert_eq!(names(&filter(views(), "bkm"))[0], "Bookmarks");
        assert_eq!(names(&filter(views(), "catch"))[0], "Catch Up");
        assert_eq!(names(&filter(views(), "unrmsg")), vec!["Unread Messages"]);
        assert!(filter(views(), "CATCH").is_empty());
        assert!(filter(views(), "zzz").is_empty());

        // Lists can be found by their commands too.
        assert_eq!(names(&filter(views(), "nicks")), vec!["Profile Changes"]);
        assert_eq!(names(&filter(views(), "chats"))[0], "DMs & Rooms");

        // The matching part of the name is highlighted, but not when matched by command.
        let found = filter(views(), "cu");
        assert_eq!(found[0].name(), "Catch Up");
        assert_eq!(found[0].matched, vec![0, 6]);
        assert!(filter(views(), "nicks")[0].matched.is_empty());
    }

    #[test]
    fn test_matched_spans() {
        let style = Style::default();
        let hl = style.add_modifier(StyleModifier::BOLD | StyleModifier::UNDERLINED);
        let spans = matched_spans("Catch Up", &[0, 6], style);
        assert_eq!(spans, vec![
            Span::styled("C", hl),
            Span::styled("atch ", style),
            Span::styled("U", hl),
            Span::styled("p", style),
        ]);

        // Matches index graphemes, not bytes.
        let spans = matched_spans("☕ café", &[0, 5], style);
        assert_eq!(spans, vec![
            Span::styled("☕", hl),
            Span::styled(" caf", style),
            Span::styled("é", hl),
        ]);

        assert_eq!(matched_spans("", &[], style), vec![Span::styled("", style)]);

        // Including when every grapheme starts with an ASCII character, like with a decomposed é.
        let room = GotoItem {
            target: GotoTarget::View("Cafe\u{301} Chat", "x"),
            matched: vec![],
        };
        let found = filter(vec![room], "Chat");
        assert_eq!(found[0].matched, vec![5, 6, 7, 8]);
        let spans = matched_spans(found[0].name(), &found[0].matched, style);
        assert_eq!(spans, vec![
            Span::styled("Cafe\u{301} ", style),
            Span::styled("Chat", hl)
        ]);
    }

    #[tokio::test]
    async fn test_goto_window() {
        let mut store = mock_store().await;
        let ctx = ProgramContext::default();
        let mut win = GotoState::new(&mut store);
        win.update(&mut store);
        assert_eq!(win.list.get().map(GotoItem::id), Some(IambId::CatchUp));

        // Moving down a line, or completing, picks the next place.
        let down = EditorAction::Edit(
            EditAction::Motion.into(),
            EditTarget::Motion(MoveType::Line(MoveDir1D::Next), Count::Exact(1)),
        );
        win.editor_command(&down, &ctx, &mut store).unwrap();
        assert_eq!(win.list.get().map(GotoItem::id), Some(IambId::BookmarkList));
        let next = EditorAction::Complete(
            CompletionStyle::List(MoveDir1D::Next, true),
            CompletionType::Auto,
            CompletionDisplay::List,
        );
        win.editor_command(&next, &ctx, &mut store).unwrap();
        assert_eq!(win.list.get().map(GotoItem::id), Some(IambId::UnreadList));
        assert_eq!(win.query(), "");

        // Redrawing keeps the same place selected, without filling the jumplist, so <C-O> goes
        // back to the previous window.
        win.update(&mut store);
        win.update(&mut store);
        assert_eq!(win.list.get().map(GotoItem::id), Some(IambId::UnreadList));
        assert_eq!(win.jump(PositionList::JumpList, MoveDir1D::Previous, 1, &ctx).unwrap(), 1);

        // Typing edits the query, which picks the best match again.
        let typed = |c| {
            EditorAction::InsertText(InsertTextAction::Type(
                Char::Single(c).into(),
                MoveDir1D::Previous,
                Count::Exact(1),
            ))
        };
        for c in ['n', 'i', 'c', '\n', 'k'] {
            win.editor_command(&typed(c), &ctx, &mut store).unwrap();
        }

        // Newlines, like from <Enter> when `send_on_enter` is off, are left out.
        assert_eq!(win.query(), "nick");
        let paste = EditorAction::InsertText(InsertTextAction::Transcribe(
            "a\nb".into(),
            MoveDir1D::Previous,
            Count::Exact(1),
        ));
        win.editor_command(&paste, &ctx, &mut store).unwrap();
        win.editor_command(&typed('c'), &ctx, &mut store).unwrap();
        assert_eq!(win.query(), "nicka bc");
        win.tbox.set_text("nick");
        win.update(&mut store);
        assert_eq!(win.list.get().map(GotoItem::id), Some(IambId::ProfileChangeList));

        // Submitting opens it, even before the list is drawn again, and leaves Insert mode.
        let acts = win.prompt(&PromptAction::Submit, &ctx, &mut store).unwrap();
        let acts = acts.into_iter().map(|(act, _)| act).collect::<Vec<_>>();
        assert_eq!(acts, vec![
            WindowAction::Switch(OpenTarget::Application(IambId::ProfileChangeList)).into(),
            MacroAction::Run("<Esc>".into(), Count::Exact(1)).into(),
        ]);

        // It shows what's been typed, how many places match, and the matches.
        let area = Rect::new(0, 0, 40, 4);
        let mut buf = Buffer::empty(area);
        win.draw(area, &mut buf, true, &mut store);
        let rows = (0..area.height)
            .map(|y| (0..area.width).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>();
        assert!(rows[0].starts_with("> nick"), "{rows:?}");
        assert!(rows[0].ends_with(" 1/12 "), "{rows:?}");
        assert_eq!(rows[1].trim_end(), "    Profile Changes (List, :nicks)");
        assert_eq!(rows[2].trim(), "");

        // Opening it again starts afresh.
        let win = GotoState::new(&mut store);
        assert_eq!(win.query(), "");
    }
}
