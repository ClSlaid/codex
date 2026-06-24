// This is derived from `ratatui::Terminal`, which is licensed under the following terms:
//
// The MIT License (MIT)
// Copyright (c) 2016-2022 Florian Dehau
// Copyright (c) 2023-2025 The Ratatui Developers
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all
// copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
// SOFTWARE.
use std::io;
use std::io::Write;
use std::ops::Range;
#[cfg(test)]
use std::time::Duration;
#[cfg(test)]
use std::time::Instant;

use crossterm::cursor::MoveTo;
use crossterm::cursor::SetCursorStyle;
use crossterm::queue;
use crossterm::style::Colors;
use crossterm::style::Print;
use crossterm::style::SetAttribute;
use crossterm::style::SetBackgroundColor;
use crossterm::style::SetColors;
use crossterm::style::SetForegroundColor;
use crossterm::terminal::Clear;
use crossterm::terminal::ClearType as CrosstermClearType;
use derive_more::IsVariant;
use ratatui::backend::Backend;
use ratatui::backend::ClearType;
use ratatui::buffer::Buffer;
use ratatui::layout::Position;
use ratatui::layout::Rect;
use ratatui::layout::Size;
use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::widgets::WidgetRef;
use unicode_width::UnicodeWidthStr;

/// Returns the display width of a cell symbol, ignoring OSC escape sequences.
///
/// OSC sequences (e.g. OSC 8 hyperlinks: `\x1B]8;;URL\x07`) are terminal
/// control sequences that don't consume display columns.  The standard
/// `UnicodeWidthStr::width()` method incorrectly counts the printable
/// characters inside OSC payloads (like `]`, `8`, `;`, and URL characters).
/// This function strips them first so that only visible characters contribute
/// to the width.
fn display_width(s: &str) -> usize {
    if s == " " {
        return 1;
    }
    if s.len() == 1 && s.as_bytes()[0].is_ascii_graphic() {
        return 1;
    }

    // Fast path: no escape sequences present.
    if !s.contains('\x1B') {
        return s.width();
    }

    // Strip OSC sequences: ESC ] ... BEL
    let mut visible = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(ch) = chars.next() {
        if ch == '\x1B' && chars.clone().next() == Some(']') {
            // Consume the ']' and everything up to and including BEL.
            chars.next(); // skip ']'
            for c in chars.by_ref() {
                if c == '\x07' {
                    break;
                }
            }
            continue;
        }
        visible.push(ch);
    }
    visible.width()
}

pub struct Frame<'a> {
    /// Where should the cursor be after drawing this frame?
    ///
    /// If `None`, the cursor is hidden and its position is controlled by the backend. If `Some((x,
    /// y))`, the cursor is shown and placed at `(x, y)` after the call to `Terminal::draw()`.
    pub(crate) cursor_position: Option<Position>,

    /// Visible cursor shape to apply after drawing this frame.
    cursor_style: SetCursorStyle,

    /// The area of the viewport
    pub(crate) viewport_area: Rect,

    /// The buffer that is used to draw the current frame
    pub(crate) buffer: &'a mut Buffer,
}

#[derive(Debug)]
pub(crate) enum FrameFlush {
    Sparse,
    /// Repaint a dense row range without diffing against the previous buffer.
    ///
    /// Callers that use an optimized renderer which skips blank cells must clear the same rows in
    /// the frame buffer before rendering; the terminal only controls flushing and buffer swapping.
    Dense(Range<u16>),
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct TerminalDrawTimings {
    pub(crate) autoresize: Duration,
    pub(crate) render: Duration,
    pub(crate) diff: Duration,
    pub(crate) encode: Duration,
    pub(crate) cursor: Duration,
    pub(crate) buffer_swap: Duration,
    pub(crate) backend_flush: Duration,
    pub(crate) total: Duration,
    pub(crate) commands: usize,
}

impl Frame<'_> {
    /// The area of the current frame
    ///
    /// This is guaranteed not to change during rendering, so may be called multiple times.
    ///
    /// If your app listens for a resize event from the backend, it should ignore the values from
    /// the event for any calculations that are used to render the current frame and use this value
    /// instead as this is the area of the buffer that is used to render the current frame.
    pub const fn area(&self) -> Rect {
        self.viewport_area
    }

    /// Render a [`WidgetRef`] to the current buffer using [`WidgetRef::render_ref`].
    ///
    /// Usually the area argument is the size of the current frame or a sub-area of the current
    /// frame (which can be obtained using [`Layout`] to split the total area).
    #[allow(clippy::needless_pass_by_value)]
    pub fn render_widget_ref<W: WidgetRef>(&mut self, widget: W, area: Rect) {
        widget.render_ref(area, self.buffer);
    }

    /// After drawing this frame, make the cursor visible and put it at the specified (x, y)
    /// coordinates. If this method is not called, the cursor will be hidden.
    ///
    /// Note that this will interfere with calls to [`Terminal::hide_cursor`],
    /// [`Terminal::show_cursor`], and [`Terminal::set_cursor_position`]. Pick one of the APIs and
    /// stick with it.
    ///
    /// [`Terminal::hide_cursor`]: crate::Terminal::hide_cursor
    /// [`Terminal::show_cursor`]: crate::Terminal::show_cursor
    /// [`Terminal::set_cursor_position`]: crate::Terminal::set_cursor_position
    pub fn set_cursor_position<P: Into<Position>>(&mut self, position: P) {
        self.cursor_position = Some(position.into());
    }

    /// After drawing this frame, set the terminal's visible cursor style.
    pub fn set_cursor_style(&mut self, style: SetCursorStyle) {
        self.cursor_style = style;
    }

    /// Gets the buffer that this `Frame` draws into as a mutable reference.
    pub fn buffer_mut(&mut self) -> &mut Buffer {
        self.buffer
    }

    /// Clear rows in the in-progress frame before a dense partial renderer writes into them.
    pub(crate) fn clear_rows(&mut self, rows: Range<u16>) {
        clear_buffer_rows(self.buffer, rows);
    }
}

#[derive(Debug, Default, Clone, Eq, PartialEq, Hash)]
pub struct Terminal<B>
where
    B: Backend + Write,
{
    /// The backend used to interface with the terminal
    backend: B,
    /// Holds the results of the current and previous draw calls. The two are compared at the end
    /// of each draw pass to output the necessary updates to the terminal
    buffers: [Buffer; 2],
    /// Index of the current buffer in the previous array
    current: usize,
    /// Whether the cursor is currently hidden
    pub hidden_cursor: bool,
    /// Area of the viewport
    pub viewport_area: Rect,
    /// Last known size of the terminal. Used to detect if the internal buffers have to be resized.
    pub last_known_screen_size: Size,
    /// Last known position of the cursor. Used to find the new area when the viewport is inlined
    /// and the terminal resized.
    pub last_known_cursor_pos: Position,
    /// Count of visible history rows rendered above the viewport in inline mode.
    visible_history_rows: u16,
}

