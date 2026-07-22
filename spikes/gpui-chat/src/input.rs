use std::{
    ops::Range,
    time::{Duration, Instant},
};

use gpui::{
    App, Bounds, ClipboardItem, Context, CursorStyle, Element, ElementId, ElementInputHandler,
    Entity, EntityInputHandler, FocusHandle, Focusable, GlobalElementId, LayoutId, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point, SharedString, Style,
    TextAlign, TextRun, UTF16Selection, UnderlineStyle, Window, WrappedLine, actions, div, fill,
    point, prelude::*, px, relative, rgba, size,
};
use unicode_segmentation::UnicodeSegmentation;

const EDIT_GROUP_WINDOW: Duration = Duration::from_millis(750);
const HISTORY_LIMIT: usize = 100;

actions!(
    composer,
    [
        Backspace,
        Delete,
        Left,
        Right,
        Up,
        Down,
        SelectLeft,
        SelectRight,
        SelectUp,
        SelectDown,
        Home,
        End,
        SelectHome,
        SelectEnd,
        SelectAll,
        Paste,
        Cut,
        Copy,
        Newline,
        Undo,
        Redo
    ]
);

pub fn bind_keys(cx: &mut App) {
    use gpui::KeyBinding;
    cx.bind_keys([
        KeyBinding::new("backspace", Backspace, Some("Composer")),
        KeyBinding::new("delete", Delete, Some("Composer")),
        KeyBinding::new("left", Left, Some("Composer")),
        KeyBinding::new("right", Right, Some("Composer")),
        KeyBinding::new("up", Up, Some("Composer")),
        KeyBinding::new("down", Down, Some("Composer")),
        KeyBinding::new("shift-left", SelectLeft, Some("Composer")),
        KeyBinding::new("shift-right", SelectRight, Some("Composer")),
        KeyBinding::new("shift-up", SelectUp, Some("Composer")),
        KeyBinding::new("shift-down", SelectDown, Some("Composer")),
        KeyBinding::new("cmd-left", Home, Some("Composer")),
        KeyBinding::new("cmd-right", End, Some("Composer")),
        KeyBinding::new("cmd-shift-left", SelectHome, Some("Composer")),
        KeyBinding::new("cmd-shift-right", SelectEnd, Some("Composer")),
        KeyBinding::new("cmd-a", SelectAll, Some("Composer")),
        KeyBinding::new("cmd-v", Paste, Some("Composer")),
        KeyBinding::new("cmd-c", Copy, Some("Composer")),
        KeyBinding::new("cmd-x", Cut, Some("Composer")),
        KeyBinding::new("enter", Newline, Some("Composer")),
        KeyBinding::new("cmd-z", Undo, Some("Composer")),
        KeyBinding::new("cmd-shift-z", Redo, Some("Composer")),
    ]);
}

#[derive(Clone, Debug, PartialEq)]
struct EditorSnapshot {
    content: SharedString,
    selection: Range<usize>,
    reversed: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EditKind {
    Insert,
    Delete,
    Replace,
    Ime,
}

#[derive(Debug, Default)]
struct EditHistory {
    undo: Vec<EditorSnapshot>,
    redo: Vec<EditorSnapshot>,
    last_group: Option<(EditKind, Instant)>,
}

impl EditHistory {
    fn record(&mut self, snapshot: EditorSnapshot, kind: EditKind) {
        let now = Instant::now();
        let grouped = self.last_group.is_some_and(|(previous, at)| {
            previous == kind
                && matches!(kind, EditKind::Insert | EditKind::Delete | EditKind::Ime)
                && now.duration_since(at) <= EDIT_GROUP_WINDOW
        });
        if !grouped {
            self.undo.push(snapshot);
            if self.undo.len() > HISTORY_LIMIT {
                self.undo.remove(0);
            }
        }
        self.redo.clear();
        self.last_group = Some((kind, now));
    }

    fn break_group(&mut self) {
        self.last_group = None;
    }

