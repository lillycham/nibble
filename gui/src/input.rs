//! A text field, and the same thing read-only for showing text that can be
//! selected. GPUI has no input widget of its own, so this is adapted from the
//! `input` example in the GPUI crate, which is copyright Zed Industries, Inc.
//! and licensed under the Apache License 2.0. A copy of that licence is in
//! gui/LICENSE-APACHE. The rest of nibble is MIT.
//!
//! The changes from the example: colours come from the caller, the demo view is
//! gone, and it has the editing keys a macOS text field has: movement and
//! deletion by word and to either end, the Emacs-style Control keys, undo,
//! and double- and triple-click selection. macOS gives those to its own text
//! fields for nothing; GPUI draws its own, so each one is written out here.
//! The text wraps, so the field grows a line at a time up to a limit and then
//! scrolls, and a field can take new lines (Shift-Return) or be read-only.

use std::ops::Range;

use gpui::{
    App, Bounds, ClipboardItem, ContentMask, Context, CursorStyle, DispatchPhase, ElementId, ElementInputHandler,
    Entity, EntityInputHandler, FocusHandle, Focusable, GlobalElementId, Hitbox, HitboxBehavior, Hsla, KeyBinding,
    LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point, ScrollWheelEvent,
    SharedString, Style, TextAlign, TextRun, UTF16Selection, UnderlineStyle, Window, WrappedLine, actions, div, fill,
    point, prelude::*, px, relative, size,
};
use unicode_segmentation::*;

actions!(
    text_input,
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
        SelectAll,
        Home,
        End,
        Top,
        Bottom,
        ShowCharacterPalette,
        Paste,
        Cut,
        Copy,
        WordLeft,
        WordRight,
        SelectWordLeft,
        SelectWordRight,
        SelectToStart,
        SelectToEnd,
        SelectToTop,
        SelectToBottom,
        DeleteWordLeft,
        DeleteWordRight,
        DeleteToStart,
        DeleteToEnd,
        Newline,
        Undo,
        Redo,
    ]
);

/// Where the word before `offset` starts.
fn previous_word(text: &str, offset: usize) -> usize {
    text.unicode_word_indices().rev().map(|(start, _)| start).find(|start| *start < offset).unwrap_or(0)
}

/// Where the word at or after `offset` ends.
fn next_word(text: &str, offset: usize) -> usize {
    text.unicode_word_indices().map(|(start, word)| start + word.len()).find(|end| *end > offset).unwrap_or(text.len())
}

/// The word, or the run of spaces or punctuation, that `offset` is in.
fn word_at(text: &str, offset: usize) -> Range<usize> {
    text.split_word_bound_indices()
        .map(|(start, piece)| start..start + piece.len())
        .find(|range| range.end > offset)
        .unwrap_or(text.len()..text.len())
}

/// Where the line that `offset` is on starts: the paragraph, not the row a
/// long line wraps onto, as with Control-A in a macOS text view.
fn line_start(text: &str, offset: usize) -> usize {
    text[..offset].rfind('\n').map_or(0, |at| at + 1)
}

/// Where the line that `offset` is on ends, before its newline.
fn line_end(text: &str, offset: usize) -> usize {
    text[offset..].find('\n').map_or(text.len(), |at| offset + at)
}

/// The text as last drawn: one wrapped line per line of text, each with the
/// offset it starts at, and where the first one was drawn, scrolling included.
struct Layout {
    lines: Vec<(usize, WrappedLine)>,
    line_height: Pixels,
    origin: Point<Pixels>,
    /// For a masked field, the text behind the dots that were drawn. Offsets
    /// in and out of the layout are offsets in that text, not in the dots.
    masked: Option<SharedString>,
}

/// What a masked field draws in place of each character.
const MASK: char = '•';

impl Layout {
    fn height(&self) -> Pixels {
        self.lines.iter().fold(px(0.), |height, (_, line)| height + line.size(self.line_height).height)
    }

    /// The top left of the character at `offset`, from the top left of the text.
    fn position_for(&self, offset: usize) -> Point<Pixels> {
        let offset = match &self.masked {
            Some(text) => text[..offset.min(text.len())].chars().count() * MASK.len_utf8(),
            None => offset,
        };
        let mut top = px(0.);
        for (n, (start, line)) in self.lines.iter().enumerate() {
            let last = n + 1 == self.lines.len();
            if offset <= start + line.len() || last {
                let local = offset.saturating_sub(*start).min(line.len());
                let at = line.position_for_index(local, self.line_height).unwrap_or(point(line.width(), px(0.)));
                return point(at.x, top + at.y);
            }
            top += line.size(self.line_height).height;
        }
        point(px(0.), px(0.))
    }

