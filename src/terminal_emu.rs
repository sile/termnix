//! VTE performer that mutates [`TerminalState`](crate::terminal::TerminalState).

use unicode_width::UnicodeWidthChar;
use vte::Perform;

use crate::terminal::{DefaultColors, TerminalState};
use crate::terminal_buffer::Screen;
use crate::terminal_types::{
    ChildRequest, ClipboardSelection, Color, MouseReporting, Position, SavedCursor, Style,
    TerminalModes, indexed_rgb,
};

pub(crate) struct Emulator<'a> {
    pub(crate) term: &'a mut TerminalState,
}

impl Perform for Emulator<'_> {
    fn print(&mut self, c: char) {
        self.term.print_char(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            0x08 => self.term.backspace(),
            0x09 => self.term.horizontal_tab(),
            0x0a..=0x0c => self.term.line_feed(),
            0x0d => self.term.carriage_return(),
            0x07 => self.term.ring_bell(),
            _ => {}
        }
    }

    fn hook(&mut self, _params: &vte::Params, _intermediates: &[u8], _ignore: bool, _action: char) {
    }

    fn put(&mut self, _byte: u8) {}

    fn unhook(&mut self) {}

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        // OSC 0 / 2 store the window title; OSC 4 and 10-12 set colour state
        // and answer a `?` query from it; OSC 52 records a clipboard request.
        // Every other identifier is unmodelled and offered to the caller as
        // `ChildRequest::OtherOsc`, so its payload neither becomes printable
        // text nor is silently lost.
        // xterm OSC catalogue: https://invisible-island.net/xterm/ctlseqs/ctlseqs.html
        // (OSC identifiers evolve; termnix reads title text, colour state, and
        // OSC 52 selection data, and passes the rest through.)
        let Some((&id, rest)) = params.split_first() else {
            return;
        };
        // Match on the identifier's bytes, not on a decoded string: the known
        // identifiers are ASCII, so a byte match is exact, and an identifier
        // that is not valid UTF-8 must still reach the passthrough arm rather
        // than being dropped before it. (A `str::from_utf8` gate here would
        // discard exactly the vendor extensions passthrough exists for.)
        match id {
            b"0" | b"2" => {
                self.term.title = rest
                    .first()
                    .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
                    .unwrap_or_default();
                self.term.title_changed = true;
                self.term.events.mark_title_updated();
            }
            b"52" => self.osc_clipboard(params),
            // OSC 4 redefines a palette entry; OSC 10/11/12 set the default
            // foreground, background, and cursor colours. All four are state
            // a later paint reads, and all four answer a `?` query from that
            // state, so they are handled here rather than passed through.
            b"4" => self.osc_palette(params),
            b"10" => self.osc_default_color(params, id, |colors| &mut colors.foreground),
            b"11" => self.osc_default_color(params, id, |colors| &mut colors.background),
            b"12" => self.osc_default_color(params, id, |colors| &mut colors.cursor),
            _ => self.other_osc(id, rest),
        }
    }

    fn csi_dispatch(
        &mut self,
        params: &vte::Params,
        intermediates: &[u8],
        ignore: bool,
        action: char,
    ) {
        if ignore {
            return;
        }
        self.term.handle_csi(params, intermediates, action);
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], ignore: bool, byte: u8) {
        if ignore || !intermediates.is_empty() {
            return;
        }
        match byte {
            b'7' => self.term.save_cursor(),
            b'8' => self.term.restore_cursor(),
            b'D' => self.term.line_feed(),
            b'E' => {
                self.term.carriage_return();
                self.term.line_feed();
            }
            b'M' => self.term.reverse_index(),
            b'c' => self.term.reset_child_state(),
            _ => {}
        }
    }
}