    fn undo(&mut self, current: EditorSnapshot) -> Option<EditorSnapshot> {
        let previous = self.undo.pop()?;
        self.redo.push(current);
        self.last_group = None;
        Some(previous)
    }

    fn redo(&mut self, current: EditorSnapshot) -> Option<EditorSnapshot> {
        let next = self.redo.pop()?;
        self.undo.push(current);
        self.last_group = None;
        Some(next)
    }

    fn clear(&mut self) {
        self.undo.clear();
        self.redo.clear();
        self.last_group = None;
    }
}

#[derive(Clone, Debug)]
struct LayoutLine {
    line: WrappedLine,
    byte_start: usize,
    visual_start: usize,
    visual_rows: usize,
}

pub struct Composer {
    focus: FocusHandle,
    content: SharedString,
    placeholder: SharedString,
    selection: Range<usize>,
    reversed: bool,
    marked: Option<Range<usize>>,
    last_layout: Vec<LayoutLine>,
    last_bounds: Option<Bounds<Pixels>>,
    line_height: Pixels,
    first_visible_row: usize,
    is_selecting: bool,
    preferred_x: Option<Pixels>,
    history: EditHistory,
}

impl Composer {
    pub fn new(cx: &mut Context<Self>, placeholder: impl Into<SharedString>) -> Self {
        Self {
            focus: cx.focus_handle(),
            content: "".into(),
            placeholder: placeholder.into(),
            selection: 0..0,
            reversed: false,
            marked: None,
            last_layout: Vec::new(),
            last_bounds: None,
            line_height: px(20.),
            first_visible_row: 0,
            is_selecting: false,
            preferred_x: None,
            history: EditHistory::default(),
        }
    }

    pub fn text(&self) -> String {
        self.content.to_string()
    }

    pub fn set_text(&mut self, text: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.content = text.into();
        self.selection = self.content.len()..self.content.len();
        self.reversed = false;
        self.marked = None;
        self.preferred_x = None;
        self.history.clear();
        cx.notify();
    }

    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.set_text("", cx);
    }

    fn snapshot(&self) -> EditorSnapshot {
        EditorSnapshot {
            content: self.content.clone(),
            selection: self.selection.clone(),
            reversed: self.reversed,
        }
    }

    fn restore(&mut self, snapshot: EditorSnapshot, cx: &mut Context<Self>) {
        self.content = snapshot.content;
        self.selection = snapshot.selection;
        self.reversed = snapshot.reversed;
        self.marked = None;
        self.preferred_x = None;
        cx.notify();
    }