    /// The offset nearest to `position`, which is from the top left of the text.
    fn index_for(&self, position: Point<Pixels>) -> usize {
        let index = self.drawn_index_for(position);
        match &self.masked {
            Some(text) => text.char_indices().nth(index / MASK.len_utf8()).map_or(text.len(), |(at, _)| at),
            None => index,
        }
    }

    /// The same, as an offset in what was drawn.
    fn drawn_index_for(&self, position: Point<Pixels>) -> usize {
        if position.y < px(0.) {
            return 0;
        }
        let mut top = px(0.);
        for (start, line) in &self.lines {
            let height = line.size(self.line_height).height;
            if position.y < top + height {
                let local = point(position.x.max(px(0.)), position.y - top);
                let (Ok(index) | Err(index)) = line.closest_index_for_position(local, self.line_height);
                return start + index;
            }
            top += height;
        }
        self.lines.last().map_or(0, |(start, line)| start + line.len())
    }
}

pub struct TextInput {
    focus_handle: FocusHandle,
    content: SharedString,
    placeholder: SharedString,
    /// Colours for the placeholder, and for the cursor and selection.
    pub dim: Hsla,
    pub accent: Hsla,
    /// Whether Shift-Return and pasted text can put new lines in it.
    newlines: bool,
    /// How many lines tall the field may grow before it scrolls. None: as
    /// tall as its text, for read-only text in a page that scrolls itself.
    max_lines: Option<usize>,
    /// Text to select and copy, but not to change.
    read_only: bool,
    /// Drawn as dots, and never copied, for a secret.
    masked: bool,
    selected_range: Range<usize>,
    selection_reversed: bool,
    marked_range: Option<Range<usize>>,
    last_layout: Option<Layout>,
    is_selecting: bool,
    /// How far the text is scrolled up, and how far it can be.
    scroll_top: Pixels,
    max_scroll: Pixels,
    /// The cursor moved, so scroll it into view when next drawn.
    reveal: bool,
    /// Where Up and Down aim for, kept across a run of them so that the cursor
    /// doesn't drift left on its way past short lines.
    goal_x: Option<Pixels>,
    /// Earlier states to go back to, and undone ones to return to.
    undo: Vec<(SharedString, Range<usize>)>,
    redo: Vec<(SharedString, Range<usize>)>,
    /// The last edit was a typed character, so the next one joins its undo step.
    typing: bool,
}

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("backspace", Backspace, None),
        KeyBinding::new("delete", Delete, None),
        KeyBinding::new("left", Left, None),
        KeyBinding::new("right", Right, None),
        KeyBinding::new("up", Up, None),
        KeyBinding::new("down", Down, None),
        KeyBinding::new("shift-left", SelectLeft, None),
        KeyBinding::new("shift-right", SelectRight, None),
        KeyBinding::new("shift-up", SelectUp, None),
        KeyBinding::new("shift-down", SelectDown, None),
        KeyBinding::new("cmd-a", SelectAll, None),
        KeyBinding::new("cmd-v", Paste, None),
        KeyBinding::new("cmd-c", Copy, None),
        KeyBinding::new("cmd-x", Cut, None),
        KeyBinding::new("home", Home, None),
        KeyBinding::new("end", End, None),
        KeyBinding::new("cmd-left", Home, None),
        KeyBinding::new("cmd-right", End, None),
        KeyBinding::new("cmd-up", Top, None),
        KeyBinding::new("cmd-down", Bottom, None),
        KeyBinding::new("ctrl-cmd-space", ShowCharacterPalette, None),
        // Return sends; these start a new line, in the fields that take one.
        KeyBinding::new("shift-enter", Newline, Some("TextInput")),
        KeyBinding::new("alt-enter", Newline, Some("TextInput")),
        KeyBinding::new("ctrl-enter", Newline, Some("TextInput")),
        // By word and to either end, as in any macOS text field.
        KeyBinding::new("alt-left", WordLeft, None),
        KeyBinding::new("alt-right", WordRight, None),
        KeyBinding::new("alt-shift-left", SelectWordLeft, None),
        KeyBinding::new("alt-shift-right", SelectWordRight, None),
        KeyBinding::new("cmd-shift-left", SelectToStart, None),
        KeyBinding::new("cmd-shift-right", SelectToEnd, None),
        KeyBinding::new("cmd-shift-up", SelectToTop, None),
        KeyBinding::new("cmd-shift-down", SelectToBottom, None),
        KeyBinding::new("shift-home", SelectToStart, None),
        KeyBinding::new("shift-end", SelectToEnd, None),
        KeyBinding::new("alt-backspace", DeleteWordLeft, None),
        KeyBinding::new("alt-delete", DeleteWordRight, None),
        KeyBinding::new("cmd-backspace", DeleteToStart, None),
        KeyBinding::new("cmd-z", Undo, None),
        KeyBinding::new("cmd-shift-z", Redo, None),
        // The Emacs-style keys that macOS text fields also answer to.
        KeyBinding::new("ctrl-a", Home, None),
        KeyBinding::new("ctrl-e", End, None),
        KeyBinding::new("ctrl-b", Left, None),
        KeyBinding::new("ctrl-f", Right, None),
        KeyBinding::new("ctrl-p", Up, None),
        KeyBinding::new("ctrl-n", Down, None),
        KeyBinding::new("ctrl-d", Delete, None),
        KeyBinding::new("ctrl-h", Backspace, None),
        KeyBinding::new("ctrl-k", DeleteToEnd, None),
    ]);
}