impl<B> Drop for Terminal<B>
where
    B: Backend,
    B: Write,
{
    #[allow(clippy::print_stderr)]
    fn drop(&mut self) {
        // Attempt to restore the cursor state
        if let Err(err) = self.reset_cursor_style() {
            eprintln!("Failed to reset the cursor style: {err}");
        }

        if self.hidden_cursor
            && let Err(err) = self.show_cursor()
        {
            eprintln!("Failed to show the cursor: {err}");
        }
    }
}

impl<B> Terminal<B>
where
    B: Backend,
    B: Write,
{
    /// Creates a new [`Terminal`] with the given [`Backend`] and [`TerminalOptions`].
    pub fn with_options(mut backend: B) -> io::Result<Self> {
        let screen_size = backend.size()?;
        let cursor_pos = backend.get_cursor_position().unwrap_or_else(|err| {
            // Some PTYs do not answer CPR (`ESC[6n`); continue with a safe default instead
            // of failing TUI startup.
            tracing::warn!("failed to read initial cursor position; defaulting to origin: {err}");
            Position { x: 0, y: 0 }
        });
        Ok(Self::with_screen_size_and_cursor_position(
            backend,
            screen_size,
            cursor_pos,
        ))
    }

    /// Creates a new [`Terminal`] from a caller-provided initial cursor position.
    ///
    /// Startup code uses this when cursor probing has already happened outside the backend, for
    /// example through a bounded terminal probe. Supplying a stale or synthetic position changes
    /// the inline viewport anchor, so callers should only use this after they have chosen the same
    /// fallback they want the first render to honor.
    pub fn with_options_and_cursor_position(backend: B, cursor_pos: Position) -> io::Result<Self> {
        let screen_size = backend.size()?;
        Ok(Self::with_screen_size_and_cursor_position(
            backend,
            screen_size,
            cursor_pos,
        ))
    }

    fn with_screen_size_and_cursor_position(
        backend: B,
        screen_size: Size,
        cursor_pos: Position,
    ) -> Self {
        Self {
            backend,
            buffers: [Buffer::empty(Rect::ZERO), Buffer::empty(Rect::ZERO)],
            current: 0,
            hidden_cursor: false,
            viewport_area: Rect::new(
                /*x*/ 0,
                cursor_pos.y,
                /*width*/ 0,
                /*height*/ 0,
            ),
            last_known_screen_size: screen_size,
            last_known_cursor_pos: cursor_pos,
            visible_history_rows: 0,
        }
    }

    /// Get a Frame object which provides a consistent view into the terminal state for rendering.
    pub fn get_frame(&mut self) -> Frame<'_> {
        Frame {
            cursor_position: None,
            cursor_style: SetCursorStyle::DefaultUserShape,
            viewport_area: self.viewport_area,
            buffer: self.current_buffer_mut(),
        }
    }

    /// Gets the current buffer as a reference.
    fn current_buffer(&self) -> &Buffer {
        &self.buffers[self.current]
    }

    /// Gets the current buffer as a mutable reference.
    fn current_buffer_mut(&mut self) -> &mut Buffer {
        &mut self.buffers[self.current]
    }

    /// Gets the previous buffer as a reference.
    fn previous_buffer(&self) -> &Buffer {
        &self.buffers[1 - self.current]
    }

    /// Gets the previous buffer as a mutable reference.
    fn previous_buffer_mut(&mut self) -> &mut Buffer {
        &mut self.buffers[1 - self.current]
    }

    /// Gets the backend
    pub const fn backend(&self) -> &B {
        &self.backend
    }

    /// Gets the backend as a mutable reference
    pub fn backend_mut(&mut self) -> &mut B {
        &mut self.backend
    }

    /// Obtains a difference between the previous and the current buffer and passes it to the
    /// current backend for drawing.
    pub fn flush(&mut self) -> io::Result<()> {
        let updates = diff_buffers(self.previous_buffer(), self.current_buffer());
        let last_put_command = updates.iter().rfind(|command| command.is_put());
        if let Some(&DrawCommand::Put { x, y, .. }) = last_put_command {
            self.last_known_cursor_pos = Position { x, y };
        }
        draw(&mut self.backend, updates.into_iter())
    }

    fn flush_dense_rows(&mut self, rows: Range<u16>) -> io::Result<()> {
        let current = self.current;
        let stats = draw_dense_rows(&mut self.backend, &self.buffers[current], rows)?;
        if let Some(position) = stats.last_put {
            self.last_known_cursor_pos = position;
        }
        Ok(())
    }

    #[cfg(test)]
    fn flush_profiled(
        &mut self,
        dense_rows: Option<Range<u16>>,
    ) -> io::Result<(Duration, Duration, usize)> {
        if let Some(rows) = dense_rows {
            let started = Instant::now();
            let current = self.current;
            let stats = draw_dense_rows(&mut self.backend, &self.buffers[current], rows)?;
            let encode = started.elapsed();
            if let Some(position) = stats.last_put {
                self.last_known_cursor_pos = position;
            }
            return Ok((Duration::ZERO, encode, stats.commands));
        }

        let started = Instant::now();
        let updates = diff_buffers(self.previous_buffer(), self.current_buffer());
        let diff = started.elapsed();
        let commands = updates.len();
        let last_put_command = updates.iter().rfind(|command| command.is_put());
        if let Some(&DrawCommand::Put { x, y, .. }) = last_put_command {
            self.last_known_cursor_pos = Position { x, y };
        }

        let started = Instant::now();
        draw(&mut self.backend, updates.into_iter())?;
        Ok((diff, started.elapsed(), commands))
    }

    /// Updates the Terminal so that internal buffers match the requested area.
    ///
    /// Requested area will be saved to remain consistent when rendering. This leads to a full clear
    /// of the screen.
    pub fn resize(&mut self, screen_size: Size) -> io::Result<()> {
        self.last_known_screen_size = screen_size;
        Ok(())
    }

    /// Sets the viewport area.
    pub fn set_viewport_area(&mut self, area: Rect) {
        self.current_buffer_mut().resize(area);
        self.previous_buffer_mut().resize(area);
        self.viewport_area = area;
        self.visible_history_rows = self.visible_history_rows.min(area.top());
    }

    /// Queries the backend for size and resizes if it doesn't match the previous size.
    pub fn autoresize(&mut self) -> io::Result<()> {
        let screen_size = self.size()?;
        if screen_size != self.last_known_screen_size {
            self.resize(screen_size)?;
        }
        Ok(())
    }

    /// Draws a single frame to the terminal.
    ///
    /// Returns a [`CompletedFrame`] if successful, otherwise a [`std::io::Error`].
    ///
    /// If the render callback passed to this method can fail, use [`try_draw`] instead.
    ///
    /// Applications should call `draw` or [`try_draw`] in a loop to continuously render the
    /// terminal. These methods are the main entry points for drawing to the terminal.
    ///
    /// [`try_draw`]: Terminal::try_draw
    ///
    /// This method will:
    ///
    /// - autoresize the terminal if necessary
    /// - call the render callback, passing it a [`Frame`] reference to render to
    /// - flush the current internal state by copying the current buffer to the backend
    /// - move the cursor to the last known position if it was set during the rendering closure
    ///
    /// The render callback should fully render the entire frame when called, including areas that
    /// are unchanged from the previous frame. This is because each frame is compared to the
    /// previous frame to determine what has changed, and only the changes are written to the
    /// terminal. If the render callback does not fully render the frame, the terminal will not be
    /// in a consistent state.
    pub fn draw<F>(&mut self, render_callback: F) -> io::Result<()>
    where
        F: FnOnce(&mut Frame),
    {
        self.try_draw(|frame| {
            render_callback(frame);
            io::Result::Ok(())
        })
    }

    pub(crate) fn draw_with_flush<F>(&mut self, render_callback: F) -> io::Result<()>
    where
        F: FnOnce(&mut Frame) -> FrameFlush,
    {
        self.try_draw_with_flush(|frame| io::Result::Ok(render_callback(frame)))
    }

    /// Tries to draw a single frame to the terminal.
    ///
    /// Returns [`Result::Ok`] containing a [`CompletedFrame`] if successful, otherwise
    /// [`Result::Err`] containing the [`std::io::Error`] that caused the failure.
    ///
    /// This is the equivalent of [`Terminal::draw`] but the render callback is a function or
    /// closure that returns a `Result` instead of nothing.
    ///
    /// Applications should call `try_draw` or [`draw`] in a loop to continuously render the
    /// terminal. These methods are the main entry points for drawing to the terminal.
    ///
    /// [`draw`]: Terminal::draw
    ///
    /// This method will:
    ///
    /// - autoresize the terminal if necessary
    /// - call the render callback, passing it a [`Frame`] reference to render to
    /// - flush the current internal state by copying the current buffer to the backend
    /// - move the cursor to the last known position if it was set during the rendering closure
    /// - return a [`CompletedFrame`] with the current buffer and the area of the terminal
    ///
    /// The render callback passed to `try_draw` can return any [`Result`] with an error type that
    /// can be converted into an [`std::io::Error`] using the [`Into`] trait. This makes it possible
    /// to use the `?` operator to propagate errors that occur during rendering. If the render
    /// callback returns an error, the error will be returned from `try_draw` as an
    /// [`std::io::Error`] and the terminal will not be updated.
    ///
    /// The [`CompletedFrame`] returned by this method can be useful for debugging or testing
    /// purposes, but it is often not used in regular applicationss.
    ///
    /// The render callback should fully render the entire frame when called, including areas that
    /// are unchanged from the previous frame. This is because each frame is compared to the
    /// previous frame to determine what has changed, and only the changes are written to the
    /// terminal. If the render function does not fully render the frame, the terminal will not be
    /// in a consistent state.
    pub fn try_draw<F, E>(&mut self, render_callback: F) -> io::Result<()>
    where
        F: FnOnce(&mut Frame) -> Result<(), E>,
        E: Into<io::Error>,
    {
        self.try_draw_with_flush(|frame| {
            render_callback(frame).map_err(Into::into)?;
            io::Result::Ok(FrameFlush::Sparse)
        })
    }

    pub(crate) fn try_draw_with_flush<F, E>(&mut self, render_callback: F) -> io::Result<()>
    where
        F: FnOnce(&mut Frame) -> Result<FrameFlush, E>,
        E: Into<io::Error>,
    {
        // Autoresize - otherwise we get glitches if shrinking or potential desync between widgets
        // and the terminal (if growing), which may OOB.
        self.autoresize()?;

        // We can't change the cursor position right away because we have to flush the frame to
        // stdout first. But we also can't keep the frame around, since it holds a &mut to
        // Buffer. Thus, we're taking the important data out of the Frame.
        let (cursor_position, cursor_style, flush) = {
            let mut frame = self.get_frame();
            let flush = render_callback(&mut frame).map_err(Into::into)?;
            (frame.cursor_position, frame.cursor_style, flush)
        };

        // Draw to stdout
        let dense_rows = match flush {
            FrameFlush::Sparse => {
                self.flush()?;
                None
            }
            FrameFlush::Dense(rows) => {
                self.flush_dense_rows(rows.clone())?;
                Some(rows)
            }
        };

        match cursor_position {
            None => self.hide_cursor()?,
            Some(position) => {
                self.set_cursor_style(cursor_style)?;
                self.show_cursor()?;
                self.set_cursor_position(position)?;
            }
        }

        match dense_rows {
            Some(_) => self.swap_buffers_after_dense_rows(),
            None => self.swap_buffers(),
        }

        Backend::flush(&mut self.backend)?;

        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn draw_profiled<F>(&mut self, render_callback: F) -> io::Result<TerminalDrawTimings>
    where
        F: FnOnce(&mut Frame) -> FrameFlush,
    {
        self.try_draw_profiled(|frame| io::Result::Ok(render_callback(frame)))
    }

    #[cfg(test)]
    pub(crate) fn try_draw_profiled<F, E>(
        &mut self,
        render_callback: F,
    ) -> io::Result<TerminalDrawTimings>
    where
        F: FnOnce(&mut Frame) -> Result<FrameFlush, E>,
        E: Into<io::Error>,
    {
        let total_start = Instant::now();
        let started = Instant::now();
        self.autoresize()?;
        let autoresize = started.elapsed();

        let started = Instant::now();
        let (cursor_position, cursor_style, flush) = {
            let mut frame = self.get_frame();
            let flush = render_callback(&mut frame).map_err(Into::into)?;
            (frame.cursor_position, frame.cursor_style, flush)
        };
        let render = started.elapsed();

        let dense_rows = match flush {
            FrameFlush::Sparse => None,
            FrameFlush::Dense(rows) => Some(rows),
        };
        let (diff, encode, commands) = self.flush_profiled(dense_rows.clone())?;

        let started = Instant::now();
        match cursor_position {
            None => self.hide_cursor()?,
            Some(position) => {
                self.set_cursor_style(cursor_style)?;
                self.show_cursor()?;
                self.set_cursor_position(position)?;
            }
        }
        let cursor = started.elapsed();

        let started = Instant::now();
        match dense_rows {
            Some(_) => self.swap_buffers_after_dense_rows(),
            None => self.swap_buffers(),
        }
        let buffer_swap = started.elapsed();

        let started = Instant::now();
        Backend::flush(&mut self.backend)?;
        let backend_flush = started.elapsed();

        Ok(TerminalDrawTimings {
            autoresize,
            render,
            diff,
            encode,
            cursor,
            buffer_swap,
            backend_flush,
            total: total_start.elapsed(),
            commands,
        })
    }

    /// Hides the cursor.
    pub fn hide_cursor(&mut self) -> io::Result<()> {
        self.backend.hide_cursor()?;
        self.hidden_cursor = true;
        Ok(())
    }

    /// Shows the cursor.
    pub fn show_cursor(&mut self) -> io::Result<()> {
        self.backend.show_cursor()?;
        self.hidden_cursor = false;
        Ok(())
    }

    /// Sets the visible terminal cursor style.
    pub fn set_cursor_style(&mut self, style: SetCursorStyle) -> io::Result<()> {
        queue!(self.backend, style)
    }

    /// Restores the user-configured terminal cursor style.
    pub fn reset_cursor_style(&mut self) -> io::Result<()> {
        self.set_cursor_style(SetCursorStyle::DefaultUserShape)
    }

    /// Gets the current cursor position.
    ///
    /// This is the position of the cursor after the last draw call.
    #[allow(dead_code)]
    pub fn get_cursor_position(&mut self) -> io::Result<Position> {
        self.backend.get_cursor_position()
    }

    /// Sets the cursor position.
    pub fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        let position = position.into();
        self.backend.set_cursor_position(position)?;
        self.last_known_cursor_pos = position;
        Ok(())
    }

    /// Clear the terminal and force a full redraw on the next draw call.
    pub fn clear(&mut self) -> io::Result<()> {
        if self.viewport_area.is_empty() {
            return Ok(());
        }
        self.clear_after_position(self.viewport_area.as_position())
    }

    /// Clear from `position` through the end of the visible screen and force a full redraw.
    pub(crate) fn clear_after_position(&mut self, position: Position) -> io::Result<()> {
        self.backend.set_cursor_position(position)?;
        self.backend.clear_region(ClearType::AfterCursor)?;
        // Reset the back buffer to make sure the next update will redraw everything.
        self.previous_buffer_mut().reset();
        Ok(())
    }

    /// Force the next draw pass to repaint the entire viewport by resetting the
    /// diff buffer. Call this after raw terminal operations that move screen
    /// content outside ratatui's knowledge.
    pub fn invalidate_viewport(&mut self) {
        self.previous_buffer_mut().reset();
    }

    /// Clear terminal scrollback (if supported) and force a full redraw.
    pub fn clear_scrollback(&mut self) -> io::Result<()> {
        if self.viewport_area.is_empty() {
            return Ok(());
        }
        let home = Position { x: 0, y: 0 };
        // Use an explicit cursor-home around scrollback purge for terminals that
        // are sensitive to inline viewport cursor placement (e.g. Terminal.app).
        self.set_cursor_position(home)?;
        queue!(self.backend, Clear(crossterm::terminal::ClearType::Purge))?;
        self.set_cursor_position(home)?;
        std::io::Write::flush(&mut self.backend)?;
        self.previous_buffer_mut().reset();
        Ok(())
    }

    /// Clear the entire visible screen (not just the viewport) and force a full redraw.
    pub fn clear_visible_screen(&mut self) -> io::Result<()> {
        let home = Position { x: 0, y: 0 };
        // Some terminals (notably Terminal.app) behave more reliably if we pair ED2
        // with an explicit cursor-home before/after, matching the common `clear`
        // sequence (`CSI 2J` + `CSI H`).
        self.set_cursor_position(home)?;
        self.backend.clear_region(ClearType::All)?;
        self.set_cursor_position(home)?;
        std::io::Write::flush(&mut self.backend)?;
        self.visible_history_rows = 0;
        self.previous_buffer_mut().reset();
        Ok(())
    }

    /// Hard-reset scrollback + visible screen using an explicit ANSI sequence.
    ///
    /// Some terminals behave more reliably when purge + clear are emitted as a
    /// single ANSI sequence instead of separate backend commands.
    pub fn clear_scrollback_and_visible_screen_ansi(&mut self) -> io::Result<()> {
        if self.viewport_area.is_empty() {
            return Ok(());
        }

        // Reset scroll region + style state, home cursor, clear screen, purge scrollback.
        // The order matches the common shell `clear && printf '\\e[3J'` behavior.
        write!(self.backend, "\x1b[r\x1b[0m\x1b[H\x1b[2J\x1b[3J\x1b[H")?;
        std::io::Write::flush(&mut self.backend)?;
        self.last_known_cursor_pos = Position { x: 0, y: 0 };
        self.visible_history_rows = 0;
        self.previous_buffer_mut().reset();
        Ok(())
    }

    pub fn visible_history_rows(&self) -> u16 {
        self.visible_history_rows
    }

    pub(crate) fn note_history_rows_inserted(&mut self, inserted_rows: u16) {
        self.visible_history_rows = self
            .visible_history_rows
            .saturating_add(inserted_rows)
            .min(self.viewport_area.top());
    }

    /// Clears the inactive buffer and swaps it with the current buffer
    pub fn swap_buffers(&mut self) {
        self.previous_buffer_mut().reset();
        self.current = 1 - self.current;
    }

    fn swap_buffers_after_dense_rows(&mut self) {
        // Dense renderers clear their repaint rows before writing into the frame. The inactive
        // buffer is only the next render target; sparse draws render the full frame, and dense
        // draws clear their target rows before rendering.
        self.current = 1 - self.current;
    }

    /// Queries the real size of the backend.
    pub fn size(&self) -> io::Result<Size> {
        self.backend.size()
    }
}