    fn cursor(&self) -> usize {
        if self.reversed {
            self.selection.start
        } else {
            self.selection.end
        }
    }

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        let offset = offset.min(self.content.len());
        self.selection = offset..offset;
        self.reversed = false;
        self.preferred_x = None;
        self.history.break_group();
        cx.notify();
    }

    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        let offset = offset.min(self.content.len());
        if self.reversed {
            self.selection.start = offset;
        } else {
            self.selection.end = offset;
        }
        if self.selection.end < self.selection.start {
            self.reversed = !self.reversed;
            self.selection = self.selection.end..self.selection.start;
        }
        self.preferred_x = None;
        self.history.break_group();
        cx.notify();
    }

    fn previous(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .rev()
            .find_map(|(index, _)| (index < offset).then_some(index))
            .unwrap_or(0)
    }

    fn next(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .find_map(|(index, _)| (index > offset).then_some(index))
            .unwrap_or(self.content.len())
    }

    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        if self.selection.is_empty() {
            self.move_to(self.previous(self.cursor()), cx);
        } else {
            self.move_to(self.selection.start, cx);
        }
    }

    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        if self.selection.is_empty() {
            self.move_to(self.next(self.cursor()), cx);
        } else {
            self.move_to(self.selection.end, cx);
        }
    }

    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.previous(self.cursor()), cx);
    }

    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.next(self.cursor()), cx);
    }

    fn up(&mut self, _: &Up, _: &mut Window, cx: &mut Context<Self>) {
        self.move_vertically(-1, false, cx);
    }

    fn down(&mut self, _: &Down, _: &mut Window, cx: &mut Context<Self>) {
        self.move_vertically(1, false, cx);
    }

    fn select_up(&mut self, _: &SelectUp, _: &mut Window, cx: &mut Context<Self>) {
        self.move_vertically(-1, true, cx);
    }

    fn select_down(&mut self, _: &SelectDown, _: &mut Window, cx: &mut Context<Self>) {
        self.move_vertically(1, true, cx);
    }

    fn move_vertically(&mut self, rows: isize, selecting: bool, cx: &mut Context<Self>) {
        let Some(position) = self.layout_position_for_offset(self.cursor()) else {
            return;
        };
        let preferred_x = self.preferred_x.unwrap_or(position.x);
        let target_y = position.y + self.line_height * rows as f32;
        let target = self.offset_for_layout_position(point(preferred_x, target_y));
        if selecting {
            self.select_to(target, cx);
        } else {
            self.move_to(target, cx);
        }
        self.preferred_x = Some(preferred_x);
    }

    fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to_visual_edge(false, false, cx);
    }

    fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to_visual_edge(true, false, cx);
    }

    fn select_home(&mut self, _: &SelectHome, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to_visual_edge(false, true, cx);
    }

    fn select_end(&mut self, _: &SelectEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to_visual_edge(true, true, cx);
    }

    fn move_to_visual_edge(&mut self, end: bool, selecting: bool, cx: &mut Context<Self>) {
        let Some(position) = self.layout_position_for_offset(self.cursor()) else {
            return;
        };
        let target_x = if end { px(1_000_000.) } else { px(0.) };
        let target = self.offset_for_layout_position(point(target_x, position.y));
        if selecting {
            self.select_to(target, cx);
        } else {
            self.move_to(target, cx);
        }
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.selection = 0..self.content.len();
        self.reversed = false;
        self.preferred_x = None;
        self.history.break_group();
        cx.notify();
    }

    fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        if self.selection.is_empty() {
            let cursor = self.cursor();
            self.selection = self.previous(cursor)..cursor;
            self.reversed = true;
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        if self.selection.is_empty() {
            let cursor = self.cursor();
            self.selection = cursor..self.next(cursor);
            self.reversed = false;
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    fn newline(&mut self, _: &Newline, window: &mut Window, cx: &mut Context<Self>) {
        self.history.break_group();
        self.replace_text_in_range(None, "\n", window, cx);
        self.history.break_group();
    }

    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.history.break_group();
            self.replace_text_in_range(None, &text, window, cx);
            self.history.break_group();
        }
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selection.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selection.clone()].to_string(),
            ));
        }
    }

    fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        if self.selection.is_empty() {
            return;
        }
        self.copy(&Copy, window, cx);
        self.history.break_group();
        self.replace_text_in_range(None, "", window, cx);
        self.history.break_group();
    }

    fn undo(&mut self, _: &Undo, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(snapshot) = self.history.undo(self.snapshot()) {
            self.restore(snapshot, cx);
        }
    }

    fn redo(&mut self, _: &Redo, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(snapshot) = self.history.redo(self.snapshot()) {
            self.restore(snapshot, cx);
        }
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus);
        self.is_selecting = true;
        self.history.break_group();
        let offset = self.index_for_mouse_position(event.position);
        if event.modifiers.shift {
            self.select_to(offset, cx);
        } else {
            self.move_to(offset, cx);
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.is_selecting = false;
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_selecting {
            let offset = self.index_for_mouse_position(event.position);
            self.select_to(offset, cx);
        }
    }

    fn index_for_mouse_position(&self, position: Point<Pixels>) -> usize {
        let Some(bounds) = self.last_bounds else {
            return 0;
        };
        if position.y <= bounds.top() {
            return self.offset_for_layout_position(point(px(0.), px(0.)));
        }
        if position.y >= bounds.bottom() {
            return self.content.len();
        }
        let local = point(
            position.x - bounds.left(),
            position.y - bounds.top() + self.line_height * self.first_visible_row as f32,
        );
        self.offset_for_layout_position(local)
    }

    fn layout_position_for_offset(&self, offset: usize) -> Option<Point<Pixels>> {
        let line = self.layout_line_for_offset(offset)?;
        let local = offset
            .saturating_sub(line.byte_start)
            .min(line.line.text.len());
        let position = line.line.position_for_index(local, self.line_height)?;
        Some(point(
            position.x,
            position.y + self.line_height * line.visual_start as f32,
        ))
    }

    fn layout_line_for_offset(&self, offset: usize) -> Option<&LayoutLine> {
        self.last_layout
            .iter()
            .find(|line| {
                let end = line.byte_start + line.line.text.len();
                offset >= line.byte_start && offset <= end
            })
            .or_else(|| self.last_layout.last())
    }

    fn offset_for_layout_position(&self, position: Point<Pixels>) -> usize {
        if self.last_layout.is_empty() {
            return 0;
        }
        if position.y < px(0.) {
            return 0;
        }
        let row = (position.y / self.line_height) as usize;
        let Some(line) = self
            .last_layout
            .iter()
            .find(|line| row >= line.visual_start && row < line.visual_start + line.visual_rows)
        else {
            return self.content.len();
        };
        let local_y = position.y - self.line_height * line.visual_start as f32;
        let local = line
            .line
            .closest_index_for_position(point(position.x, local_y), self.line_height)
            .unwrap_or_else(|index| index);
        (line.byte_start + local).min(self.content.len())
    }

    fn utf8_offset_from_utf16(&self, offset: usize) -> usize {
        utf8_offset_from_utf16(&self.content, offset)
    }

    fn utf16_offset_from_utf8(&self, offset: usize) -> usize {
        utf16_offset_from_utf8(&self.content, offset)
    }

    fn range_from_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.utf8_offset_from_utf16(range.start)..self.utf8_offset_from_utf16(range.end)
    }

    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.utf16_offset_from_utf8(range.start)..self.utf16_offset_from_utf8(range.end)
    }

    fn apply_replacement(&mut self, range: Range<usize>, text: &str, kind: EditKind) {
        if range.is_empty() && text.is_empty() {
            return;
        }
        self.history.record(self.snapshot(), kind);
        self.content =
            (self.content[..range.start].to_owned() + text + &self.content[range.end..]).into();
        let cursor = range.start + text.len();
        self.selection = cursor..cursor;
        self.reversed = false;
        self.preferred_x = None;
    }
}