impl TextInput {
    /// A field for a single value: it wraps a long one onto a few lines,
    /// but takes no new lines of its own.
    pub fn new(placeholder: &str, cx: &mut Context<Self>) -> Self {
        TextInput {
            focus_handle: cx.focus_handle(),
            content: "".into(),
            placeholder: placeholder.to_string().into(),
            dim: gpui::opaque_grey(0.5, 1.0),
            accent: gpui::blue(),
            newlines: false,
            max_lines: Some(4),
            read_only: false,
            masked: false,
            selected_range: 0..0,
            selection_reversed: false,
            marked_range: None,
            last_layout: None,
            is_selecting: false,
            scroll_top: px(0.),
            max_scroll: px(0.),
            reveal: false,
            goal_x: None,
            undo: Vec::new(),
            redo: Vec::new(),
            typing: false,
        }
    }

    /// A field for writing in: Shift-Return starts a new line, and it grows
    /// to `max_lines` before it scrolls.
    pub fn multiline(placeholder: &str, max_lines: usize, cx: &mut Context<Self>) -> Self {
        TextInput { newlines: true, max_lines: Some(max_lines), ..Self::new(placeholder, cx) }
    }

    /// Text that can be selected and copied, but not changed, as tall as it is.
    pub fn read_only(cx: &mut Context<Self>) -> Self {
        TextInput { newlines: true, max_lines: None, read_only: true, ..Self::new("", cx) }
    }

    /// A single-value field that shows a dot for each character, as a
    /// password field does.
    pub fn masked(placeholder: &str, cx: &mut Context<Self>) -> Self {
        TextInput { masked: true, ..Self::new(placeholder, cx) }
    }

    pub fn text(&self) -> String {
        self.content.to_string()
    }

    pub fn set_text(&mut self, text: &str, cx: &mut Context<Self>) {
        self.reset();
        self.content = text.to_string().into();
        self.selected_range = text.len()..text.len();
        self.reveal = true;
        cx.notify();
    }

    /// Change read-only text, as a reply grows, keeping what is selected.
    pub fn show(&mut self, text: &str, cx: &mut Context<Self>) {
        if self.content.as_ref() == text {
            return;
        }
        self.content = text.to_string().into();
        let clamp = |offset: usize| {
            let mut offset = offset.min(text.len());
            while !text.is_char_boundary(offset) {
                offset -= 1;
            }
            offset
        };
        self.selected_range = clamp(self.selected_range.start)..clamp(self.selected_range.end);
        cx.notify();
    }

    pub fn set_placeholder(&mut self, placeholder: &str) {
        self.placeholder = placeholder.to_string().into();
    }