impl Emulator<'_> {
    /// Records an OSC 52 clipboard message (`ESC ] 52 ; <Pc> ; <Pd> ST`).
    ///
    /// A set (`<Pd>` being base64, possibly `+`-prefixed) is pushed onto the
    /// caller's request queue as [`ChildRequest::SetClipboard`]; a read
    /// (`<Pd>` being `?`) is pushed as [`ChildRequest::GetClipboard`]. termnix
    /// holds no clipboard, so it can neither apply a set nor answer a read; in
    /// both cases the ask is all it can keep.
    ///
    /// `vte` splits OSC parameters on `;`, so a payload containing `;` cannot
    /// reach here intact; that is a property of the protocol's framing rather
    /// than a check made below.
    fn osc_clipboard(&mut self, params: &[&[u8]]) {
        let selection = match params.get(1) {
            // A missing selection means the system clipboard per xterm.
            None | Some(&[]) => ClipboardSelection::Clipboard,
            Some(bytes) => match *bytes {
                b"c" => ClipboardSelection::Clipboard,
                b"p" => ClipboardSelection::Primary,
                other => ClipboardSelection::Other(other.to_vec()),
            },
        };

        let payload = params.get(2).copied().unwrap_or(b"");
        // `?` is the one payload that points the other way: a read asks the
        // caller for a selection's contents instead of telling it what to
        // store. Both directions stay on the request channel, in the order the
        // child wrote them.
        if payload == b"?" {
            self.term
                .events
                .push_request(ChildRequest::GetClipboard { selection });
            return;
        }
        let (append, encoded) = match payload.split_first() {
            Some((b'+', rest)) => (true, rest),
            _ => (false, payload),
        };
        let Some(text) = decode_base64(encoded) else {
            return;
        };

        self.term.events.push_request(ChildRequest::SetClipboard {
            text,
            selection,
            append,
        });
    }

    /// Offers an OSC identifier termnix does not interpret to the caller.
    ///
    /// `id` is the identifier field and `rest` the argument fields, both as
    /// the tokenizer framed them. Nothing is decoded: the crate knows where an
    /// OSC's fields end and stops there, leaving the meaning of the arguments
    /// (a URI, a mark code, a vendor payload) to a caller that knows the
    /// number. An empty argument list stays empty rather than being filled
    /// with a placeholder, which is the honest answer for `ESC ] 7 ST`.
    fn other_osc(&mut self, id: &[u8], rest: &[&[u8]]) {
        self.term.events.push_request(ChildRequest::OtherOsc {
            id: id.to_vec(),
            params: rest.iter().map(|param| param.to_vec()).collect(),
        });
    }

    /// Handles an OSC 4 palette message (`ESC ] 4 ; <index> ; <spec> ST`).
    ///
    /// One sequence carries any number of `index ; spec` pairs, and the pairs
    /// are independent: a set (`<spec>` being `rgb:...`) stores an override,
    /// while a query (`<spec>` being `?`) answers from the state the terminal
    /// holds - the child's override if it set one, otherwise the built-in
    /// default for that index. A malformed pair stores nothing and answers
    /// nothing, the same silent ignore an undecodable OSC 52 payload gets.
    ///
    /// A query is answered on the reply buffer, not the event channel: the
    /// answer is the terminal's own state, which the caller has nothing to do
    /// with producing. An odd trailing field (an index with no spec) is
    /// ignored rather than read as a value.
    fn osc_palette(&mut self, params: &[&[u8]]) {
        // Each turn of the loop peels one `index ; spec` pair off the front.
        // The pattern needs two leading fields, so an odd trailing field ends
        // the loop instead of being read as a value.
        let mut rest = params.get(1..).unwrap_or_default();
        while let [key, spec, tail @ ..] = rest {
            rest = tail;
            let Some(index) = parse_osc_index(key) else {
                continue;
            };
            if *spec == b"?" {
                let (r, g, b) =
                    self.term.palette[index as usize].unwrap_or_else(|| indexed_rgb(index));
                let reply = format!("\x1b]4;{index};rgb:{r:02x}/{g:02x}/{b:02x}\x1b\\");
                self.term.replies.push_bounded(reply.as_bytes());
                continue;
            }
            if let Some(rgb) = parse_osc_rgb(spec) {
                self.term.palette[index as usize] = Some(rgb);
            }
        }
    }

    /// Handles an OSC 10, 11, or 12 message (`ESC ] <n> ; <spec> ST`).
    ///
    /// Unlike OSC 4 these carry no index: the identifier selects one of three
    /// slots, which the caller hands in as a projection so the three arms above
    /// share this body. A set stores the colour; a query answers only when the
    /// slot holds one, because a default colour the child never set is the
    /// *host's*, and the crate has no true answer to give. An absent or
    /// malformed value stores nothing (xterm defines no reset form here).
    fn osc_default_color<F>(&mut self, params: &[&[u8]], id: &[u8], slot: F)
    where
        F: FnOnce(&mut DefaultColors) -> &mut Option<(u8, u8, u8)>,
    {
        let Some(spec) = params.get(1) else {
            return;
        };
        if *spec == b"?" {
            let Some((r, g, b)) = *slot(&mut self.term.default_colors) else {
                return;
            };
            let id = String::from_utf8_lossy(id);
            let reply = format!("\x1b]{id};rgb:{r:02x}/{g:02x}/{b:02x}\x1b\\");
            self.term.replies.push_bounded(reply.as_bytes());
            return;
        }
        if let Some(rgb) = parse_osc_rgb(spec) {
            *slot(&mut self.term.default_colors) = Some(rgb);
        }
    }
}