impl Focusable for Composer {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl EntityInputHandler for Composer {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        actual: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range);
        actual.replace(self.range_to_utf16(&range));
        Some(self.content[range].to_string())
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.range_to_utf16(&self.selection),
            reversed: self.reversed,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked.as_ref().map(|range| self.range_to_utf16(range))
    }

    fn unmark_text(&mut self, _: &mut Window, _: &mut Context<Self>) {
        self.marked = None;
        self.history.break_group();
    }

    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range
            .as_ref()
            .map(|range| self.range_from_utf16(range))
            .or(self.marked.clone())
            .unwrap_or(self.selection.clone());
        let kind = if self.marked.is_some() {
            EditKind::Ime
        } else if range.is_empty() && !text.is_empty() && !text.contains('\n') {
            EditKind::Insert
        } else if text.is_empty() {
            EditKind::Delete
        } else {
            EditKind::Replace
        };
        self.apply_replacement(range, text, kind);
        self.marked = None;
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        selected: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range
            .as_ref()
            .map(|range| self.range_from_utf16(range))
            .or(self.marked.clone())
            .unwrap_or(self.selection.clone());
        self.apply_replacement(range.clone(), text, EditKind::Ime);
        self.marked = (!text.is_empty()).then_some(range.start..range.start + text.len());
        self.selection = selected
            .map(|selected| {
                utf8_offset_from_utf16(text, selected.start)
                    ..utf8_offset_from_utf16(text, selected.end)
            })
            .map(|selected| range.start + selected.start..range.start + selected.end)
            .unwrap_or(range.start + text.len()..range.start + text.len());
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range: Range<usize>,
        bounds: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let range = self.range_from_utf16(&range);
        let start = self.layout_position_for_offset(range.start)?;
        let end = self.layout_position_for_offset(range.end)?;
        let scroll_y = self.line_height * self.first_visible_row as f32;
        if start.y == end.y {
            Some(Bounds::from_corners(
                point(bounds.left() + start.x, bounds.top() + start.y - scroll_y),
                point(
                    bounds.left() + end.x.max(start.x + px(1.)),
                    bounds.top() + start.y - scroll_y + self.line_height,
                ),
            ))
        } else {
            Some(Bounds::from_corners(
                point(bounds.left(), bounds.top() + start.y - scroll_y),
                point(
                    bounds.right(),
                    bounds.top() + end.y - scroll_y + self.line_height,
                ),
            ))
        }
    }

    fn character_index_for_point(
        &mut self,
        position: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        Some(self.utf16_offset_from_utf8(self.index_for_mouse_position(position)))
    }
}