    /// Empty the field and return what was in it.
    pub fn take(&mut self, cx: &mut Context<Self>) -> String {
        let text = self.content.to_string();
        self.reset();
        cx.notify();
        text
    }

    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.previous_boundary(self.cursor_offset()), cx);
        } else {
            self.move_to(self.selected_range.start, cx)
        }
    }

    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.next_boundary(self.selected_range.end), cx);
        } else {
            self.move_to(self.selected_range.end, cx)
        }
    }

    fn up(&mut self, _: &Up, _: &mut Window, cx: &mut Context<Self>) {
        let from = if self.selected_range.is_empty() { self.cursor_offset() } else { self.selected_range.start };
        let (offset, goal) = self.vertical(from, -1);
        self.move_to(offset, cx);
        self.goal_x = goal;
    }

    fn down(&mut self, _: &Down, _: &mut Window, cx: &mut Context<Self>) {
        let from = if self.selected_range.is_empty() { self.cursor_offset() } else { self.selected_range.end };
        let (offset, goal) = self.vertical(from, 1);
        self.move_to(offset, cx);
        self.goal_x = goal;
    }

    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.previous_boundary(self.cursor_offset()), cx);
    }

    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.next_boundary(self.cursor_offset()), cx);
    }

    fn select_up(&mut self, _: &SelectUp, _: &mut Window, cx: &mut Context<Self>) {
        let (offset, goal) = self.vertical(self.cursor_offset(), -1);
        self.select_to(offset, cx);
        self.goal_x = goal;
    }

    fn select_down(&mut self, _: &SelectDown, _: &mut Window, cx: &mut Context<Self>) {
        let (offset, goal) = self.vertical(self.cursor_offset(), 1);
        self.select_to(offset, cx);
        self.goal_x = goal;
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
        self.select_to(self.content.len(), cx)
    }

    fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(line_start(&self.content, self.cursor_offset()), cx);
    }

    fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(line_end(&self.content, self.cursor_offset()), cx);
    }

    fn top(&mut self, _: &Top, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
    }

    fn bottom(&mut self, _: &Bottom, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.content.len(), cx);
    }

    fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.select_to(self.previous_boundary(self.cursor_offset()), cx)
        }
        self.replace_text_in_range(None, "", window, cx)
    }

    fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.select_to(self.next_boundary(self.cursor_offset()), cx)
        }
        self.replace_text_in_range(None, "", window, cx)
    }

    fn word_left(&mut self, _: &WordLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(previous_word(&self.content, self.cursor_offset()), cx);
    }

    fn word_right(&mut self, _: &WordRight, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(next_word(&self.content, self.cursor_offset()), cx);
    }

    fn select_word_left(&mut self, _: &SelectWordLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(previous_word(&self.content, self.cursor_offset()), cx);
    }

    fn select_word_right(&mut self, _: &SelectWordRight, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(next_word(&self.content, self.cursor_offset()), cx);
    }

    fn select_to_start(&mut self, _: &SelectToStart, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(line_start(&self.content, self.cursor_offset()), cx);
    }

    fn select_to_end(&mut self, _: &SelectToEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(line_end(&self.content, self.cursor_offset()), cx);
    }

    fn select_to_top(&mut self, _: &SelectToTop, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(0, cx);
    }

    fn select_to_bottom(&mut self, _: &SelectToBottom, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.content.len(), cx);
    }

    /// Delete from the cursor to `offset`, or the selection if there is one.
    fn delete_to(&mut self, offset: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.select_to(offset, cx);
        }
        self.replace_text_in_range(None, "", window, cx)
    }

    fn delete_word_left(&mut self, _: &DeleteWordLeft, window: &mut Window, cx: &mut Context<Self>) {
        self.delete_to(previous_word(&self.content, self.cursor_offset()), window, cx);
    }

    fn delete_word_right(&mut self, _: &DeleteWordRight, window: &mut Window, cx: &mut Context<Self>) {
        self.delete_to(next_word(&self.content, self.cursor_offset()), window, cx);
    }

    fn delete_to_start(&mut self, _: &DeleteToStart, window: &mut Window, cx: &mut Context<Self>) {
        self.delete_to(line_start(&self.content, self.cursor_offset()), window, cx);
    }

    /// Control-K: to the end of the line, or the newline itself when the
    /// cursor is already there, as in a macOS text view.
    fn delete_to_end(&mut self, _: &DeleteToEnd, window: &mut Window, cx: &mut Context<Self>) {
        let cursor = self.cursor_offset();
        let end = line_end(&self.content, cursor);
        let end = if end == cursor { self.next_boundary(cursor) } else { end };
        self.delete_to(end, window, cx);
    }

    fn newline(&mut self, _: &Newline, window: &mut Window, cx: &mut Context<Self>) {
        if self.newlines && !self.read_only {
            self.replace_text_in_range(None, "\n", window, cx);
        } else {
            cx.propagate();
        }
    }

    fn undo(&mut self, _: &Undo, _: &mut Window, cx: &mut Context<Self>) {
        if let Some((content, selection)) = self.undo.pop() {
            self.redo.push((std::mem::replace(&mut self.content, content), self.selected_range.clone()));
            self.selected_range = selection;
            self.typing = false;
            self.reveal = true;
            cx.notify();
        }
    }

    fn redo(&mut self, _: &Redo, _: &mut Window, cx: &mut Context<Self>) {
        if let Some((content, selection)) = self.redo.pop() {
            self.undo.push((std::mem::replace(&mut self.content, content), self.selected_range.clone()));
            self.selected_range = selection;
            self.typing = false;
            self.reveal = true;
            cx.notify();
        }
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.is_selecting = true;
        let offset = self.index_for_mouse_position(event.position);

        if event.click_count >= 3 {
            self.selection_reversed = false;
            self.selected_range = line_start(&self.content, offset)..line_end(&self.content, offset);
            cx.notify();
        } else if event.click_count == 2 {
            self.selection_reversed = false;
            self.selected_range = word_at(&self.content, offset);
            cx.notify();
        } else if event.modifiers.shift {
            self.select_to(offset, cx);
        } else {
            self.move_to(offset, cx)
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _window: &mut Window, _: &mut Context<Self>) {
        self.is_selecting = false;
    }

    /// Any move while the button is down, over the field or not, so that a
    /// drag past its edge still selects to the start or end.
    fn on_drag(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        if !self.is_selecting {
            return;
        }
        if event.pressed_button != Some(MouseButton::Left) {
            self.is_selecting = false;
            return;
        }
        self.select_to(self.index_for_mouse_position(event.position), cx);
    }

    /// The wheel or trackpad, over a field that has more lines than it shows.
    /// Returns whether it moved, so that at either end the page scrolls instead.
    fn on_scroll(&mut self, event: &ScrollWheelEvent, cx: &mut Context<Self>) -> bool {
        let line_height = self.last_layout.as_ref().map_or(px(16.), |layout| layout.line_height);
        let delta = event.delta.pixel_delta(line_height);
        let scroll_top = (self.scroll_top - delta.y).clamp(px(0.), self.max_scroll);
        if scroll_top == self.scroll_top {
            return false;
        }
        self.scroll_top = scroll_top;
        cx.notify();
        true
    }

    fn show_character_palette(
        &mut self,
        _: &ShowCharacterPalette,
        window: &mut Window,
        _: &mut Context<Self>,
    ) {
        window.show_character_palette();
    }

    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            let text = text.replace("\r\n", "\n");
            let text = if self.newlines { text } else { text.replace('\n', " ") };
            self.replace_text_in_range(None, &text, window, cx);
        }
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() && !self.masked {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
        }
    }
    fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() && !self.read_only && !self.masked {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
            self.replace_text_in_range(None, "", window, cx)
        }
    }

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.selected_range = offset..offset;
        self.selection_reversed = false;
        self.reveal = true;
        self.goal_x = None;
        cx.notify()
    }

    fn cursor_offset(&self) -> usize {
        if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        }
    }

    /// The offset a line above (`rows` -1) or below (1) `from`, and the x it
    /// aimed for. Past the first or last line, the start or end of the text.
    fn vertical(&self, from: usize, rows: i32) -> (usize, Option<Pixels>) {
        let Some(layout) = self.last_layout.as_ref().filter(|_| !self.content.is_empty()) else {
            return (if rows < 0 { 0 } else { self.content.len() }, None);
        };
        let at = layout.position_for(from);
        let x = self.goal_x.unwrap_or(at.x);
        let y = at.y + layout.line_height * (rows as f32 + 0.5);
        if y < px(0.) {
            return (0, Some(x));
        }
        if y >= layout.height() {
            return (self.content.len(), Some(x));
        }
        (layout.index_for(point(x, y)), Some(x))
    }

    fn index_for_mouse_position(&self, position: Point<Pixels>) -> usize {
        if self.content.is_empty() {
            return 0;
        }
        let Some(layout) = self.last_layout.as_ref() else {
            return 0;
        };
        layout.index_for(position - layout.origin).min(self.content.len())
    }

    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        if self.selection_reversed {
            self.selected_range.start = offset
        } else {
            self.selected_range.end = offset
        };
        if self.selected_range.end < self.selected_range.start {
            self.selection_reversed = !self.selection_reversed;
            self.selected_range = self.selected_range.end..self.selected_range.start;
        }
        self.reveal = true;
        self.goal_x = None;
        cx.notify()
    }

    fn offset_from_utf16(&self, offset: usize) -> usize {
        let mut utf8_offset = 0;
        let mut utf16_count = 0;

        for ch in self.content.chars() {
            if utf16_count >= offset {
                break;
            }
            utf16_count += ch.len_utf16();
            utf8_offset += ch.len_utf8();
        }

        utf8_offset
    }

    fn offset_to_utf16(&self, offset: usize) -> usize {
        let mut utf16_offset = 0;
        let mut utf8_count = 0;

        for ch in self.content.chars() {
            if utf8_count >= offset {
                break;
            }
            utf8_count += ch.len_utf8();
            utf16_offset += ch.len_utf16();
        }

        utf16_offset
    }

    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    fn range_from_utf16(&self, range_utf16: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(range_utf16.start)..self.offset_from_utf16(range_utf16.end)
    }

    fn previous_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .rev()
            .find_map(|(idx, _)| (idx < offset).then_some(idx))
            .unwrap_or(0)
    }

    fn next_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .find_map(|(idx, _)| (idx > offset).then_some(idx))
            .unwrap_or(self.content.len())
    }

    fn reset(&mut self) {
        self.content = "".into();
        self.selected_range = 0..0;
        self.selection_reversed = false;
        self.marked_range = None;
        self.last_layout = None;
        self.is_selecting = false;
        self.scroll_top = px(0.);
        self.goal_x = None;
        self.undo.clear();
        self.redo.clear();
        self.typing = false;
    }

    /// What to draw, the placeholder when there is no text, and its runs:
    /// text that an input method is still composing is underlined.
    fn display(&self, style: &gpui::TextStyle) -> (SharedString, Vec<TextRun>) {
        let (display_text, text_color) = if self.content.is_empty() {
            (self.placeholder.clone(), self.dim)
        } else if self.masked {
            (MASK.to_string().repeat(self.content.chars().count()).into(), style.color)
        } else {
            (self.content.clone(), style.color)
        };

        let run = TextRun {
            len: display_text.len(),
            font: style.font(),
            color: text_color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        // Composed text is underlined where it is, which dots would give away.
        let runs = if let Some(marked_range) = self.marked_range.as_ref().filter(|_| !self.masked) {
            vec![
                TextRun {
                    len: marked_range.start,
                    ..run.clone()
                },
                TextRun {
                    len: marked_range.end - marked_range.start,
                    underline: Some(UnderlineStyle {
                        color: Some(run.color),
                        thickness: px(1.0),
                        wavy: false,
                    }),
                    ..run.clone()
                },
                TextRun {
                    len: display_text.len() - marked_range.end,
                    ..run
                },
            ]
            .into_iter()
            .filter(|run| run.len > 0)
            .collect()
        } else {
            vec![run]
        };
        (display_text, runs)
    }
}