impl TerminalState {
    fn print_char(&mut self, ch: char) {
        let Some(width) = char_display_width(ch) else {
            return;
        };
        if width == 0 {
            return;
        }

        if self.wrap_pending {
            self.wrap_pending = false;
            if self.modes.autowrap {
                self.carriage_return();
                self.line_feed();
            }
        }

        if self.cursor.col as usize + width > self.size.cols.get() as usize {
            if self.modes.autowrap {
                self.carriage_return();
                self.line_feed();
            } else {
                self.cursor.col = self.size.cols.get() - 1;
            }
        }

        if self.cursor.col as usize + width > self.size.cols.get() as usize {
            return;
        }

        if self.modes.insert {
            let row = self.cursor.row;
            let col = self.cursor.col;
            let style = self.pen;
            self.active_mut()
                .insert_columns(row, col, width as u16, style);
        }

        let row = self.cursor.row;
        let col = self.cursor.col;
        let style = self.pen;
        self.active_mut()
            .put_glyph(row, col, ch, width as u8, style);

        let next_col = col + width as u16;
        if next_col >= self.size.cols.get() {
            self.cursor.col = self.size.cols.get() - 1;
            self.wrap_pending = self.modes.autowrap;
        } else {
            self.cursor.col = next_col;
            self.wrap_pending = false;
        }
    }

    fn backspace(&mut self) {
        self.wrap_pending = false;
        self.cursor.col = self.cursor.col.saturating_sub(1);
    }

    fn horizontal_tab(&mut self) {
        self.wrap_pending = false;
        let next = (self.cursor.col / 8) * 8 + 8;
        self.cursor.col = next.min(self.size.cols.get().saturating_sub(1));
    }

    fn line_feed(&mut self) {
        self.wrap_pending = false;
        if self.cursor.row == self.scroll_bottom {
            self.scroll_up_screen(1);
        } else if self.cursor.row + 1 < self.size.rows.get() {
            self.cursor.row += 1;
        }
    }

    fn carriage_return(&mut self) {
        self.wrap_pending = false;
        self.cursor.col = 0;
    }

    fn reverse_index(&mut self) {
        self.wrap_pending = false;
        if self.cursor.row == self.scroll_top {
            let style = self.pen;
            let top = self.scroll_top;
            let bottom = self.scroll_bottom;
            self.active_mut().scroll_down(1, top, bottom, style);
        } else if self.cursor.row > 0 {
            self.cursor.row -= 1;
        }
    }

    fn save_cursor(&mut self) {
        self.saved = SavedCursor {
            cursor: self.cursor,
            pen: self.pen,
            wrap_pending: self.wrap_pending,
            origin: self.modes.origin,
        };
    }

    fn restore_cursor(&mut self) {
        self.cursor = self.saved.cursor;
        self.pen = self.saved.pen;
        self.wrap_pending = self.saved.wrap_pending;
        self.modes.origin = self.saved.origin;
        self.clamp_cursor();
    }

    fn ring_bell(&mut self) {
        self.events.push_request(ChildRequest::RingBell);
    }

