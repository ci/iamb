//! Image previews for messages shown in list windows, like `:catchup` and `:bookmarks`.
//!
//! Lists only draw text, so once a list has been drawn, this works out where each visible
//! message landed the same way the list does, and draws its images over their placeholders.
use modalkit_ratatui::list::{ListItem, ListState};
use ratatui_image::sliced::{SignedPosition, SlicedImage};

use crate::prelude::*;

/// A list item that shows a message.
pub trait PreviewItem {
    /// The message, the message shown before it, and the line of the item's text it starts on.
    fn preview_message<'a>(
        &self,
        store: &'a ProgramStore,
    ) -> Option<(&'a RoomInfo, &'a Message, Option<&'a Message>, usize)>;
}

/// Whether list windows show image previews.
pub fn enabled(settings: &ApplicationSettings) -> bool {
    let image_preview = &settings.tunables.image_preview;
    image_preview.enabled && image_preview.lists
}

/// The previews to show messages with in list windows.
pub fn previews(store: &ProgramStore) -> &PreviewManager {
    if enabled(&store.application.settings) {
        &store.application.previews
    } else {
        PreviewManager::text_only()
    }
}

/// The images a message shows, which need loading before they can be drawn.
fn sources(info: &RoomInfo, msg: &Message) -> Vec<(MediaSource, PreviewKind)> {
    let mut sources = vec![];

    if let Some(source) = msg.image_preview() {
        sources.push((source.clone(), PreviewKind::Message));
    }

    let reply = msg
        .reply_to()
        .or_else(|| msg.thread_root())
        .and_then(|event_id| info.get_event(&event_id))
        .and_then(|reply| reply.image_preview());
    if let Some(source) = reply {
        sources.push((source.clone(), PreviewKind::Message));
    }

    if let Some(event_id) = msg.event.event_id() {
        let reactions = info.get_reaction_images(event_id);
        sources.extend(reactions.map(|source| (source.clone(), PreviewKind::Reaction)));
    }

    sources
}

/// Where the text of each visible item starts on screen, as `(index, row)`, following the same
/// layout as the list uses when drawing: section headings take a line when the section changes,
/// and a selected item too tall for the window is drawn on its own.
fn layout<T>(
    state: &ListState<T, IambInfo>,
    area: Rect,
    focused: bool,
    store: &ProgramStore,
) -> Vec<(usize, i32, usize)>
where
    T: ListItem<IambInfo>,
{
    let viewctx = state.last_viewctx();
    let height = viewctx.get_height();
    let items = state.get_items();
    let corner = &viewctx.corner;

    if height == 0 || corner.position >= items.len() {
        return vec![];
    }

    let selected = state.get();
    let mut section = corner.position.checked_sub(1).and_then(|n| items[n].get_section());
    let mut top = i32::from(area.top());
    let mut tops = vec![];

    for (idx, item) in items.iter().enumerate().skip(corner.position) {
        let sel = selected.is_some_and(|s| std::ptr::eq(s, item));
        let lines = item.show(focused && sel, viewctx, store).lines.len();
        let item_section = item.get_section();
        let header = usize::from(item_section != section && item_section.is_some()) as i32;
        section = item_section;

        let text_top = if idx != corner.position {
            top + header
        } else if sel && lines >= height {
            i32::from(area.top())
        } else {
            i32::from(area.top()) + header - corner.text_row as i32
        };

        tops.push((idx, text_top, lines));
        top = text_top + lines as i32;

        if top >= i32::from(area.bottom()) {
            break;
        }
    }

    tops
}