impl EntityInputHandler for TextInput {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range_utf16);
        actual_range.replace(self.range_to_utf16(&range));
        Some(self.content[range].to_string())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.range_to_utf16(&self.selected_range),
            reversed: self.selection_reversed,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.marked_range
            .as_ref()
            .map(|range| self.range_to_utf16(range))
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.marked_range = None;
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.read_only {
            return;
        }
        let range = range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());

        // One undo step per run of typing, and one per other edit. Text that
        // an input method is still composing is not a step of its own.
        let typed = range.is_empty() && new_text.chars().count() == 1 && !new_text.contains([' ', '\n']);
        if self.marked_range.is_none() && !(typed && self.typing) {
            self.undo.push((self.content.clone(), self.selected_range.clone()));
            self.undo.drain(..self.undo.len().saturating_sub(100));
        }
        self.redo.clear();
        self.typing = typed;

        self.content =
            (self.content[0..range.start].to_owned() + new_text + &self.content[range.end..])
                .into();
        self.selected_range = range.start + new_text.len()..range.start + new_text.len();
        self.selection_reversed = false;
        self.marked_range.take();
        self.reveal = true;
        self.goal_x = None;
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.read_only {
            return;
        }
        let range = range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());

        self.content =
            (self.content[0..range.start].to_owned() + new_text + &self.content[range.end..])
                .into();
        if !new_text.is_empty() {
            self.marked_range = Some(range.start..range.start + new_text.len());
        } else {
            self.marked_range = None;
        }
        self.selected_range = new_selected_range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .map(|new_range| new_range.start + range.start..new_range.end + range.end)
            .unwrap_or_else(|| range.start + new_text.len()..range.start + new_text.len());
        self.reveal = true;

        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        _bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let layout = self.last_layout.as_ref()?;
        let range = self.range_from_utf16(&range_utf16);
        let (start, end) = (layout.position_for(range.start), layout.position_for(range.end));
        // On one row, the range itself; across rows, where it starts is enough
        // to put the input method's window next to it.
        let width = if start.y == end.y { end.x - start.x } else { px(0.) };
        Some(Bounds::new(layout.origin + start, size(width, layout.line_height)))
    }

    fn character_index_for_point(
        &mut self,
        point: gpui::Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        self.last_layout.as_ref()?;
        Some(self.offset_to_utf16(self.index_for_mouse_position(point)))
    }
}