    /// Resets the state the terminal derived from the child (RIS, `ESC c`).
    fn reset_child_state(&mut self) {
        let size = self.size;
        self.primary = Screen::blank(size);
        self.alternate = Screen::blank(size);
        // The whole screen was replaced, which is a visible change even when
        // the new screen is blank; record it so `feed` sees it. The alternate
        // screen needs no such mark: `on_alternate` is reset below, and that
        // alone is part of the snapshot `feed` compares.
        self.primary.mark_dirty();
        self.on_alternate = false;
        self.cursor = Position { row: 0, col: 0 };
        self.saved = SavedCursor::default();
        self.wrap_pending = false;
        self.pen = Style::default();
        self.modes = TerminalModes::default();
        // The palette and the colour slots are state the child set, so RIS
        // returns them to the theme the terminal had before it touched them.
        // The built-in table is a constant and is not "cleared".
        self.palette.fill(None);
        self.default_colors = DefaultColors::default();
        self.title.clear();
        // RIS restores the terminal, and a pending request belongs to the
        // session being reset; leaving it would let a caller act on an ask
        // from before the reset.
        self.events.clear_requests();
        self.scroll_top = 0;
        self.scroll_bottom = size.rows.get().saturating_sub(1);
        // RIS — DEC terminal documentation:
        // https://vt100.net/docs/vt510-rm/RIS.html
        // A hard reset restores the terminal, including saved lines; the
        // reference may change.
        self.scrollback.clear();
        self.scrollback_cells = 0;
        // The whole terminal was reset. This outranks the state-change flags
        // it would otherwise also raise (the screen is blank, the history is
        // gone, the title is cleared), so a caller sees one TerminalReset and
        // knows everything derived from the child before it is stale.
        self.events.mark_terminal_reset();
    }

    fn handle_csi(&mut self, params: &vte::Params, intermediates: &[u8], action: char) {
        // CSI allows one intermediate byte in the 0x20-0x2F range. The three
        // private markers are distinct queries, not one flag: `?` selects the
        // DEC-private form of an otherwise reusable final byte, while `>` and
        // `=` select DA2 and DA3. Folding `>` and `=` into "not private" (as a
        // `bool` would) routes a DA2 (`CSI > c`, sent by tmux at startup) and a
        // DA3 into the plain `'c'` arm and answers them with the DA1 reply.
        let marker = intermediates.first().copied();
        let private = marker == Some(b'?');
        match action {
            'A' => self.cursor_up(param_or(params, 0, 1)),
            'B' => self.cursor_down(param_or(params, 0, 1)),
            'C' => self.cursor_forward(param_or(params, 0, 1)),
            'D' => self.cursor_backward(param_or(params, 0, 1)),
            'E' => {
                self.cursor_down(param_or(params, 0, 1));
                self.cursor.col = 0;
            }
            'F' => {
                self.cursor_up(param_or(params, 0, 1));
                self.cursor.col = 0;
            }
            'G' | '`' => self.set_col(param_or(params, 0, 1)),
            'H' | 'f' => {
                let row = param_or(params, 0, 1);
                let col = param_or(params, 1, 1);
                self.set_position(row, col);
            }
            'd' => self.set_row(param_or(params, 0, 1)),
            'J' => self.erase_display(param_or(params, 0, 0)),
            'K' => self.erase_line(param_or(params, 0, 0)),
            'L' => {
                let count = param_or(params, 0, 1);
                let row = self.cursor.row;
                let style = self.pen;
                let top = self.scroll_top;
                let bottom = self.scroll_bottom;
                self.active_mut()
                    .insert_lines(row, count, top, bottom, style);
            }
            'M' => {
                let count = param_or(params, 0, 1);
                let row = self.cursor.row;
                let style = self.pen;
                let top = self.scroll_top;
                let bottom = self.scroll_bottom;
                self.active_mut()
                    .delete_lines(row, count, top, bottom, style);
            }
            '@' => {
                let count = param_or(params, 0, 1);
                let row = self.cursor.row;
                let col = self.cursor.col;
                let style = self.pen;
                self.active_mut().insert_columns(row, col, count, style);
            }
            'P' => {
                let count = param_or(params, 0, 1);
                let row = self.cursor.row;
                let col = self.cursor.col;
                let style = self.pen;
                self.active_mut().delete_columns(row, col, count, style);
            }
            'X' => {
                let count = param_or(params, 0, 1);
                let row = self.cursor.row;
                let col = self.cursor.col;
                let style = self.pen;
                self.active_mut()
                    .erase_cells(row, col, col.saturating_add(count), style);
            }
            'S' => self.scroll_up_screen(param_or(params, 0, 1)),
            'T' => {
                let count = param_or(params, 0, 1);
                let style = self.pen;
                let top = self.scroll_top;
                let bottom = self.scroll_bottom;
                self.active_mut().scroll_down(count, top, bottom, style);
            }
            'r' if !private => self.set_scroll_region(params),
            'm' if !private => self.handle_sgr(params),
            'h' => self.set_mode(params, private, true),
            'l' => self.set_mode(params, private, false),
            'n' => self.device_status(params, private),
            // DA1 answers only the primary forms (`CSI c` and `CSI ? c`). DA2
            // (`>`) and DA3 (`=`) are recognized so they no longer reach
            // `primary_da`, but are left unanswered rather than replied to with
            // a DA1-shaped string; see the supported-query list on
            // `TerminalState`.
            'c' if !private && marker.is_none() => self.primary_da(),
            's' if !private => self.save_cursor(),
            'u' if !private => self.restore_cursor(),
            _ => {}
        }
    }