use ratatui::buffer::Cell;

fn clear_buffer_rows(buffer: &mut Buffer, rows: Range<u16>) {
    let area = buffer.area;
    let width = usize::from(area.width);
    if width == 0 {
        return;
    }

    let start = rows.start.max(area.y);
    let end = rows.end.min(area.bottom());
    if start >= end {
        return;
    }

    let start = usize::from(start - area.y) * width;
    let end = usize::from(end - area.y) * width;
    buffer.content[start..end].fill(Cell::EMPTY);
}

#[derive(Debug, IsVariant)]
enum DrawCommand {
    Put { x: u16, y: u16, cell: Cell },
    ClearToEnd { x: u16, y: u16, bg: Color },
}

fn diff_buffers(a: &Buffer, b: &Buffer) -> Vec<DrawCommand> {
    let previous_buffer = &a.content;
    let next_buffer = &b.content;

    let width = usize::from(a.area.width);
    if width == 0 {
        return Vec::new();
    }

    let mut updates = Vec::new();
    for y in 0..a.area.height {
        let row_start = y as usize * a.area.width as usize;
        let row_end = row_start + width;
        let previous_row = &previous_buffer[row_start..row_end];
        let next_row = &next_buffer[row_start..row_end];
        if previous_row == next_row {
            continue;
        }
        let bg = next_row.last().map(|cell| cell.bg).unwrap_or(Color::Reset);

        // Scan the row to find the rightmost column that still matters: any non-space glyph,
        // any cell whose bg differs from the row’s trailing bg, or any cell with modifiers.
        // Multi-width glyphs extend that region through their full displayed width.
        // After that point the rest of the row can be cleared with a single ClearToEnd, a perf win
        // versus emitting multiple space Put commands.
        let last_nonblank_column = last_nonblank_column(next_row, bg).unwrap_or(0);

        // Cells invalidated by drawing/replacing preceding multi-width characters.
        let mut invalidated: usize = 0;
        let mut column = 0usize;
        while column < next_row.len() && column <= last_nonblank_column {
            let cell = &next_row[column];
            let width = display_width(cell.symbol());
            let step = width.max(1);

            let previous = &previous_row[column];
            if !cell.skip && (cell != previous || invalidated > 0) {
                updates.push(DrawCommand::Put {
                    x: a.area.x + column as u16,
                    y: a.area.y + y,
                    cell: cell.clone(),
                });
            }

            let affected_width = std::cmp::max(width, display_width(previous.symbol()));
            invalidated = std::cmp::max(affected_width, invalidated).saturating_sub(step);
            column += step;
        }

        if last_nonblank_column + 1 < next_row.len()
            && previous_row[last_nonblank_column + 1..] != next_row[last_nonblank_column + 1..]
        {
            updates.push(DrawCommand::ClearToEnd {
                x: a.area.x + (last_nonblank_column + 1) as u16,
                y: a.area.y + y,
                bg,
            });
        }
    }
    updates
}