/// Load and draw the image previews of the visible items, after the list has drawn its text.
pub fn draw<T>(
    state: &ListState<T, IambInfo>,
    area: Rect,
    buf: &mut Buffer,
    focused: bool,
    store: &mut ProgramStore,
) where
    T: ListItem<IambInfo> + PreviewItem,
{
    if !enabled(&store.application.settings) {
        return;
    }

    let viewctx = state.last_viewctx();
    let items = state.get_items();
    let selected = state.get();
    let mut loads = vec![];
    let mut images = vec![];

    for (idx, text_top, _) in layout(state, area, focused, store) {
        let item = &items[idx];
        let sel = selected.is_some_and(|s| std::ptr::eq(s, item));

        let Some((info, msg, prev, line)) = item.preview_message(store) else {
            continue;
        };

        loads.extend(sources(info, msg));

        let settings = &store.application.settings;
        let previews = &store.application.previews;
        let width = viewctx.get_width();
        let (_, protos) =
            msg.show_with_preview(prev, focused && sel, width, info, settings, previews);

        for (proto, x, y) in protos {
            let y = text_top + line as i32 + i32::from(y);
            images.push((area.left() + x, y, proto));
        }
    }

    for (source, kind) in loads {
        let ChatStore { previews, worker, .. } = &mut store.application;
        previews.load(&source, kind, worker);
    }

    let msg_width = Message::message_column_width(viewctx, &store.application.settings);

    for (x, y, proto) in images {
        let size = proto.size();

        if i32::from(size.height) + y < i32::from(area.top()) {
            continue;
        }

        let hidden = (i32::from(area.top()) - y).max(0);
        let Ok(hidden_lines) = i16::try_from(hidden) else {
            continue;
        };
        let Ok(y) = u16::try_from(y + hidden) else {
            continue;
        };

        let mut rect: Rect = size.into();
        rect.x = x;
        rect.y = y;
        rect.height = rect.height.saturating_sub(hidden_lines as u16);
        rect.width = rect.width.min(msg_width as u16);

        let rect = rect.intersection(area);
        if !rect.is_empty() {
            let position = SignedPosition { x: 0, y: -hidden_lines };
            SlicedImage::new(&proto, position).render(rect, buf);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::*;
    use crate::windows::catchup::tests::mock_item;
    use modalkit::actions::{EditAction, EditorActions};
    use modalkit_ratatui::list::List;

    fn row_text(buf: &Buffer, area: Rect, y: i32) -> String {
        let Ok(y) = u16::try_from(y) else {
            return String::new();
        };
        if y < area.top() || y >= area.bottom() {
            return String::new();
        }
        (area.left()..area.right())
            .map(|x| buf[(x, y)].symbol().to_string())
            .collect()
    }

    #[tokio::test]
    async fn test_layout_matches_list() {
        let mut store = mock_store().await;
        let area = Rect::new(0, 0, 60, 7);

        let rooms = [
            "!a:example.com",
            "!b:example.com",
            "!c:example.com",
            "!d:example.com",
        ];
        let items = rooms
            .iter()
            .flat_map(|room| (0..2).map(move |n| mock_item(room, n)))
            .collect::<Vec<_>>();

        for selected in 0..items.len() {
            let mut state = ListState::new(IambBufferId::CatchUp, items.clone());
            let count = Count::Exact(selected + 1);
            let target = EditTarget::Motion(MoveType::BufferLineOffset, count);
            state
                .edit(&EditAction::Motion, &target, &ProgramContext::default(), &mut store)
                .unwrap();

            let mut buf = Buffer::empty(area);
            List::new(&mut store).focus(true).render(area, &mut buf, &mut state);

            let tops = layout(&state, area, true, &store);
            assert!(!tops.is_empty());

            for (idx, top, _) in tops {
                // Every room's first item follows its heading.
                if idx % 2 == 0 && top > i32::from(area.top()) {
                    let heading = row_text(&buf, area, top - 1);
                    assert!(heading.contains(rooms[idx / 2]), "{selected} {idx}: {heading:?}");
                }

                // And each item's text starts where we expect it to.
                let text = row_text(&buf, area, top);
                if !text.trim().is_empty() {
                    assert!(text.contains("Unable to load room"), "{selected} {idx}: {text:?}");
                }
            }
        }
    }
}