    fn cursor_up(&mut self, n: u16) {
        self.wrap_pending = false;
        let min_row = if self.modes.origin {
            self.scroll_top
        } else {
            0
        };
        self.cursor.row = self.cursor.row.saturating_sub(n).max(min_row);
    }

    fn cursor_down(&mut self, n: u16) {
        self.wrap_pending = false;
        let max_row = if self.modes.origin {
            self.scroll_bottom
        } else {
            self.size.rows.get().saturating_sub(1)
        };
        self.cursor.row = self.cursor.row.saturating_add(n).min(max_row);
    }

    fn cursor_forward(&mut self, n: u16) {
        self.wrap_pending = false;
        self.cursor.col = self
            .cursor
            .col
            .saturating_add(n)
            .min(self.size.cols.get().saturating_sub(1));
    }

    fn cursor_backward(&mut self, n: u16) {
        self.wrap_pending = false;
        self.cursor.col = self.cursor.col.saturating_sub(n);
    }

    fn set_col(&mut self, col_one_based: u16) {
        self.wrap_pending = false;
        self.cursor.col = col_one_based
            .saturating_sub(1)
            .min(self.size.cols.get().saturating_sub(1));
    }

    fn set_row(&mut self, row_one_based: u16) {
        self.wrap_pending = false;
        let (min_row, max_row) = self.origin_bounds();
        let row = row_one_based.saturating_sub(1) + if self.modes.origin { min_row } else { 0 };
        self.cursor.row = row.clamp(min_row, max_row);
    }

    fn set_position(&mut self, row_one_based: u16, col_one_based: u16) {
        self.wrap_pending = false;
        let (min_row, max_row) = self.origin_bounds();
        let row = row_one_based.saturating_sub(1) + if self.modes.origin { min_row } else { 0 };
        let col = col_one_based.saturating_sub(1);
        self.cursor.row = row.clamp(min_row, max_row);
        self.cursor.col = col.min(self.size.cols.get().saturating_sub(1));
    }

    fn origin_bounds(&self) -> (u16, u16) {
        if self.modes.origin {
            (self.scroll_top, self.scroll_bottom)
        } else {
            (0, self.size.rows.get().saturating_sub(1))
        }
    }

    fn clamp_cursor(&mut self) {
        let (min_row, max_row) = self.origin_bounds();
        self.cursor.row = self.cursor.row.clamp(min_row, max_row);
        self.cursor.col = self.cursor.col.min(self.size.cols.get().saturating_sub(1));
        self.repair_cursor_cell();
    }

    fn erase_display(&mut self, mode: u16) {
        let style = self.pen;
        let row = self.cursor.row;
        let col = self.cursor.col;
        let cols = self.size.cols.get();
        let rows = self.size.rows.get();
        match mode {
            0 => {
                self.active_mut().erase_cells(row, col, cols, style);
                self.active_mut()
                    .erase_rows(row.saturating_add(1), rows, style);
            }
            1 => {
                self.active_mut().erase_rows(0, row, style);
                self.active_mut()
                    .erase_cells(row, 0, col.saturating_add(1), style);
            }
            2 => self.active_mut().erase_rows(0, rows, style),
            3 => {
                // xterm Control Sequences: ED parameter 3 ("Erase Saved
                // Lines"). https://invisible-island.net/xterm/ctlseqs/ctlseqs.html
                // The sequence name and reference may change as the host
                // terminal evolves.
                self.scrollback.clear();
                self.scrollback_cells = 0;
            }
            _ => {}
        }
    }