#[derive(Default)]
struct DenseDrawStats {
    last_put: Option<Position>,
    commands: usize,
}

#[derive(Debug)]
struct DenseRun {
    // Terminal output bytes. Keep this as bytes so dense render does not rebuild
    // a UTF-8 text container just to pass it back to `Write::write_all`.
    bytes: Vec<u8>,
    end_col: usize,
    position: Position,
    width: u16,
    fg: Color,
    bg: Color,
    modifier: Modifier,
}

fn draw_dense_rows(
    writer: &mut impl Write,
    buffer: &Buffer,
    rows: Range<u16>,
) -> io::Result<DenseDrawStats> {
    let width = usize::from(buffer.area.width);
    if width == 0 {
        return Ok(DenseDrawStats::default());
    }

    let start = rows.start.max(buffer.area.y);
    let end = rows.end.min(buffer.area.bottom());
    if start >= end {
        return Ok(DenseDrawStats::default());
    }

    let capacity = width
        .saturating_mul(usize::from(end - start))
        .saturating_mul(4);
    let mut output = Vec::with_capacity(capacity);
    let stats = encode_dense_rows(&mut output, buffer, start..end, width)?;
    writer.write_all(&output)?;
    Ok(stats)
}

fn encode_dense_rows(
    writer: &mut impl Write,
    buffer: &Buffer,
    rows: Range<u16>,
    width: usize,
) -> io::Result<DenseDrawStats> {
    let mut stats = DenseDrawStats::default();
    let mut fg = Color::Reset;
    let mut current_bg = Color::Reset;
    let mut modifier = Modifier::empty();
    let mut next_pos: Option<Position> = None;
    let clear_to_bottom_bg = dense_clear_to_bottom_bg(buffer, rows.clone(), width);
    let mut run = DenseRun {
        bytes: Vec::with_capacity(width.saturating_mul(4)),
        end_col: 0,
        position: Position { x: 0, y: 0 },
        width: 0,
        fg: Color::Reset,
        bg: Color::Reset,
        modifier: Modifier::empty(),
    };

    if let Some(bg) = clear_to_bottom_bg {
        draw_dense_clear(
            writer,
            Position {
                x: buffer.area.x,
                y: rows.start,
            },
            bg,
            CrosstermClearType::FromCursorDown,
            &mut fg,
            &mut current_bg,
            &mut modifier,
            &mut next_pos,
        )?;
        stats.commands += 1;
    }

    for absolute_y in rows {
        let y = absolute_y - buffer.area.y;
        let row_start = y as usize * width;
        let row_end = row_start + width;
        let row = &buffer.content[row_start..row_end];
        let row_bg = row.last().map(|cell| cell.bg).unwrap_or(Color::Reset);
        let Some(last_nonblank_column) = last_nonblank_column(row, row_bg) else {
            if clear_to_bottom_bg.is_none() {
                draw_dense_clear(
                    writer,
                    Position {
                        x: buffer.area.x,
                        y: buffer.area.y + y,
                    },
                    row_bg,
                    CrosstermClearType::UntilNewLine,
                    &mut fg,
                    &mut current_bg,
                    &mut modifier,
                    &mut next_pos,
                )?;
                stats.commands += 1;
            }
            continue;
        };

        let mut column = 0usize;
        let mut run_active = false;
        while column < row.len() && column <= last_nonblank_column {
            let cell = &row[column];
            if cell.skip {
                column += 1;
                continue;
            }

            let cell_width = display_width(cell.symbol()).max(1);
            let position = Position {
                x: buffer.area.x + column as u16,
                y: buffer.area.y + y,
            };
            let cell_width_u16 = cell_width as u16;
            if run_active
                && run.end_col == column
                && run.fg == cell.fg
                && run.bg == cell.bg
                && run.modifier == cell.modifier
            {
                run.end_col = column + cell_width;
                run.width = run.width.saturating_add(cell_width_u16);
                run.bytes.extend_from_slice(cell.symbol().as_bytes());
            } else {
                if run_active {
                    draw_dense_run(
                        writer,
                        &run,
                        &mut fg,
                        &mut current_bg,
                        &mut modifier,
                        &mut next_pos,
                    )?;
                    stats.commands += 1;
                    run.bytes.clear();
                }
                run.bytes.extend_from_slice(cell.symbol().as_bytes());
                run.end_col = column + cell_width;
                run.position = position;
                run.width = cell_width_u16;
                run.fg = cell.fg;
                run.bg = cell.bg;
                run.modifier = cell.modifier;
                run_active = true;
            }
            stats.last_put = Some(position);
            column += cell_width;
        }
        if run_active {
            draw_dense_run(
                writer,
                &run,
                &mut fg,
                &mut current_bg,
                &mut modifier,
                &mut next_pos,
            )?;
            stats.commands += 1;
            run.bytes.clear();
        }

        if clear_to_bottom_bg.is_none() && last_nonblank_column + 1 < row.len() {
            draw_dense_clear(
                writer,
                Position {
                    x: buffer.area.x + (last_nonblank_column + 1) as u16,
                    y: buffer.area.y + y,
                },
                row_bg,
                CrosstermClearType::UntilNewLine,
                &mut fg,
                &mut current_bg,
                &mut modifier,
                &mut next_pos,
            )?;
            stats.commands += 1;
        }
    }

    queue!(
        writer,
        SetForegroundColor(crossterm::style::Color::Reset),
        SetBackgroundColor(crossterm::style::Color::Reset),
        SetAttribute(crossterm::style::Attribute::Reset),
    )?;

    Ok(stats)
}