struct TextElement {
    input: Entity<TextInput>,
}

struct PrepaintState {
    layout: Option<Layout>,
    cursor: Option<PaintQuad>,
    selection: Vec<PaintQuad>,
    /// Set when the field scrolls, for the wheel to know it is over it.
    hitbox: Option<Hitbox>,
}

impl IntoElement for TextElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TextElement {
    type RequestLayoutState = ();
    type PrepaintState = PrepaintState;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let input = self.input.read(cx);
        let style = window.text_style();
        let (text, runs) = input.display(&style);
        let max_lines = input.max_lines;
        let font_size = style.font_size.to_pixels(window.rem_size());
        let line_height = window.line_height();

        let mut layout_style = Style::default();
        layout_style.size.width = relative(1.).into();
        // As tall as the text once wrapped to the width it is given, one line
        // at least, and no more than `max_lines`.
        let layout_id = window.request_measured_layout(layout_style, move |known, available, window, _| {
            let width = known.width.or(match available.width {
                gpui::AvailableSpace::Definite(width) => Some(width),
                _ => None,
            });
            let lines = window.text_system().shape_text(text.clone(), font_size, &runs, width, None).unwrap_or_default();
            let rows: usize = lines.iter().map(|line| line.wrap_boundaries().len() + 1).sum();
            let rows = rows.max(1).min(max_lines.unwrap_or(usize::MAX));
            // It fills the width it is given and never asks for more, or a
            // long line would push the field past the edge of a row.
            size(width.unwrap_or_default(), line_height * rows as f32)
        });
        (layout_id, ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let input = self.input.read(cx);
        let selected_range = input.selected_range.clone();
        let cursor = input.cursor_offset();
        let accent = input.accent;
        let masked = (input.masked && !input.content.is_empty()).then(|| input.content.clone());
        let style = window.text_style();
        let (text, runs) = input.display(&style);
        let font_size = style.font_size.to_pixels(window.rem_size());
        let line_height = window.line_height();

        let shaped = window
            .text_system()
            .shape_text(text, font_size, &runs, Some(bounds.size.width), None)
            .unwrap_or_default();
        let mut start = 0;
        let lines = shaped
            .into_iter()
            .map(|line| {
                let at = start;
                start += line.len() + 1;
                (at, line)
            })
            .collect();
        let mut layout = Layout { lines, line_height, origin: bounds.origin, masked };

        // Scroll so that the cursor is in view, if it has moved.
        let max_scroll = (layout.height() - bounds.size.height).max(px(0.));
        let mut scroll_top = input.scroll_top.min(max_scroll);
        if input.reveal {
            let at = layout.position_for(cursor);
            if at.y < scroll_top {
                scroll_top = at.y;
            } else if at.y + line_height > scroll_top + bounds.size.height {
                scroll_top = (at.y + line_height - bounds.size.height).min(max_scroll);
            }
        }
        layout.origin = bounds.origin - point(px(0.), scroll_top);
        let origin = layout.origin;

        let cursor_quad = selected_range.is_empty().then(|| {
            let at = layout.position_for(cursor);
            fill(Bounds::new(origin + at, size(px(2.), line_height)), accent)
        });
        // A selection over several rows runs to the right edge on each but
        // the last, and from the left edge on each but the first.
        let mut selection = Vec::new();
        if !selected_range.is_empty() {
            let (from, to) = (layout.position_for(selected_range.start), layout.position_for(selected_range.end));
            let right = bounds.size.width;
            let mut band = |left: Pixels, top: Pixels, right: Pixels, bottom: Pixels| {
                if right > left && bottom > top {
                    let corners = (origin + point(left, top), origin + point(right, bottom));
                    selection.push(fill(Bounds::from_corners(corners.0, corners.1), accent.opacity(0.25)));
                }
            };
            if from.y == to.y {
                band(from.x, from.y, to.x, from.y + line_height);
            } else {
                band(from.x, from.y, right, from.y + line_height);
                band(px(0.), from.y + line_height, right, to.y);
                band(px(0.), to.y, to.x, to.y + line_height);
            }
        }

        let hitbox = (max_scroll > px(0.)).then(|| window.insert_hitbox(bounds, HitboxBehavior::Normal));
        self.input.update(cx, |input, _| {
            input.scroll_top = scroll_top;
            input.max_scroll = max_scroll;
            input.reveal = false;
        });
        PrepaintState { layout: Some(layout), cursor: cursor_quad, selection, hitbox }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let (focus_handle, read_only) = {
            let input = self.input.read(cx);
            (input.focus_handle.clone(), input.read_only)
        };
        let focused = focus_handle.is_focused(window);
        if !read_only {
            window.handle_input(
                &focus_handle,
                ElementInputHandler::new(bounds, self.input.clone()),
                cx,
            );
        }

        let input = self.input.clone();
        window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, cx| {
            if phase == DispatchPhase::Bubble {
                input.update(cx, |input, cx| input.on_drag(event, cx));
            }
        });
        if let Some(hitbox) = prepaint.hitbox.take() {
            let input = self.input.clone();
            window.on_mouse_event(move |event: &ScrollWheelEvent, phase, window, cx| {
                if phase == DispatchPhase::Bubble
                    && hitbox.is_hovered(window)
                    && input.update(cx, |input, cx| input.on_scroll(event, cx))
                {
                    cx.stop_propagation();
                }
            });
        }