struct ComposerElement {
    input: Entity<Composer>,
}

struct Prepaint {
    lines: Vec<LayoutLine>,
    cursor: Option<PaintQuad>,
    selections: Vec<PaintQuad>,
    first_visible_row: usize,
    line_height: Pixels,
}

impl IntoElement for ComposerElement {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

impl Element for ComposerElement {
    type RequestLayoutState = ();
    type PrepaintState = Prepaint;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = px(60.).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Prepaint {
        let input = self.input.read(cx);
        let shown = if input.content.is_empty() {
            input.placeholder.clone()
        } else {
            input.content.clone()
        };
        let color = if input.content.is_empty() {
            gpui::hsla(0.62, 0.2, 0.62, 0.52)
        } else {
            window.text_style().color
        };
        let run = TextRun {
            len: shown.len(),
            font: window.text_style().font(),
            color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let runs = if let Some(marked) = input.marked.as_ref() {
            vec![
                TextRun {
                    len: marked.start,
                    ..run.clone()
                },
                TextRun {
                    len: marked.end - marked.start,
                    underline: Some(UnderlineStyle {
                        color: Some(run.color),
                        thickness: px(1.),
                        wavy: false,
                    }),
                    ..run.clone()
                },
                TextRun {
                    len: shown.len().saturating_sub(marked.end),
                    ..run
                },
            ]
            .into_iter()
            .filter(|run| run.len > 0)
            .collect()
        } else {
            vec![run]
        };
        let line_height = window.line_height().max(px(18.));
        let wrapped = window
            .text_system()
            .shape_text(
                shown,
                window.text_style().font_size.to_pixels(window.rem_size()),
                &runs,
                Some(bounds.size.width.max(px(1.))),
                None,
            )
            .unwrap_or_default();
        let mut byte_start = 0;
        let mut visual_start = 0;
        let mut lines = Vec::with_capacity(wrapped.len());
        for line in wrapped {
            let visual_rows = line.wrap_boundaries().len() + 1;
            let len = line.text.len();
            lines.push(LayoutLine {
                line,
                byte_start,
                visual_start,
                visual_rows,
            });
            byte_start += len + 1;
            visual_start += visual_rows;
        }

        let cursor_position = layout_position_for_offset(&lines, input.cursor(), line_height)
            .unwrap_or(point(px(0.), px(0.)));
        let cursor_row = (cursor_position.y / line_height) as usize;
        let visible_rows = ((bounds.size.height / line_height) as usize).max(1);
        let max_first = visual_start.saturating_sub(visible_rows);
        let mut first_visible_row = input.first_visible_row.min(max_first);
        if cursor_row < first_visible_row {
            first_visible_row = cursor_row;
        } else if cursor_row >= first_visible_row + visible_rows {
            first_visible_row = cursor_row + 1 - visible_rows;
        }
        let scroll_y = line_height * first_visible_row as f32;
        let cursor = input.selection.is_empty().then(|| {
            fill(
                Bounds::new(
                    point(
                        bounds.left() + cursor_position.x,
                        bounds.top() + cursor_position.y - scroll_y,
                    ),
                    size(px(1.5), line_height),
                ),
                rgba(0x9b87ffff),
            )
        });
        let selections = selection_quads(
            &lines,
            &input.selection,
            bounds,
            line_height,
            first_visible_row,
            visible_rows,
        );
        Prepaint {
            lines,
            cursor,
            selections,
            first_visible_row,
            line_height,
        }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        state: &mut Prepaint,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus = self.input.read(cx).focus.clone();
        window.handle_input(
            &focus,
            ElementInputHandler::new(bounds, self.input.clone()),
            cx,
        );
        for selection in state.selections.drain(..) {
            window.paint_quad(selection);
        }
        let visible_rows = ((bounds.size.height / state.line_height) as usize).max(1);
        let last_visible = state.first_visible_row + visible_rows;
        for line in &state.lines {
            if line.visual_start + line.visual_rows <= state.first_visible_row
                || line.visual_start >= last_visible
            {
                continue;
            }
            let y = state.line_height
                * (line.visual_start as isize - state.first_visible_row as isize) as f32;
            line.line
                .paint(
                    bounds.origin + point(px(0.), y),
                    state.line_height,
                    TextAlign::default(),
                    Some(bounds),
                    window,
                    cx,
                )
                .unwrap();
        }
        if focus.is_focused(window)
            && let Some(cursor) = state.cursor.take()
        {
            window.paint_quad(cursor);
        }
        self.input.update(cx, |input, _| {
            input.last_layout = state.lines.clone();
            input.last_bounds = Some(bounds);
            input.line_height = state.line_height;
            input.first_visible_row = state.first_visible_row;
        });
    }
}

impl Render for Composer {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .key_context("Composer")
            .overflow_hidden()
            .track_focus(&self.focus_handle(cx))
            .cursor(CursorStyle::IBeam)
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::up))
            .on_action(cx.listener(Self::down))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_up))
            .on_action(cx.listener(Self::select_down))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::select_home))
            .on_action(cx.listener(Self::select_end))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::newline))
            .on_action(cx.listener(Self::undo))
            .on_action(cx.listener(Self::redo))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .child(ComposerElement { input: cx.entity() })
    }
}