    fn erase_line(&mut self, mode: u16) {
        let style = self.pen;
        let row = self.cursor.row;
        let col = self.cursor.col;
        let cols = self.size.cols.get();
        match mode {
            0 => self.active_mut().erase_cells(row, col, cols, style),
            1 => self
                .active_mut()
                .erase_cells(row, 0, col.saturating_add(1), style),
            2 => self.active_mut().erase_cells(row, 0, cols, style),
            _ => {}
        }
    }

    fn set_scroll_region(&mut self, params: &vte::Params) {
        let top = param_or(params, 0, 1).saturating_sub(1);
        let bottom = if params_len(params) >= 2 {
            param_or(params, 1, self.size.rows.get()).saturating_sub(1)
        } else {
            self.size.rows.get().saturating_sub(1)
        };
        if top < self.size.rows.get() && bottom < self.size.rows.get() && top < bottom {
            self.scroll_top = top;
            self.scroll_bottom = bottom;
        } else if params_len(params) == 0 {
            self.scroll_top = 0;
            self.scroll_bottom = self.size.rows.get().saturating_sub(1);
        }
        self.cursor = Position { row: 0, col: 0 };
        if self.modes.origin {
            self.cursor.row = self.scroll_top;
        }
        self.wrap_pending = false;
    }

    fn set_mode(&mut self, params: &vte::Params, private: bool, enable: bool) {
        for param in flatten_params(params) {
            if private {
                match param {
                    // DECCKM — https://vt100.net/docs/vt510-rm/DECCKM.html
                    1 => self.modes.application_cursor = enable,
                    // DECOM — https://vt100.net/docs/vt510-rm/DECOM.html
                    6 => {
                        self.modes.origin = enable;
                        self.set_position(1, 1);
                    }
                    // DECAWM — https://vt100.net/docs/vt510-rm/DECAWM.html
                    7 => self.modes.autowrap = enable,
                    // DECTCEM — cursor visible (?25).
                    25 => self.modes.cursor_visible = enable,
                    9 => {
                        self.modes.mouse = if enable {
                            MouseReporting::X10
                        } else {
                            MouseReporting::Off
                        };
                    }
                    1000 => {
                        self.modes.mouse = if enable {
                            MouseReporting::Normal
                        } else {
                            MouseReporting::Off
                        };
                    }
                    1002 => {
                        self.modes.mouse = if enable {
                            MouseReporting::ButtonEvent
                        } else {
                            MouseReporting::Off
                        };
                    }
                    1003 => {
                        self.modes.mouse = if enable {
                            MouseReporting::AnyEvent
                        } else {
                            MouseReporting::Off
                        };
                    }
                    1006 => self.modes.mouse_sgr = enable,
                    // Application keypad (?66).
                    66 => self.modes.application_keypad = enable,
                    47 | 1047 => self.set_alternate(enable, false),
                    1049 => self.set_alternate(enable, true),
                    // Bracketed paste — xterm ctlseqs (may evolve).
                    2004 => self.modes.bracketed_paste = enable,
                    _ => {}
                }
            } else if param == 4 {
                // IRM — insert/replace (ECMA-48).
                self.modes.insert = enable;
            }
        }
    }

    fn set_alternate(&mut self, enable: bool, clear_on_enter: bool) {
        if enable {
            if !self.on_alternate {
                self.save_cursor();
                self.on_alternate = true;
            }
            if clear_on_enter {
                self.alternate.clear_all(Style::default());
                self.cursor = Position { row: 0, col: 0 };
                self.wrap_pending = false;
                self.scroll_top = 0;
                self.scroll_bottom = self.size.rows.get().saturating_sub(1);
            }
        } else if self.on_alternate {
            self.on_alternate = false;
            self.restore_cursor();
        }
    }