fn dense_clear_to_bottom_bg(buffer: &Buffer, rows: Range<u16>, width: usize) -> Option<Color> {
    // Clearing down is only safe when the repaint range reaches the viewport bottom, and only
    // when every row has the same trailing background because the terminal clear uses one bg.
    if rows.end != buffer.area.bottom() || rows.end.saturating_sub(rows.start) <= 1 {
        return None;
    }

    let mut bg = None;
    for absolute_y in rows {
        let y = absolute_y - buffer.area.y;
        let row_start = y as usize * width;
        let row_end = row_start + width;
        let row_bg = buffer.content[row_start..row_end]
            .last()
            .map(|cell| cell.bg)
            .unwrap_or(Color::Reset);
        if bg.is_some_and(|bg| bg != row_bg) {
            return None;
        }
        bg = Some(row_bg);
    }
    bg
}

fn draw_dense_run(
    writer: &mut impl Write,
    run: &DenseRun,
    fg: &mut Color,
    current_bg: &mut Color,
    modifier: &mut Modifier,
    next_pos: &mut Option<Position>,
) -> io::Result<()> {
    if *next_pos != Some(run.position) {
        queue!(writer, MoveTo(run.position.x, run.position.y))?;
    }
    if run.modifier != *modifier {
        let diff = ModifierDiff {
            from: *modifier,
            to: run.modifier,
        };
        diff.queue(writer)?;
        *modifier = run.modifier;
    }
    if run.fg != *fg || run.bg != *current_bg {
        queue!(writer, SetColors(Colors::new(run.fg.into(), run.bg.into())))?;
        *fg = run.fg;
        *current_bg = run.bg;
    }

    writer.write_all(&run.bytes)?;
    *next_pos = Some(Position {
        x: run.position.x.saturating_add(run.width),
        y: run.position.y,
    });

    Ok(())
}