        let layout = prepaint.layout.take().unwrap();
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            // Read-only text shows its selection only while it has the focus,
            // so that one stays lit when another reply is clicked.
            if focused || !read_only {
                for selection in prepaint.selection.drain(..) {
                    window.paint_quad(selection)
                }
            }
            let mut top = layout.origin.y;
            for (_, line) in &layout.lines {
                line.paint(point(layout.origin.x, top), layout.line_height, TextAlign::Left, None, window, cx)
                    .unwrap();
                top += line.size(layout.line_height).height;
            }
            if focused
                && !read_only
                && let Some(cursor) = prepaint.cursor.take()
            {
                window.paint_quad(cursor);
            }
        });

        self.input.update(cx, |input, _cx| {
            input.last_layout = Some(layout);
        });
    }
}

impl Render for TextInput {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .key_context("TextInput")
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
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::top))
            .on_action(cx.listener(Self::bottom))
            .on_action(cx.listener(Self::show_character_palette))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::word_left))
            .on_action(cx.listener(Self::word_right))
            .on_action(cx.listener(Self::select_word_left))
            .on_action(cx.listener(Self::select_word_right))
            .on_action(cx.listener(Self::select_to_start))
            .on_action(cx.listener(Self::select_to_end))
            .on_action(cx.listener(Self::select_to_top))
            .on_action(cx.listener(Self::select_to_bottom))
            .on_action(cx.listener(Self::delete_word_left))
            .on_action(cx.listener(Self::delete_word_right))
            .on_action(cx.listener(Self::delete_to_start))
            .on_action(cx.listener(Self::delete_to_end))
            .on_action(cx.listener(Self::newline))
            .on_action(cx.listener(Self::undo))
            .on_action(cx.listener(Self::redo))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .w_full()
            .child(TextElement { input: cx.entity() })
    }
}