    fn device_status(&mut self, params: &vte::Params, private: bool) {
        if private {
            return;
        }
        match param_or(params, 0, 0) {
            // DSR — terminal OK.
            5 => self.replies.push(b"\x1b[0n"),
            // CPR — cursor position report.
            6 => {
                let row = self.cursor.row + 1;
                let col = self.cursor.col + 1;
                let reply = format!("\x1b[{row};{col}R");
                self.replies.push(reply.as_bytes());
            }
            _ => {}
        }
    }

    /// Answers a primary device attributes request (DA1).
    ///
    /// The reply is shaped like a VT102 (spec:
    /// https://vt100.net/docs/vt510-rm/DA1.html). DA2 (`CSI > c`) and DA3
    /// (`CSI = c`) are not answered: callers that probe with those forms read no
    /// reply rather than a reply that does not match the request (see the
    /// `'c'` arm in `handle_csi`).
    fn primary_da(&mut self) {
        self.replies.push(b"\x1b[?6c");
    }

    fn handle_sgr(&mut self, params: &vte::Params) {
        let values = flatten_params(params);
        if values.is_empty() {
            self.pen = Style::default();
            return;
        }
        let mut i = 0;
        while i < values.len() {
            match values[i] {
                0 => self.pen = Style::default(),
                1 => self.pen.bold = true,
                3 => self.pen.italic = true,
                4 => self.pen.underline = true,
                7 => self.pen.reverse = true,
                22 => self.pen.bold = false,
                23 => self.pen.italic = false,
                24 => self.pen.underline = false,
                27 => self.pen.reverse = false,
                n @ 30..=37 => self.pen.foreground = Color::Indexed((n - 30) as u8),
                39 => self.pen.foreground = Color::Default,
                n @ 40..=47 => self.pen.background = Color::Indexed((n - 40) as u8),
                49 => self.pen.background = Color::Default,
                n @ 90..=97 => self.pen.foreground = Color::Indexed((n - 90 + 8) as u8),
                n @ 100..=107 => self.pen.background = Color::Indexed((n - 100 + 8) as u8),
                38 => {
                    if let Some((color, consumed)) = parse_extended_color(&values[i + 1..]) {
                        self.pen.foreground = color;
                        i += consumed;
                    }
                }
                48 => {
                    if let Some((color, consumed)) = parse_extended_color(&values[i + 1..]) {
                        self.pen.background = color;
                        i += consumed;
                    }
                }
                _ => {}
            }
            i += 1;
        }
    }
}

fn parse_extended_color(values: &[u16]) -> Option<(Color, usize)> {
    match values.first().copied()? {
        5 => {
            let idx = u8::try_from(*values.get(1)?).ok()?;
            Some((Color::Indexed(idx), 2))
        }
        2 => {
            let r = u8::try_from(*values.get(1)?).ok()?;
            let g = u8::try_from(*values.get(2)?).ok()?;
            let b = u8::try_from(*values.get(3)?).ok()?;
            Some((Color::Rgb(r, g, b), 4))
        }
        _ => None,
    }
}

/// Parses an OSC 4 palette index field into a `u8`.
///
/// The field is decimal, and an index of 255 is the largest the palette has, so
/// a larger number names nothing and is rejected rather than wrapped. A
/// non-numeric or empty field is rejected the same way: the OSC catalogue gives
/// the field no other meaning.
fn parse_osc_index(field: &[u8]) -> Option<u8> {
    if field.is_empty() || !field.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(field).ok()?.parse().ok()
}

/// Parses an xterm colour spec (`rgb:RR/GG/BB`) into 8-bit channels.
///
/// Each channel is one to four hex digits, the form xterm's `OSC 4` and
/// `OSC 10`-`12` accept. The scaling is the documented xterm one: a
/// one-digit channel is scaled to spread it across the byte (`f` is 255, not
/// 15), while two or more digits are taken as the most significant ones and
/// the byte is the value shifted down (`ff` and `ffff` are both 255). That
/// keeps `rgb:f/0/0`, `rgb:ff/00/00`, and `rgb:ffff/0000/0000` all meaning
/// the same colour, which is what a child that queries and re-sets a colour
/// expects.
///
/// A value without the `rgb:` prefix, with the wrong number of channels, with
/// an empty channel, or with a non-hex digit returns `None` so the caller
/// stores nothing: an unrecognized spec is not a colour to guess at.
fn parse_osc_rgb(value: &[u8]) -> Option<(u8, u8, u8)> {
    let digits = value.strip_prefix(b"rgb:")?;
    let mut channels = digits.split(|byte| *byte == b'/');
    let r = parse_osc_channel(channels.next()?)?;
    let g = parse_osc_channel(channels.next()?)?;
    let b = parse_osc_channel(channels.next()?)?;
    if channels.next().is_some() {
        return None;
    }
    Some((r, g, b))
}

