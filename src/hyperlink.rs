//! OSC8 metadata for the PR chip, painted through the backend rather than widget text.

use std::io::{self, Write};
use std::time::Duration;

use ratatui::Terminal;
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::buffer::{Cell, CellDiffOption};
use ratatui::crossterm::cursor::{RestorePosition, SavePosition};
use ratatui::crossterm::queue;
use ratatui::layout::{Position, Rect};
use unicode_width::UnicodeWidthStr;

use crate::{app::App, ui};

const CLOSE: &[u8] = b"\x1b]8;;\x1b\\";

#[derive(Default)]
pub(crate) struct PrHyperlink {
    linked: bool,
}

impl PrHyperlink {
    pub(crate) fn draw<W: Write>(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<W>>,
        app: &App,
    ) -> io::Result<Option<Duration>> {
        let mut header = Vec::new();
        let mut link = None;
        let mut due = None;
        terminal.draw(|frame| {
            due = ui::render_frame(frame, app);
            link = ui::pr_header_link(frame.area(), app).map(|(area, url)| (area, url.to_owned()));
            if self.linked || link.is_some() {
                let area = frame.area();
                let mut next = area.x;
                for x in area.left()..area.right() {
                    if let Some(cell) = frame.buffer_mut().cell((x, area.y))
                        && x >= next
                        && cell.diff_option != CellDiffOption::Skip
                    {
                        next = x.saturating_add(cell.symbol().width().max(1) as u16);
                        header.push((x, area.y, cell.clone()));
                    }
                }
            }
        })?;
        if self.linked || link.is_some() {
            paint_header(terminal.backend_mut(), &header, link.as_ref())?;
        }
        self.linked = link.is_some();
        Ok(due)
    }
}