fn layout_position_for_offset(
    lines: &[LayoutLine],
    offset: usize,
    line_height: Pixels,
) -> Option<Point<Pixels>> {
    let line = lines
        .iter()
        .find(|line| {
            let end = line.byte_start + line.line.text.len();
            offset >= line.byte_start && offset <= end
        })
        .or_else(|| lines.last())?;
    let local = offset
        .saturating_sub(line.byte_start)
        .min(line.line.text.len());
    let position = line.line.position_for_index(local, line_height)?;
    Some(point(
        position.x,
        position.y + line_height * line.visual_start as f32,
    ))
}

fn selection_quads(
    lines: &[LayoutLine],
    selection: &Range<usize>,
    bounds: Bounds<Pixels>,
    line_height: Pixels,
    first_visible_row: usize,
    visible_rows: usize,
) -> Vec<PaintQuad> {
    if selection.is_empty() {
        return Vec::new();
    }
    let mut quads = Vec::new();
    let scroll_y = line_height * first_visible_row as f32;
    let last_visible = first_visible_row + visible_rows;
    for line in lines {
        let line_start = line.byte_start;
        let line_end = line_start + line.line.text.len();
        let start = selection.start.max(line_start).min(line_end);
        let end = selection.end.max(line_start).min(line_end);
        if start >= end {
            continue;
        }
        let Some(start_position) = line
            .line
            .position_for_index(start - line_start, line_height)
        else {
            continue;
        };
        let Some(end_position) = line.line.position_for_index(end - line_start, line_height) else {
            continue;
        };
        let start_row = line.visual_start + (start_position.y / line_height) as usize;
        let end_row = line.visual_start + (end_position.y / line_height) as usize;
        for row in start_row..=end_row {
            if row < first_visible_row || row >= last_visible {
                continue;
            }
            let left = if row == start_row {
                start_position.x
            } else {
                px(0.)
            };
            let right = if row == end_row {
                end_position.x.max(left + px(1.))
            } else {
                bounds.size.width
            };
            quads.push(fill(
                Bounds::new(
                    point(
                        bounds.left() + left,
                        bounds.top() + line_height * row as f32 - scroll_y,
                    ),
                    size((right - left).max(px(1.)), line_height),
                ),
                rgba(0x7868ff38),
            ));
        }
    }
    quads
}