/// Converts one hex channel of one to four digits to a `u8`, xterm-style.
fn parse_osc_channel(field: &[u8]) -> Option<u8> {
    if field.is_empty() || field.len() > 4 || !field.iter().all(u8::is_ascii_hexdigit) {
        return None;
    }
    let digits = std::str::from_utf8(field).ok()?;
    let value = u16::from_str_radix(digits, 16).ok()?;
    Some(match field.len() {
        1 => (value * 17) as u8,
        _ => (value >> (4 * (field.len() as u32 - 2))) as u8,
    })
}

fn param_or(params: &vte::Params, index: usize, default: u16) -> u16 {
    match params.iter().nth(index).and_then(|pair| pair.first()) {
        Some(&0) | None => default,
        Some(&value) => value,
    }
}

fn params_len(params: &vte::Params) -> usize {
    params.iter().count()
}

fn flatten_params(params: &vte::Params) -> Vec<u16> {
    let mut out = Vec::new();
    for sub in params.iter() {
        if sub.is_empty() {
            out.push(0);
        } else {
            // Colon subparameters (e.g. 38:2:r:g:b) are flattened so both
            // semicolon and colon SGR color forms work.
            out.extend(sub.iter().copied());
        }
    }
    out
}

/// Decodes a standard-alphabet base64 payload, returning `None` when it is
/// malformed.
///
/// OSC 52 payloads are quoted in whatever `vte` handed over, so input is
/// bytes rather than `&str`. Padding is optional (`=` / `==` are accepted but
/// not required), and whitespace is rejected rather than skipped: a payload
/// with an embedded space is not something a well-formed writer produces, and
/// silently accepting it would make an invalid sequence indistinguishable from
/// a valid one.
///
/// A malformed payload returns `None` so the caller can store nothing. It must
/// not return a partially decoded value: a caller has no way to tell a mangled
/// payload from the bytes that were meant.
///
/// The result is bytes rather than a string: the payload is opaque to the
/// terminal, so a decoded value that is not valid UTF-8 is returned as-is
/// instead of being repaired or rejected.
fn decode_base64(input: &[u8]) -> Option<Vec<u8>> {
    // Groups of four base64 characters become three bytes. `buffer` holds the
    // six-bit values of an incomplete group (0..4 of them), `out` the bytes
    // produced so far.
    let mut out: Vec<u8> = Vec::with_capacity(input.len() / 4 * 3);
    let mut buffer: [u8; 4] = [0; 4];
    let mut buffered = 0usize;

    // Padding is only valid at the end: once `=` appears, every remaining byte
    // must also be `=`, so a character after padding is malformed rather than
    // ignored.
    let mut padding = false;
    for &byte in input {
        if padding {
            if byte != b'=' {
                return None;
            }
            continue;
        }
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => {
                padding = true;
                continue;
            }
            _ => return None,
        };
        buffer[buffered] = value;
        buffered += 1;
        if buffered == 4 {
            out.push((buffer[0] << 2) | (buffer[1] >> 4));
            out.push((buffer[1] << 4) | (buffer[2] >> 2));
            out.push((buffer[2] << 6) | buffer[3]);
            buffered = 0;
        }
    }

    // A tail of 2 or 3 characters encodes 1 or 2 bytes; a single leftover
    // character is not a valid amount of padding and is rejected.
    match buffered {
        0 => {}
        2 => out.push(buffer[0] << 2 | buffer[1] >> 4),
        3 => {
            out.push(buffer[0] << 2 | buffer[1] >> 4);
            out.push(buffer[1] << 4 | buffer[2] >> 2);
        }
        _ => return None,
    }

    Some(out)
}

fn char_display_width(ch: char) -> Option<usize> {
    match ch.width() {
        None => None,
        Some(0) => Some(0),
        Some(1) => Some(1),
        Some(2) => Some(2),
        Some(_) => Some(1),
    }
}