impl Focusable for TextInput {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_are_found_the_way_a_text_field_finds_them() {
        let text = "read the file, s\u{2019}il vous pla\u{ee}t";
        //          0    5   9
        assert_eq!(previous_word(text, 9), 5);
        assert_eq!(previous_word(text, 7), 5);
        assert_eq!(previous_word(text, 5), 0);
        assert_eq!(previous_word(text, 0), 0);
        assert_eq!(next_word(text, 0), 4);
        assert_eq!(next_word(text, 4), 8);
        // Past the comma and the space to the end of the next word.
        assert_eq!(&text[..next_word(text, 13)], "read the file, s\u{2019}il");
        assert_eq!(next_word(text, text.len()), text.len());
        // A double click takes the word under it, or the gap between words.
        assert_eq!(&text[word_at(text, 6)], "the");
        assert_eq!(&text[word_at(text, 4)], " ");
        assert_eq!(word_at(text, text.len()), text.len()..text.len());
    }

    #[test]
    fn lines_end_at_newlines_not_where_they_wrap() {
        let text = "first line\nsecond\n\nlast";
        //          0         10 11    17 18 19
        assert_eq!(line_start(text, 0), 0);
        assert_eq!(line_start(text, 5), 0);
        assert_eq!(line_start(text, 10), 0);
        assert_eq!(line_start(text, 11), 11);
        assert_eq!(line_start(text, 14), 11);
        assert_eq!(line_start(text, 18), 18);
        assert_eq!(line_start(text, text.len()), 19);
        assert_eq!(line_end(text, 0), 10);
        assert_eq!(line_end(text, 10), 10);
        assert_eq!(line_end(text, 11), 17);
        assert_eq!(line_end(text, 18), 18);
        assert_eq!(line_end(text, 19), text.len());
        // With no newline in it, a line is the whole text.
        assert_eq!(line_start("one line", 4), 0);
        assert_eq!(line_end("one line", 4), 8);
    }
}