fn utf8_offset_from_utf16(text: &str, offset: usize) -> usize {
    let mut utf8 = 0;
    let mut utf16 = 0;
    for ch in text.chars() {
        if utf16 >= offset {
            break;
        }
        utf16 += ch.len_utf16();
        utf8 += ch.len_utf8();
    }
    utf8
}

fn utf16_offset_from_utf8(text: &str, offset: usize) -> usize {
    let mut utf8 = 0;
    let mut utf16 = 0;
    for ch in text.chars() {
        if utf8 >= offset {
            break;
        }
        utf8 += ch.len_utf8();
        utf16 += ch.len_utf16();
    }
    utf16
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use gpui::SharedString;

    use super::{
        EDIT_GROUP_WINDOW, EditHistory, EditKind, EditorSnapshot, utf8_offset_from_utf16,
        utf16_offset_from_utf8,
    };

    fn snapshot(text: &str) -> EditorSnapshot {
        EditorSnapshot {
            content: SharedString::from(text.to_string()),
            selection: text.len()..text.len(),
            reversed: false,
        }
    }

    #[test]
    fn unicode_offsets_round_trip() {
        let text = "a🦀é";
        for utf8 in [0, 1, 5, 7] {
            let utf16 = utf16_offset_from_utf8(text, utf8);
            assert_eq!(utf8_offset_from_utf16(text, utf16), utf8);
        }
    }

    #[test]
    fn consecutive_typing_is_one_undo_group() {
        let mut history = EditHistory::default();
        history.record(snapshot(""), EditKind::Insert);
        history.record(snapshot("a"), EditKind::Insert);
        assert_eq!(history.undo.len(), 1);
        let restored = history.undo(snapshot("ab")).unwrap();
        assert_eq!(restored.content.as_ref(), "");
    }

    #[test]
    fn movement_or_elapsed_time_breaks_an_undo_group() {
        let mut history = EditHistory::default();
        history.record(snapshot(""), EditKind::Insert);
        history.break_group();
        history.record(snapshot("a"), EditKind::Insert);
        assert_eq!(history.undo.len(), 2);
        history.last_group = Some((
            EditKind::Insert,
            Instant::now() - EDIT_GROUP_WINDOW - EDIT_GROUP_WINDOW,
        ));
        history.record(snapshot("ab"), EditKind::Insert);
        assert_eq!(history.undo.len(), 3);
    }

    #[test]
    fn redo_restores_the_undone_snapshot() {
        let mut history = EditHistory::default();
        history.record(snapshot(""), EditKind::Replace);
        let previous = history.undo(snapshot("hello")).unwrap();
        assert_eq!(previous.content.as_ref(), "");
        let next = history.redo(previous).unwrap();
        assert_eq!(next.content.as_ref(), "hello");
    }
}