fn paint_header<W: Write>(
    backend: &mut CrosstermBackend<W>,
    header: &[(u16, u16, Cell)],
    link: Option<&(Rect, String)>,
) -> io::Result<()> {
    // Repaint the final row: ratatui's text diff cannot see changed or removed hyperlink metadata.
    let linked = |cell: &(u16, u16, Cell)| {
        link.is_some_and(|(area, _)| area.contains(Position::new(cell.0, cell.1)))
    };
    queue!(backend, SavePosition)?;
    let result = (|| {
        backend.write_all(CLOSE)?;
        for run in header.chunk_by(|a, b| linked(a) == linked(b)) {
            if linked(&run[0]) {
                let (_, url) = link.expect("linked cells have a URL");
                write!(backend, "\x1b]8;;{url}\x1b\\")?;
            }
            backend.draw(run.iter().map(|(x, y, cell)| (*x, *y, cell)))?;
            backend.write_all(CLOSE)?;
        }
        Ok(())
    })();
    let restore = (|| {
        backend.write_all(CLOSE)?;
        queue!(backend, RestorePosition)?;
        Write::flush(backend)
    })();
    result.and(restore)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::HashSet;
    use std::rc::Rc;

    use ratatui::{TerminalOptions, Viewport};
    use unicode_width::UnicodeWidthChar;

    use super::*;
    use crate::app::{Mode, Tab};
    use crate::forge::{Merge, PrSnapshot, PrState, PrView, Sync};
    use crate::model::Scope;

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

    impl Output {
        fn take(&self) -> String {
            String::from_utf8(std::mem::take(&mut *self.0.borrow_mut())).unwrap()
        }
    }

    fn pr_app() -> App {
        let mut app = App::new(".".into(), Scope::Uncommitted, None);
        app.tab = Tab::Pr;
        app.pr = PrView::Pr(Box::new(PrSnapshot {
            number: 7,
            title: "Review 界面".into(),
            url: "https://example.test/pull/7?a=1&b=2".into(),
            body: "Description outside the link".into(),
            state: PrState::Open,
            is_draft: false,
            head_ref: "feature".into(),
            head_is_fork: false,
            head_oid: String::new(),
            base_ref: "main".into(),
            merge: Merge::Clean,
            sync: Sync::InSync,
            checks: vec![],
            comments: vec![],
            comments_truncated: false,
            checks_truncated: false,
        }));
        app
    }

    fn terminal(output: &Output, width: u16) -> Terminal<CrosstermBackend<Output>> {
        Terminal::with_options(
            CrosstermBackend::new(output.clone()),
            TerminalOptions { viewport: Viewport::Fixed(Rect::new(3, 2, width, 12)) },
        )
        .unwrap()
    }

    fn printed(text: &str) -> String {
        let mut chars = text.chars();
        let mut out = String::new();
        while let Some(ch) = chars.next() {
            if ch == '\x1b' {
                assert_eq!(chars.next(), Some('['));
                for ch in chars.by_ref() {
                    if ('@'..='~').contains(&ch) {
                        break;
                    }
                }
            } else {
                out.push(ch);
            }
        }
        out
    }

    fn unlinked_writes(text: &str) -> HashSet<Position> {
        let mut chars = text.chars();
        let mut cursor = Position::ORIGIN;
        let mut saved = cursor;
        let mut linked = false;
        let mut writes = HashSet::new();
        while let Some(ch) = chars.next() {
            if ch == '\x1b' {
                match chars.next().unwrap() {
                    '[' => {
                        let mut params = String::new();
                        let command = loop {
                            let ch = chars.next().unwrap();
                            if ('@'..='~').contains(&ch) {
                                break ch;
                            }
                            params.push(ch);
                        };
                        match command {
                            'H' => {
                                let (row, col) = params.split_once(';').unwrap();
                                cursor = Position::new(
                                    col.parse::<u16>().unwrap() - 1,
                                    row.parse::<u16>().unwrap() - 1,
                                );
                            }
                            'm' | 'h' | 'l' | 'J' => {}
                            _ => panic!("unexpected CSI {params}{command}"),
                        }
                    }
                    ']' => {
                        let mut osc = String::new();
                        loop {
                            let ch = chars.next().unwrap();
                            if ch == '\x1b' {
                                assert_eq!(chars.next(), Some('\\'));
                                break;
                            }
                            osc.push(ch);
                        }
                        let url = osc.strip_prefix("8;;").expect("OSC8 link command");
                        linked = !url.is_empty();
                    }
                    '7' => saved = cursor,
                    '8' => cursor = saved,
                    command => panic!("unexpected escape {command:?}"),
                }
            } else {
                assert!(!ch.is_control(), "unexpected control {ch:?}");
                let width = ch.width().unwrap_or(0) as u16;
                if !linked {
                    writes.extend((cursor.x..cursor.x + width).map(|x| Position::new(x, cursor.y)));
                }
                cursor.x += width;
            }
        }
        writes
    }

    #[test]
    fn compact_pr_chip_has_osc8_metadata_even_when_only_its_url_changes() {
        let output = Output::default();
        let mut terminal = terminal(&output, 80);
        let mut renderer = PrHyperlink::default();
        let mut app = pr_app();
        for url in ["https://example.test/pull/7?a=1&b=2", "https://other.test/pull/7"] {
            let PrView::Pr(snapshot) = &mut app.pr else { unreachable!() };
            snapshot.url = url.into();
            renderer.draw(&mut terminal, &app).unwrap();
            let bytes = output.take();
            let open = format!("\x1b]8;;{url}\x1b\\");
            let (_, linked) = bytes.split_once(&open).expect("OSC8 target reaches the terminal");
            let (label, tail) = linked.split_once("\x1b]8;;\x1b\\").unwrap();
            assert_eq!(printed(label), "open #7 ↗", "only the compact chip is linked");
            assert!(!tail.contains("\x1b]8;;https://"), "no link leaks past the chip");
            assert!(bytes.ends_with("\x1b8"), "the backend restores the cursor");
        }
    }

    #[test]
    fn pr_hyperlink_tracks_resize_and_disappears_with_its_view_or_safe_url() {
        let output = Output::default();
        let mut terminal = terminal(&output, 80);
        let mut renderer = PrHyperlink::default();
        let app = pr_app();
        renderer.draw(&mut terminal, &app).unwrap();
        output.take();
        terminal.resize(Rect::new(3, 2, 46, 12)).unwrap();
        renderer.draw(&mut terminal, &app).unwrap();
        let bytes = output.take();
        let (_, linked) =
            bytes.split_once("\x1b]8;;https://example.test/pull/7?a=1&b=2\x1b\\").unwrap();
        assert_eq!(printed(linked.split_once("\x1b]8;;\x1b\\").unwrap().0), "open #7 ↗");

        let changes: [fn(&mut App); 6] = [
            |app| app.tab = Tab::Changes,
            |app| app.mode = Mode::Picker,
            |app| app.pr = PrView::NoPr,
            |app| app.set_config_error("invalid config".into()),
            |app| {
                let PrView::Pr(snapshot) = &mut app.pr else { unreachable!() };
                snapshot.url = "file:///etc/passwd".into();
            },
            |app| {
                let PrView::Pr(snapshot) = &mut app.pr else { unreachable!() };
                snapshot.url = "https://example.test/\x1b]8;;bad".into();
            },
        ];
        for change in changes {
            let mut app = pr_app();
            let (chip, _) = ui::pr_header_link(terminal.get_frame().area(), &app).unwrap();
            renderer.draw(&mut terminal, &app).unwrap();
            output.take();
            change(&mut app);
            renderer.draw(&mut terminal, &app).unwrap();
            let bytes = output.take();
            let writes = unlinked_writes(&bytes);
            for x in chip.left()..chip.right() {
                assert!(
                    writes.contains(&Position::new(x, chip.y)),
                    "former chip cell ({x}, {}) must be rewritten outside a hyperlink",
                    chip.y,
                );
            }
            assert!(!bytes.contains("\x1b]8;;https://"), "no stale or unsafe link");
        }
    }
}
