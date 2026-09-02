//! VTE performer that mutates [`crate::terminal::TerminalState`].

use unicode_width::UnicodeWidthChar;
use vte::Perform;

use crate::terminal::TerminalState;
use crate::terminal_buffer::Screen;
use crate::terminal_types::{
    Color, MouseReporting, Position, SavedCursor, Style, TerminalAction, TerminalModes,
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
            0x07 => {}
            _ => {}
        }
    }

    fn hook(&mut self, _params: &vte::Params, _intermediates: &[u8], _ignore: bool, _action: char) {
    }

    fn put(&mut self, _byte: u8) {}

    fn unhook(&mut self) {}

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        // OSC 0 / 2 store the window title. Other OSC numbers are ignored so
        // their payloads never appear as printable text.
        // xterm OSC catalogue: https://invisible-island.net/xterm/ctlseqs/ctlseqs.html
        // (OSC identifiers evolve; termnix only retains title text.)
        if params.is_empty() {
            return;
        }
        let Ok(id) = std::str::from_utf8(params[0]) else {
            return;
        };
        if matches!(id, "0" | "2") {
            self.term.title = params
                .get(1)
                .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
                .unwrap_or_default();
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
            b'c' => self.term.soft_reset(),
            _ => {}
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

    fn soft_reset(&mut self) {
        let size = self.size;
        self.primary = Screen::blank(size);
        self.alternate = Screen::blank(size);
        self.on_alternate = false;
        self.cursor = Position { row: 0, col: 0 };
        self.saved = SavedCursor::default();
        self.wrap_pending = false;
        self.pen = Style::default();
        self.modes = TerminalModes::default();
        self.title.clear();
        self.scroll_top = 0;
        self.scroll_bottom = size.rows.get().saturating_sub(1);
        // RIS — DEC terminal documentation:
        // https://vt100.net/docs/vt510-rm/RIS.html
        // A hard reset restores the terminal, including saved lines; the
        // reference may change.
        self.scrollback.clear();
        self.scrollback_cells = 0;
    }

    fn handle_csi(&mut self, params: &vte::Params, intermediates: &[u8], action: char) {
        let private = intermediates.first().copied() == Some(b'?');
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
            'c' if !private => self.primary_da(),
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
            5 => self
                .actions
                .push(TerminalAction::WritePty(b"\x1b[0n".to_vec())),
            // CPR — cursor position report.
            6 => {
                let row = self.cursor.row + 1;
                let col = self.cursor.col + 1;
                let reply = format!("\x1b[{row};{col}R").into_bytes();
                self.actions.push(TerminalAction::WritePty(reply));
            }
            _ => {}
        }
    }

    fn primary_da(&mut self) {
        // DA1 reply shaped like a VT102. Spec: https://vt100.net/docs/vt510-rm/DA1.html
        self.actions
            .push(TerminalAction::WritePty(b"\x1b[?6c".to_vec()));
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

fn char_display_width(ch: char) -> Option<usize> {
    match ch.width() {
        None => None,
        Some(0) => Some(0),
        Some(1) => Some(1),
        Some(2) => Some(2),
        Some(_) => Some(1),
    }
}