fn draw_dense_clear(
    writer: &mut impl Write,
    position: Position,
    bg: Color,
    clear_type: CrosstermClearType,
    fg: &mut Color,
    current_bg: &mut Color,
    modifier: &mut Modifier,
    next_pos: &mut Option<Position>,
) -> io::Result<()> {
    if *next_pos != Some(position) {
        queue!(writer, MoveTo(position.x, position.y))?;
    }
    if *modifier != Modifier::empty() || *fg != Color::Reset {
        queue!(writer, SetAttribute(crossterm::style::Attribute::Reset))?;
        *modifier = Modifier::empty();
        *fg = Color::Reset;
        *current_bg = Color::Reset;
    }
    if *current_bg != bg {
        queue!(writer, SetBackgroundColor(bg.into()))?;
        *current_bg = bg;
    }
    queue!(writer, Clear(clear_type))?;
    *next_pos = Some(position);
    Ok(())
}

fn last_nonblank_column(row: &[Cell], bg: Color) -> Option<usize> {
    row.iter()
        .rposition(|cell| {
            cell.symbol() != " " || cell.bg != bg || cell.modifier != Modifier::empty()
        })
        .map(|column| column + display_width(row[column].symbol()).saturating_sub(1))
}

fn draw<I>(writer: &mut impl Write, commands: I) -> io::Result<()>
where
    I: Iterator<Item = DrawCommand>,
{
    let mut fg = Color::Reset;
    let mut bg = Color::Reset;
    let mut modifier = Modifier::empty();
    let mut last_pos: Option<Position> = None;
    for command in commands {
        let (x, y) = match command {
            DrawCommand::Put { x, y, .. } => (x, y),
            DrawCommand::ClearToEnd { x, y, .. } => (x, y),
        };
        // Move the cursor if the previous location was not (x - 1, y)
        if !matches!(last_pos, Some(p) if x == p.x + 1 && y == p.y) {
            queue!(writer, MoveTo(x, y))?;
        }
        last_pos = Some(Position { x, y });
        match command {
            DrawCommand::Put { cell, .. } => {
                if cell.modifier != modifier {
                    let diff = ModifierDiff {
                        from: modifier,
                        to: cell.modifier,
                    };
                    diff.queue(writer)?;
                    modifier = cell.modifier;
                }
                if cell.fg != fg || cell.bg != bg {
                    queue!(
                        writer,
                        SetColors(Colors::new(cell.fg.into(), cell.bg.into()))
                    )?;
                    fg = cell.fg;
                    bg = cell.bg;
                }

                queue!(writer, Print(cell.symbol()))?;
            }
            DrawCommand::ClearToEnd { bg: clear_bg, .. } => {
                queue!(writer, SetAttribute(crossterm::style::Attribute::Reset))?;
                modifier = Modifier::empty();
                queue!(writer, SetBackgroundColor(clear_bg.into()))?;
                bg = clear_bg;
                queue!(writer, Clear(crossterm::terminal::ClearType::UntilNewLine))?;
            }
        }
    }

    queue!(
        writer,
        SetForegroundColor(crossterm::style::Color::Reset),
        SetBackgroundColor(crossterm::style::Color::Reset),
        SetAttribute(crossterm::style::Attribute::Reset),
    )?;

    Ok(())
}

/// The `ModifierDiff` struct is used to calculate the difference between two `Modifier`
/// values. This is useful when updating the terminal display, as it allows for more
/// efficient updates by only sending the necessary changes.
struct ModifierDiff {
    pub from: Modifier,
    pub to: Modifier,
}

