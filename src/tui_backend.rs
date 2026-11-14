//! Present changed cells and the final cursor atomically; leave idle frames silent.

use std::io::{self, Write};

use ratatui::backend::{Backend, ClearType, CrosstermBackend, WindowSize};
use ratatui::buffer::Cell;
use ratatui::crossterm::{
    cursor::{Hide, MoveTo, Show},
    queue,
    terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate},
};
use ratatui::layout::{Position, Size};

pub(crate) struct TuiBackend<W: Write> {
    inner: CrosstermBackend<W>,
    position: Option<Position>,
    visible: Option<bool>,
    presenting: bool,
}

impl<W: Write> TuiBackend<W> {
    pub(crate) fn new(writer: W) -> Self {
        Self {
            inner: CrosstermBackend::new(writer),
            position: None,
            visible: None,
            presenting: false,
        }
    }

    fn begin(&mut self) -> io::Result<()> {
        if !self.presenting {
            queue!(self.inner, BeginSynchronizedUpdate)?;
            self.presenting = true;
        }
        Ok(())
    }
}

impl<W: Write> Backend for TuiBackend<W> {
    type Error = io::Error;

    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        let mut content = content.peekable();
        if content.peek().is_none() {
            return Ok(());
        }
        self.begin()?;
        // Painting moves the hardware cursor independently of the requested cursor.
        self.position = None;
        self.inner.draw(content)
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        if self.visible != Some(false) {
            self.begin()?;
            queue!(self.inner, Hide)?;
            self.visible = Some(false);
        }
        Ok(())
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        if self.visible != Some(true) {
            self.begin()?;
            queue!(self.inner, Show)?;
            self.visible = Some(true);
        }
        Ok(())
    }

    fn get_cursor_position(&mut self) -> io::Result<Position> {
        Backend::flush(self)?;
        let position = self.inner.get_cursor_position()?;
        self.position = Some(position);
        Ok(position)
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        let position = position.into();
        if self.position != Some(position) {
            self.begin()?;
            queue!(self.inner, MoveTo(position.x, position.y))?;
            self.position = Some(position);
        }
        Ok(())
    }

    fn clear(&mut self) -> io::Result<()> {
        self.begin()?;
        self.position = None;
        self.inner.clear()
    }

    fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
        self.begin()?;
        self.position = None;
        self.inner.clear_region(clear_type)
    }

    fn append_lines(&mut self, n: u16) -> io::Result<()> {
        self.begin()?;
        self.position = None;
        self.inner.append_lines(n)
    }

    fn size(&self) -> io::Result<Size> {
        self.inner.size()
    }

    fn window_size(&mut self) -> io::Result<WindowSize> {
        self.inner.window_size()
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.presenting {
            queue!(self.inner, EndSynchronizedUpdate)?;
            self.presenting = false;
        }
        Backend::flush(&mut self.inner)
    }
}

impl<W: Write> Drop for TuiBackend<W> {
    fn drop(&mut self) {
        // Ratatui can restore cursor visibility on drop, outside a draw call.
        let _ = Backend::flush(self);
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use ratatui::layout::Rect;
    use ratatui::{Terminal, TerminalOptions, Viewport};

    use super::*;

    #[derive(Clone, Default)]
    struct Output(Rc<RefCell<Vec<u8>>>);

    impl Write for Output {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.borrow_mut().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn terminal(output: &Output) -> Terminal<TuiBackend<Output>> {
        Terminal::with_options(
            TuiBackend::new(output.clone()),
            TerminalOptions {
                viewport: Viewport::Fixed(Rect::new(0, 0, 20, 5)),
            },
        )
        .unwrap()
    }

    fn draw(terminal: &mut Terminal<TuiBackend<Output>>, text: &str, cursor: Option<(u16, u16)>) {
        terminal
            .draw(|frame| {
                frame.render_widget(text, frame.area());
                if let Some(cursor) = cursor {
                    frame.set_cursor_position(cursor);
                }
            })
            .unwrap();
    }

    #[test]
    fn idle_frames_do_not_disturb_visible_or_hidden_cursor() {
        let output = Output::default();
        let mut terminal = terminal(&output);
        for cursor in [Some((3, 2)), None] {
            draw(&mut terminal, "prompt", cursor);
            output.0.borrow_mut().clear();
            for _ in 0..100 {
                draw(&mut terminal, "prompt", cursor);
            }
            assert!(
                output.0.borrow().is_empty(),
                "idle frames wrote terminal commands"
            );
        }
    }

    #[test]
    fn painting_restores_cursor_without_reissuing_visibility() {
        let output = Output::default();
        let mut terminal = terminal(&output);
        draw(&mut terminal, "first", Some((3, 2)));
        let mut parser = vt100::Parser::new(5.try_into().unwrap(), 20.try_into().unwrap(), 0);
        parser.process(&output.0.borrow());
        output.0.borrow_mut().clear();

        draw(&mut terminal, "changed", Some((3, 2)));
        let bytes = output.0.borrow();
        assert!(bytes.starts_with(b"\x1b[?2026h"));
        assert!(bytes.ends_with(b"\x1b[?2026l"));
        assert!(!bytes.windows(6).any(|bytes| bytes == b"\x1b[?25h"));
        parser.process(&bytes);
        assert_eq!(parser.screen().contents(), "changed");
        assert_eq!(parser.screen().cursor_position(), (2, 3));
        assert!(!parser.screen().hide_cursor());
    }

    #[test]
    fn cursor_only_changes_and_resize_reach_the_terminal() {
        let output = Output::default();
        let mut terminal = terminal(&output);
        let mut parser = vt100::Parser::new(5.try_into().unwrap(), 20.try_into().unwrap(), 0);
        for cursor in [Some((3, 2)), Some((5, 3)), None, Some((5, 3))] {
            draw(&mut terminal, "prompt", cursor);
            parser.process(&output.0.borrow());
            output.0.borrow_mut().clear();
            assert_eq!(parser.screen().hide_cursor(), cursor.is_none());
            if let Some((x, y)) = cursor {
                assert_eq!(parser.screen().cursor_position(), (y, x));
            }
        }
        terminal.resize(Rect::new(0, 0, 18, 4)).unwrap();
        draw(&mut terminal, "resized", Some((2, 1)));
        parser.process(&output.0.borrow());
        assert_eq!(parser.screen().contents().trim_end(), "resized");
        assert_eq!(parser.screen().cursor_position(), (1, 2));
        assert!(!parser.screen().hide_cursor());
    }
}
