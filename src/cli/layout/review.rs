use std::{
    collections::BTreeSet,
    io::{self, IsTerminal},
};

use anyhow::Result;
use crossterm::{
    cursor::{Hide, Show},
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Layout},
    style::{Modifier, Style},
    text::Line,
    widgets::{Block, List, ListItem, ListState, Paragraph, Wrap},
};

use crate::layout::{CommandReview, LayoutReply, Store};

pub(super) fn interactive(json: bool, stdin: bool, stdout: bool) -> bool {
    !json && stdin && stdout
}

pub(super) fn run(store: &Store, reply: &LayoutReply) -> Result<()> {
    if reply.commands.is_empty() {
        println!("{}", empty_text(store)?);
        return Ok(());
    }
    if !interactive(false, io::stdin().is_terminal(), io::stdout().is_terminal()) {
        println!("{}", text(&reply.commands));
        return Ok(());
    }
    let confirmed = {
        let _guard = TerminalGuard::enter()?;
        let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
        let mut menu = ReviewMenu::new(&reply.commands);
        loop {
            terminal.draw(|frame| menu.render(frame))?;
            if let Event::Key(key) = event::read()? {
                match menu.key(key) {
                    Decision::Continue => {}
                    Decision::Cancel => break None,
                    Decision::Confirm => break Some(menu.selected),
                }
            }
        }
    };
    if let Some(selected) = confirmed {
        store.update(|saved| saved.confirm_review(&reply.commands, &selected))?;
        println!(
            "Saved command approvals updated on this host; unchecked commands restore as shells."
        );
    } else {
        println!("Review cancelled; saved approvals unchanged.");
    }
    Ok(())
}

pub(super) fn text(commands: &[CommandReview]) -> String {
    commands
        .iter()
        .map(details)
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn quoted(value: impl serde::Serialize) -> String {
    serde_json::to_string(&value).expect("review values serialize")
}

fn details(command: &CommandReview) -> String {
    format!(
        "{}  {}  {}\n  cwd: {}\n  executable: {}\n  argv: {}",
        if command.approved {
            "approved"
        } else {
            "pending"
        },
        command.fingerprint,
        command.project,
        quoted(command.cwd.to_string_lossy()),
        quoted(command.command.program.to_string_lossy()),
        quoted(&command.command.args),
    )
}

fn empty_text(store: &Store) -> Result<String> {
    let saved = store.read()?;
    let mut text = String::from(
        "No restart commands were captured. Affected panes restore as shells.\nRun `fut layout save` while the intended programs are running, then review again.",
    );
    for project in &saved.projects {
        for pane in project.panes() {
            if let Some(warning) = &pane.warning {
                text.push_str(&format!(
                    "\nCapture warning — {} / {} ({}): {}",
                    project.name,
                    pane.name,
                    quoted(pane.cwd.to_string_lossy()),
                    quoted(warning),
                ));
            }
        }
    }
    Ok(text)
}

#[derive(Debug, Eq, PartialEq)]
enum Decision {
    Continue,
    Cancel,
    Confirm,
}

struct ReviewMenu<'a> {
    commands: &'a [CommandReview],
    selected: BTreeSet<String>,
    list: ListState,
    detail_scroll: u16,
    confirming: bool,
}

impl<'a> ReviewMenu<'a> {
    fn new(commands: &'a [CommandReview]) -> Self {
        Self {
            commands,
            selected: commands
                .iter()
                .filter(|c| c.approved)
                .map(|c| c.fingerprint.clone())
                .collect(),
            list: ListState::default().with_selected(Some(0)),
            detail_scroll: 0,
            confirming: false,
        }
    }

    fn key(&mut self, key: KeyEvent) -> Decision {
        if key.kind == KeyEventKind::Release {
            return Decision::Continue;
        }
        if key.code == KeyCode::Esc
            || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
        {
            return Decision::Cancel;
        }
        if self.confirming {
            match key.code {
                KeyCode::Char('y' | 'Y') => return Decision::Confirm,
                KeyCode::Char('n' | 'N') | KeyCode::Backspace => self.confirming = false,
                _ => {}
            }
            return Decision::Continue;
        }
        let current = self.list.selected().unwrap_or(0);
        let last = self.commands.len().saturating_sub(1);
        let next = match key.code {
            KeyCode::Up | KeyCode::Char('k') => current.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => (current + 1).min(last),
            KeyCode::Home => 0,
            KeyCode::End => last,
            KeyCode::PageUp => {
                self.detail_scroll = self.detail_scroll.saturating_sub(5);
                current
            }
            KeyCode::PageDown => {
                self.detail_scroll = self.detail_scroll.saturating_add(5);
                current
            }
            KeyCode::Char(' ') => {
                let fingerprint = &self.commands[current].fingerprint;
                if !self.selected.remove(fingerprint) {
                    self.selected.insert(fingerprint.clone());
                }
                current
            }
            KeyCode::Enter => {
                self.confirming = true;
                current
            }
            _ => current,
        };
        if next != current {
            self.list.select(Some(next));
            self.detail_scroll = 0;
        }
        Decision::Continue
    }