impl ModifierDiff {
    fn queue<W: io::Write>(self, w: &mut W) -> io::Result<()> {
        use crossterm::style::Attribute as CAttribute;
        let removed = self.from - self.to;
        if removed.contains(Modifier::REVERSED) {
            queue!(w, SetAttribute(CAttribute::NoReverse))?;
        }
        if removed.contains(Modifier::BOLD) {
            queue!(w, SetAttribute(CAttribute::NormalIntensity))?;
            if self.to.contains(Modifier::DIM) {
                queue!(w, SetAttribute(CAttribute::Dim))?;
            }
        }
        if removed.contains(Modifier::ITALIC) {
            queue!(w, SetAttribute(CAttribute::NoItalic))?;
        }
        if removed.contains(Modifier::UNDERLINED) {
            queue!(w, SetAttribute(CAttribute::NoUnderline))?;
        }
        if removed.contains(Modifier::DIM) {
            queue!(w, SetAttribute(CAttribute::NormalIntensity))?;
        }
        if removed.contains(Modifier::CROSSED_OUT) {
            queue!(w, SetAttribute(CAttribute::NotCrossedOut))?;
        }
        if removed.contains(Modifier::SLOW_BLINK) || removed.contains(Modifier::RAPID_BLINK) {
            queue!(w, SetAttribute(CAttribute::NoBlink))?;
        }

        let added = self.to - self.from;
        if added.contains(Modifier::REVERSED) {
            queue!(w, SetAttribute(CAttribute::Reverse))?;
        }
        if added.contains(Modifier::BOLD) {
            queue!(w, SetAttribute(CAttribute::Bold))?;
        }
        if added.contains(Modifier::ITALIC) {
            queue!(w, SetAttribute(CAttribute::Italic))?;
        }
        if added.contains(Modifier::UNDERLINED) {
            queue!(w, SetAttribute(CAttribute::Underlined))?;
        }
        if added.contains(Modifier::DIM) {
            queue!(w, SetAttribute(CAttribute::Dim))?;
        }
        if added.contains(Modifier::CROSSED_OUT) {
            queue!(w, SetAttribute(CAttribute::CrossedOut))?;
        }
        if added.contains(Modifier::SLOW_BLINK) {
            queue!(w, SetAttribute(CAttribute::SlowBlink))?;
        }
        if added.contains(Modifier::RAPID_BLINK) {
            queue!(w, SetAttribute(CAttribute::RapidBlink))?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use ratatui::backend::WindowSize;
    use ratatui::layout::Rect;
    use ratatui::style::Style;

    struct CaptureBackend {
        output: Vec<u8>,
        size: Size,
        cursor: Position,
    }

    impl CaptureBackend {
        fn new(width: u16, height: u16) -> Self {
            Self {
                output: Vec::new(),
                size: Size { width, height },
                cursor: Position { x: 0, y: 0 },
            }
        }

        fn output(&self) -> String {
            String::from_utf8_lossy(&self.output).into_owned()
        }
    }

    impl Write for CaptureBackend {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.output.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Backend for CaptureBackend {
        fn draw<'a, I>(&mut self, _content: I) -> io::Result<()>
        where
            I: Iterator<Item = (u16, u16, &'a Cell)>,
        {
            Ok(())
        }

        fn hide_cursor(&mut self) -> io::Result<()> {
            Ok(())
        }

        fn show_cursor(&mut self) -> io::Result<()> {
            Ok(())
        }

        fn get_cursor_position(&mut self) -> io::Result<Position> {
            Ok(self.cursor)
        }

        fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
            self.cursor = position.into();
            Ok(())
        }

        fn clear(&mut self) -> io::Result<()> {
            Ok(())
        }

        fn clear_region(&mut self, _clear_type: ClearType) -> io::Result<()> {
            Ok(())
        }

        fn append_lines(&mut self, _line_count: u16) -> io::Result<()> {
            Ok(())
        }

        fn scroll_region_up(
            &mut self,
            _region: std::ops::Range<u16>,
            _scroll_by: u16,
        ) -> io::Result<()> {
            Ok(())
        }

        fn scroll_region_down(
            &mut self,
            _region: std::ops::Range<u16>,
            _scroll_by: u16,
        ) -> io::Result<()> {
            Ok(())
        }

        fn size(&self) -> io::Result<Size> {
            Ok(self.size)
        }

        fn window_size(&mut self) -> io::Result<WindowSize> {
            Ok(WindowSize {
                columns_rows: self.size,
                pixels: self.size,
            })
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn diff_buffers_skips_unchanged_blank_rows() {
        let area = Rect::new(0, 0, 3, 2);
        let previous = Buffer::empty(area);
        let next = Buffer::empty(area);

        let commands = diff_buffers(&previous, &next);

        assert!(
            commands.is_empty(),
            "expected no commands for identical buffers; commands: {commands:?}",
        );
    }

    #[test]
    fn diff_buffers_clear_to_end_only_when_trailing_cells_changed() {
        let area = Rect::new(0, 0, 10, 1);
        let mut previous = Buffer::empty(area);
        let mut next = Buffer::empty(area);

        previous.set_string(0, 0, "abc", Style::default());
        next.set_string(0, 0, "a", Style::default());

        let commands = diff_buffers(&previous, &next);
        assert!(
            commands
                .iter()
                .any(|command| matches!(command, DrawCommand::ClearToEnd { x: 1, y: 0, .. })),
            "expected clear-to-end after shortened content; commands: {commands:?}",
        );
        assert!(
            commands
                .iter()
                .all(|command| !matches!(command, DrawCommand::Put { x, y: 0, .. } if *x > 0)),
            "expected trailing cells to be cleared instead of put individually; commands: {commands:?}",
        );
    }

    #[test]
    fn diff_buffers_does_not_emit_clear_to_end_for_full_width_row() {
        let area = Rect::new(0, 0, 3, 2);
        let previous = Buffer::empty(area);
        let mut next = Buffer::empty(area);

        next.cell_mut((2, 0))
            .expect("cell should exist")
            .set_symbol("X");

        let commands = diff_buffers(&previous, &next);

        let clear_count = commands
            .iter()
            .filter(|command| matches!(command, DrawCommand::ClearToEnd { y, .. } if *y == 0))
            .count();
        assert_eq!(
            0, clear_count,
            "expected diff_buffers not to emit ClearToEnd; commands: {commands:?}",
        );
        assert!(
            commands
                .iter()
                .any(|command| matches!(command, DrawCommand::Put { x: 2, y: 0, .. })),
            "expected diff_buffers to update the final cell; commands: {commands:?}",
        );
    }

    #[test]
    fn diff_buffers_clear_to_end_starts_after_wide_char() {
        let area = Rect::new(0, 0, 10, 1);
        let mut previous = Buffer::empty(area);
        let mut next = Buffer::empty(area);

        previous.set_string(0, 0, "中文", Style::default());
        next.set_string(0, 0, "中", Style::default());

        let commands = diff_buffers(&previous, &next);
        assert!(
            commands
                .iter()
                .any(|command| matches!(command, DrawCommand::ClearToEnd { x: 2, y: 0, .. })),
            "expected clear-to-end to start after the remaining wide char; commands: {commands:?}"
        );
    }

    #[test]
    fn diff_buffers_updates_ascii_after_wide_char() {
        let area = Rect::new(0, 0, 10, 1);
        let previous = Buffer::empty(area);
        let mut next = Buffer::empty(area);

        next.set_string(0, 0, "中a", Style::default());

        let commands = diff_buffers(&previous, &next);
        assert!(
            commands.iter().any(
                |command| matches!(command, DrawCommand::Put { x: 0, y: 0, cell } if cell.symbol() == "中")
            ),
            "expected first wide char to be put; commands: {commands:?}"
        );
        assert!(
            commands.iter().any(
                |command| matches!(command, DrawCommand::Put { x: 2, y: 0, cell } if cell.symbol() == "a")
            ),
            "expected ascii after wide char to be put; commands: {commands:?}"
        );
    }

    #[test]
    fn dense_rows_rewrite_unicode_rows() {
        let area = Rect::new(0, 0, 80, 1);
        let mut next = Buffer::empty(area);
        let mut output = Vec::new();
        let text = "CJK 中 emoji 👩🏽‍💻 👍🏽 ไทย cafe\u{301}";

        next.set_string(0, 0, text, Style::default());

        let stats = draw_dense_rows(&mut output, &next, 0..1).expect("dense rows");
        let text_bytes = text.as_bytes();
        assert!(
            output
                .windows(text_bytes.len())
                .any(|bytes| bytes == text_bytes),
            "expected repaint to rewrite unicode row; output: {:?}",
            String::from_utf8_lossy(&output)
        );
        assert_eq!(
            2,
            stats.commands,
            "expected one text run plus trailing clear; output: {:?}",
            String::from_utf8_lossy(&output)
        );
    }

    #[test]
    fn dense_rows_clear_bottom_range_once() {
        let area = Rect::new(0, 0, 8, 3);
        let mut next = Buffer::empty(area);
        let mut output = Vec::new();

        next.set_string(0, 0, "first", Style::default());
        next.set_string(0, 1, "二行", Style::default());

        let stats = draw_dense_rows(&mut output, &next, 0..3).expect("dense rows");

        let mut clear_down = Vec::new();
        queue!(clear_down, Clear(CrosstermClearType::FromCursorDown)).expect("queue clear down");
        assert!(
            output
                .windows(clear_down.len())
                .any(|bytes| bytes == clear_down),
            "expected dense render to clear the bottom range once; output: {:?}",
            String::from_utf8_lossy(&output)
        );

        let mut clear_to_end = Vec::new();
        queue!(clear_to_end, Clear(CrosstermClearType::UntilNewLine)).expect("queue clear to end");
        assert!(
            !output
                .windows(clear_to_end.len())
                .any(|bytes| bytes == clear_to_end),
            "expected dense render to skip per-row tail clears; output: {:?}",
            String::from_utf8_lossy(&output)
        );
        assert_eq!(3, stats.commands);
    }

    #[test]
    fn dense_rows_skip_skip_cells() {
        let area = Rect::new(0, 0, 4, 1);
        let mut next = Buffer::empty(area);
        let mut output = Vec::new();

        next.set_string(0, 0, "abcd", Style::default());
        next.cell_mut((1, 0))
            .expect("cell should exist")
            .set_skip(true);

        let stats = draw_dense_rows(&mut output, &next, 0..1).expect("dense rows");

        assert!(
            !output.windows(1).any(|bytes| bytes == b"b"),
            "expected dense render not to write skipped cell; output: {:?}",
            String::from_utf8_lossy(&output)
        );
        assert_eq!(2, stats.commands);
    }

    #[test]
    fn dense_rows_update_flushed_rows_when_caller_clears_rows() {
        let area = Rect::new(0, 0, 4, 2);
        let mut terminal =
            Terminal::with_options(CaptureBackend::new(/*width*/ 4, /*height*/ 2))
                .expect("terminal");
        terminal.set_viewport_area(area);

        terminal
            .draw(|frame| {
                frame
                    .buffer_mut()
                    .set_string(0, 0, "aaaa", Style::default());
                frame
                    .buffer_mut()
                    .set_string(0, 1, "bbbb", Style::default());
            })
            .expect("initial draw");

        terminal
            .draw_with_flush(|frame| {
                frame.clear_rows(1..2);
                frame
                    .buffer_mut()
                    .set_string(0, 1, "cccc", Style::default());
                FrameFlush::Dense(1..2)
            })
            .expect("dense draw");

        fn previous_row_text(
            terminal: &Terminal<CaptureBackend>,
            area: Rect,
            row: usize,
        ) -> String {
            let start = row * usize::from(area.width);
            terminal.previous_buffer().content[start..start + usize::from(area.width)]
                .iter()
                .map(Cell::symbol)
                .collect::<String>()
        }
        assert_eq!("cccc", previous_row_text(&terminal, area, 1));

        terminal
            .draw_with_flush(|frame| {
                frame.clear_rows(1..2);
                frame
                    .buffer_mut()
                    .set_string(0, 1, "dddd", Style::default());
                FrameFlush::Dense(1..2)
            })
            .expect("second dense draw");

        assert_eq!("dddd", previous_row_text(&terminal, area, 1));
    }

    #[test]
    fn terminal_draw_applies_requested_cursor_style() {
        let mut output = Vec::new();
        let mut terminal =
            Terminal::with_options(CaptureBackend::new(/*width*/ 2, /*height*/ 1))
                .expect("terminal");
        terminal.set_viewport_area(Rect::new(0, 0, 2, 1));

        terminal
            .try_draw(|frame| {
                frame.set_cursor_style(SetCursorStyle::SteadyBar);
                frame.set_cursor_position((0, 0));
                io::Result::Ok(())
            })
            .expect("draw");

        queue!(output, SetCursorStyle::SteadyBar).expect("queue style");
        let expected = String::from_utf8(output).expect("utf8");
        let actual = terminal.backend().output();
        assert!(
            actual.contains(&expected),
            "expected terminal output to contain cursor style {expected:?}, got {actual:?}"
        );
    }

    #[test]
    fn reset_cursor_style_emits_default_user_shape() {
        let mut output = Vec::new();
        let mut terminal =
            Terminal::with_options(CaptureBackend::new(/*width*/ 2, /*height*/ 1))
                .expect("terminal");

        terminal.reset_cursor_style().expect("reset cursor style");
        ratatui::backend::Backend::flush(terminal.backend_mut()).expect("flush backend");

        queue!(output, SetCursorStyle::DefaultUserShape).expect("queue style");
        let expected = String::from_utf8(output).expect("utf8");
        let actual = terminal.backend().output();
        assert!(
            actual.contains(&expected),
            "expected terminal output to contain cursor style reset {expected:?}, got {actual:?}"
        );
    }
}