    fn label(&self, command: &CommandReview) -> String {
        let selected = self.selected.contains(&command.fingerprint);
        let effect = match (command.approved, selected) {
            (true, true) => "keep approval",
            (true, false) => "revoke on confirm",
            (false, true) => "approve on confirm",
            (false, false) => "restore as shell",
        };
        format!(
            "[{}] {} | {} | {}",
            if selected { 'x' } else { ' ' },
            if command.approved {
                "approved"
            } else {
                "pending"
            },
            effect,
            command.project,
        )
    }

    fn render(&mut self, frame: &mut Frame) {
        let areas = Layout::vertical([
            Constraint::Length(3),
            Constraint::Percentage(35),
            Constraint::Min(3),
            Constraint::Length(4),
        ])
        .split(frame.area());
        frame.render_widget(
            Paragraph::new(vec![
                Line::from("Review saved restart commands")
                    .style(Style::default().add_modifier(Modifier::BOLD)),
                Line::from("Status is saved. Checkboxes apply only after confirmation."),
                Line::from(
                    "Unchecking an approved command revokes approval; it will restore as a shell.",
                ),
            ])
            .wrap(Wrap { trim: false }),
            areas[0],
        );
        let items: Vec<_> = self
            .commands
            .iter()
            .map(|c| ListItem::new(self.label(c)))
            .collect();
        let list = List::new(items)
            .block(Block::bordered().title(" Commands "))
            .highlight_symbol("> ")
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
        frame.render_stateful_widget(list, areas[1], &mut self.list);
        let detail = Paragraph::new(details(&self.commands[self.list.selected().unwrap_or(0)]))
            .wrap(Wrap { trim: false });
        let max_scroll = detail
            .line_count(areas[2].width.saturating_sub(2))
            .saturating_sub(usize::from(areas[2].height.saturating_sub(2)))
            .min(usize::from(u16::MAX)) as u16;
        self.detail_scroll = self.detail_scroll.min(max_scroll);
        frame.render_widget(
            detail
                .scroll((self.detail_scroll, 0))
                .block(Block::bordered().title(" Exact command · PgUp/PgDn to scroll ")),
            areas[2],
        );
        let footer = if self.confirming {
            let changes = self
                .commands
                .iter()
                .filter(|c| c.approved != self.selected.contains(&c.fingerprint))
                .count();
            format!(
                "Confirm approval changes? {changes} command entry/entries changed.\nChecked: approved. Unchecked: shell on restore. Running programs are unaffected.\ny: save approvals   n: back to checklist   Esc/Ctrl-C: cancel"
            )
        } else {
            "↑/↓ or j/k: navigate   Space: toggle   Enter: review changes\nPgUp/PgDn: scroll exact command   Home/End: first/last\nEsc/Ctrl-C: cancel without saving".into()
        };
        frame.render_widget(Paragraph::new(footer).wrap(Wrap { trim: false }), areas[3]);
    }
}

struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        let guard = Self;
        execute!(io::stdout(), EnterAlternateScreen, Hide)?;
        Ok(guard)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), Show, LeaveAlternateScreen);
        let _ = disable_raw_mode();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::RestartCommand;
    use ratatui::backend::TestBackend;

    fn commands() -> Vec<CommandReview> {
        [true, false, false]
            .into_iter()
            .enumerate()
            .map(|(i, approved)| {
                let command = RestartCommand {
                    program: "/bin/sleep".into(),
                    args: vec![format!("{i}")],
                };
                let cwd = std::path::PathBuf::from("/tmp/code");
                CommandReview {
                    fingerprint: command.fingerprint(&cwd),
                    project: format!("project-{i}"),
                    command,
                    cwd,
                    approved,
                }
            })
            .collect()
    }

    fn press(menu: &mut ReviewMenu, code: KeyCode) -> Decision {
        menu.key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn screen(menu: &mut ReviewMenu, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| menu.render(frame)).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .chunks(usize::from(width))
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn menu_shows_saved_status_and_unconfirmed_effects_for_every_command() {
        let commands = commands();
        let mut menu = ReviewMenu::new(&commands);
        assert_eq!(
            menu.selected,
            BTreeSet::from([commands[0].fingerprint.clone()])
        );
        let rendered = screen(&mut menu, 100, 30);
        assert!(rendered.contains("[x] approved | keep approval | project-0"));
        assert!(rendered.contains("[ ] pending | restore as shell | project-1"));
        assert!(rendered.contains("[ ] pending | restore as shell | project-2"));
        assert!(rendered.contains("cwd: \"/tmp/code\""));
        assert!(rendered.contains("executable: \"/bin/sleep\""));
        assert!(rendered.contains("argv: [\"0\"]"));
        press(&mut menu, KeyCode::Char(' '));
        press(&mut menu, KeyCode::Down);
        press(&mut menu, KeyCode::Char(' '));
        let rendered = screen(&mut menu, 100, 30);
        println!("{rendered}");
        assert!(rendered.contains("[ ] approved | revoke on confirm"));
        assert!(rendered.contains("[x] pending | approve on confirm"));
        assert!(commands[0].approved);
        assert!(!commands[1].approved);
    }

    #[test]
    fn multiple_selections_require_explicit_confirmation_and_can_be_cancelled() {
        let commands = commands();
        let mut menu = ReviewMenu::new(&commands);
        press(&mut menu, KeyCode::Down);
        press(&mut menu, KeyCode::Char(' '));
        press(&mut menu, KeyCode::Down);
        press(&mut menu, KeyCode::Char(' '));
        assert_eq!(menu.selected.len(), 3);
        assert_eq!(press(&mut menu, KeyCode::Char('y')), Decision::Continue);
        assert_eq!(press(&mut menu, KeyCode::Enter), Decision::Continue);
        assert!(screen(&mut menu, 80, 24).contains("Confirm approval changes?"));
        assert_eq!(press(&mut menu, KeyCode::Enter), Decision::Continue);
        assert_eq!(press(&mut menu, KeyCode::Char('n')), Decision::Continue);
        assert!(!menu.confirming);
        press(&mut menu, KeyCode::Enter);
        assert_eq!(press(&mut menu, KeyCode::Esc), Decision::Cancel);
        assert_eq!(ReviewMenu::new(&commands).selected.len(), 1);
        assert_eq!(press(&mut menu, KeyCode::Char('y')), Decision::Confirm);
    }

    #[test]
    fn duplicate_commands_share_a_single_approval_decision() {
        let mut commands = commands();
        commands.push(commands[0].clone());
        let mut menu = ReviewMenu::new(&commands);
        press(&mut menu, KeyCode::Char(' '));
        assert!(
            menu.label(&commands[3])
                .starts_with("[ ] approved | revoke on confirm")
        );
        press(&mut menu, KeyCode::End);
        press(&mut menu, KeyCode::Char(' '));
        assert!(
            menu.label(&commands[0])
                .starts_with("[x] approved | keep approval")
        );
    }

    #[test]
    fn exact_arguments_are_escaped_and_scrollable_at_narrow_sizes() {
        let mut commands = commands();
        commands[0].command.args = vec![
            "\x1b[2J\n".into(),
            "long argument ".repeat(100),
            "last-argument".into(),
        ];
        let mut menu = ReviewMenu::new(&commands);
        let rendered = screen(&mut menu, 40, 24);
        assert!(rendered.contains("approved"));
        for _ in 0..100 {
            press(&mut menu, KeyCode::PageDown);
            screen(&mut menu, 40, 24);
        }
        assert!(screen(&mut menu, 40, 24).contains("last-argument"));
        assert!(!details(&commands[0]).contains('\x1b'));
        assert!(details(&commands[0]).contains("\\u001b[2J\\n"));
        for (width, height) in [(1, 1), (20, 8), (80, 24)] {
            screen(&mut menu, width, height);
        }
    }

    #[test]
    fn json_or_redirected_streams_never_open_a_menu() {
        for json in [false, true] {
            for stdin in [false, true] {
                for stdout in [false, true] {
                    assert_eq!(interactive(json, stdin, stdout), !json && stdin && stdout);
                }
            }
        }
        assert!(text(&commands()).contains("pending"));
    }
}
