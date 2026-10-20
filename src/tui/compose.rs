use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Clear, Padding, Paragraph, Wrap},
};
use tui_textarea::TextArea;

use super::{hints, panel, popup, render_paragraph};
use crate::{
    output,
    post::{self, MAX_CHARS, Poster},
};

/// What a draft answers: a reply sits under its parent, a quote embeds it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Relation {
    Reply,
    Quote,
}

pub(super) struct Target {
    pub relation: Relation,
    pub id: String,
    pub username: String,
    /// `html_url`, what the GitHub-key endpoint takes as the reply reference.
    pub reference: String,
    pub excerpt: String,
}

#[derive(PartialEq, Eq)]
enum Stage {
    Writing,
    Review,
    Discard,
    Sending,
}

/// What the TUI should do with the reviewed draft.
pub(super) enum Action {
    Close,
    /// Sign with the GitHub key outside raw mode, then send to `/api/cli/commits`.
    SignWithKey,
    /// Send through the signed API.
    Send {
        body: String,
        parent_id: Option<String>,
        quoted_commit_id: Option<String>,
    },
}

pub(super) struct Composer {
    editor: TextArea<'static>,
    target: Option<Target>,
    poster: Poster,
    stage: Stage,
    error: Option<String>,
    scroll: u16,
}

impl Composer {
    pub fn new(poster: Poster, target: Option<Target>) -> Self {
        let mut editor = TextArea::default();
        editor.set_placeholder_text("Write your post…");
        editor.set_cursor_line_style(Style::default());
        Self {
            editor,
            target,
            poster,
            stage: Stage::Writing,
            error: None,
            scroll: 0,
        }
    }

    fn message(&self) -> String {
        self.editor.lines().join("\n")
    }

    /// For `Action::SignWithKey`: signs the draft with the GitHub key.
    pub fn sign(&self) -> Result<post::SignedPost> {
        let Poster::GitHubKey(key) = &self.poster else {
            anyhow::bail!("no GitHub key configured");
        };
        post::sign(
            &self.message(),
            self.target.as_ref().map(|target| target.reference.as_str()),
            key,
        )
    }

    pub fn failed(&mut self, error: String) {
        self.stage = Stage::Writing;
        self.error = Some(error);
    }

    fn send_action(&self) -> Action {
        match &self.poster {
            Poster::GitHubKey(_) => Action::SignWithKey,
            Poster::Session => {
                let body = self.message().trim().to_owned();
                let (parent_id, quoted_commit_id) = match &self.target {
                    Some(target) if target.relation == Relation::Reply => {
                        (Some(target.id.clone()), None)
                    }
                    Some(target) => (None, Some(target.id.clone())),
                    None => (None, None),
                };
                Action::Send {
                    body,
                    parent_id,
                    quoted_commit_id,
                }
            }
        }
    }

    pub fn key(&mut self, key: KeyEvent) -> Option<Action> {
        if key.kind == KeyEventKind::Release || self.stage == Stage::Sending {
            return None;
        }
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let cancel = key.code == KeyCode::Esc || (control && key.code == KeyCode::Char('c'));
        match self.stage {
            Stage::Writing => {
                if cancel {
                    if self.message().is_empty() {
                        return Some(Action::Close);
                    }
                    self.stage = Stage::Discard;
                } else if control && key.code == KeyCode::Char('s') {
                    match post::validate(&self.message()) {
                        Ok(_) => {
                            self.stage = Stage::Review;
                            self.scroll = 0;
                            self.error = None;
                        }
                        Err(error) => self.error = Some(error.to_string()),
                    }
                } else if self.editor.input(key) {
                    if self.message().chars().count() > MAX_CHARS {
                        self.editor.undo();
                        self.error = Some(format!("Maximum {MAX_CHARS} characters."));
                    } else {
                        self.error = None;
                    }
                }
            }
            Stage::Review => {
                if cancel {
                    self.stage = Stage::Writing;
                } else if key.code == KeyCode::Enter && key.kind == KeyEventKind::Press {
                    self.stage = Stage::Sending;
                    return Some(self.send_action());
                } else {
                    match key.code {
                        KeyCode::Down | KeyCode::PageDown => {
                            self.scroll = self.scroll.saturating_add(5)
                        }
                        KeyCode::Up | KeyCode::PageUp => {
                            self.scroll = self.scroll.saturating_sub(5)
                        }
                        _ => {}
                    }
                }
            }
            Stage::Discard => {
                if key.code == KeyCode::Esc {
                    self.stage = Stage::Writing;
                } else if matches!(key.code, KeyCode::Char('y' | 'Y')) {
                    return Some(Action::Close);
                }
            }
            Stage::Sending => {}
        }
        None
    }

    pub fn paste(&mut self, text: &str) {
        if self.stage != Stage::Writing {
            return;
        }
        let text = output::safe(&text.replace("\r\n", "\n").replace('\r', "\n"));
        if self.message().chars().count() + text.chars().count() > MAX_CHARS {
            self.error = Some(format!(
                "Paste exceeds {MAX_CHARS} characters; nothing inserted."
            ));
        } else {
            self.editor.insert_str(text);
            self.error = None;
        }
    }

    pub fn draw(&mut self, frame: &mut Frame) {
        let screen = frame.area();
        let area = popup(screen, 84, (screen.height * 3 / 5).clamp(10, 24));
        let subject = self.target.as_ref().map(|target| {
            format!(
                "{} @{}",
                match target.relation {
                    Relation::Reply => "reply to",
                    Relation::Quote => "quote of",
                },
                target.username
            )
        });
        let title = match self.stage {
            Stage::Writing => match &subject {
                Some(subject) => {
                    let mut s = subject.clone();
                    if let Some(first) = s.get_mut(..1) {
                        first.make_ascii_uppercase();
                    }
                    s
                }
                None => "New post".into(),
            },
            Stage::Review => format!("Review {}", subject.as_deref().unwrap_or("post")),
            Stage::Discard => "Discard draft?".into(),
            Stage::Sending => "Sending…".into(),
        };
        let count = self.message().chars().count();
        let counter = Line::styled(
            format!(" {count}/{MAX_CHARS} "),
            Style::default().fg(if count > MAX_CHARS {
                Color::Yellow
            } else {
                Color::DarkGray
            }),
        )
        .alignment(Alignment::Right);
        let hint = match self.stage {
            Stage::Writing => hints(&[
                ("Ctrl+S", "review"),
                ("Enter", "newline"),
                ("Esc", "cancel"),
            ]),
            Stage::Review => hints(&[("Enter", "send"), ("Esc", "edit"), ("PgUp/PgDn", "scroll")]),
            Stage::Discard => hints(&[("y", "discard"), ("Esc", "keep editing")]),
            Stage::Sending => Line::styled(
                " Sending… wait for confirmation before retrying ",
                Style::default().fg(Color::Yellow),
            ),
        };
        let block = panel(format!(" {} ", output::safe(&title)), true)
            .title_bottom(hint)
            .title_bottom(counter)
            .padding(Padding::horizontal(1));
        let inner = block.inner(area);
        frame.render_widget(Clear, area);
        frame.render_widget(block, area);
        // Context above the text: what is being answered, and at review time, how it is signed.
        let mut context: Vec<Line> = Vec::new();
        if self.stage == Stage::Discard {
            context.push(Line::raw("Your draft has not been sent."));
        } else {
            if let Some(target) = &self.target {
                context.push(Line::from(vec![
                    Span::styled(
                        format!("@{} ", output::safe(&target.username)),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(output::safe(&target.excerpt)),
                ]));
            }
            if self.stage != Stage::Writing {
                context.push(Line::raw(match &self.poster {
                    Poster::Session => "Posts as your signed-in account.".to_owned(),
                    Poster::GitHubKey(key) => format!("Signing key: {}", key.display()),
                }));
            }
        }
        let context_height = if context.is_empty() {
            0
        } else {
            context.len() as u16 + 1
        };
        let rows = Layout::vertical([
            Constraint::Length(context_height),
            Constraint::Min(1),
            Constraint::Length(if self.error.is_some() { 3 } else { 0 }),
        ])
        .split(inner);
        frame.render_widget(
            Paragraph::new(context).style(Style::default().fg(Color::Gray)),
            rows[0],
        );
        if self.stage == Stage::Writing {
            frame.render_widget(&self.editor, rows[1]);
        } else {
            render_paragraph(
                frame,
                rows[1],
                Paragraph::new(self.message()).wrap(Wrap { trim: false }),
                &mut self.scroll,
            );
        }
        if let Some(error) = &self.error {
            frame.render_widget(
                Paragraph::new(output::safe(error))
                    .style(Style::default().fg(Color::Yellow))
                    .wrap(Wrap { trim: false }),
                rows[2],
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
    fn review() -> KeyEvent {
        KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)
    }

    #[test]
    fn sending_requires_review_and_failures_keep_the_draft() {
        let mut composer = Composer::new(Poster::GitHubKey("test-key".into()), None);
        composer.paste("Hello");
        assert!(composer.key(key(KeyCode::Enter)).is_none());
        assert_eq!(composer.message(), "Hello\n");
        composer.key(review());
        composer.key(key(KeyCode::Esc));
        assert_eq!(composer.message(), "Hello\n");
        composer.key(review());
        assert!(matches!(
            composer.key(key(KeyCode::Enter)),
            Some(Action::SignWithKey)
        ));
        assert!(composer.key(key(KeyCode::Enter)).is_none());
        assert!(composer.key(key(KeyCode::Esc)).is_none());
        composer.failed("Could not confirm posting; check the website before retrying.".into());
        assert_eq!(composer.message(), "Hello\n");
        assert!(composer.key(key(KeyCode::Enter)).is_none());
    }

    #[test]
    fn session_replies_and_quotes_send_through_the_api() {
        let target = |relation| Target {
            relation,
            id: "parent-id".into(),
            username: "alice".into(),
            reference: "https://dev-null.sh/@alice/commits/parent-id".into(),
            excerpt: "Hello".into(),
        };
        for (relation, expect_parent) in [(Relation::Reply, true), (Relation::Quote, false)] {
            let mut composer = Composer::new(Poster::Session, Some(target(relation)));
            composer.paste("  A reply  ");
            composer.key(review());
            match composer.key(key(KeyCode::Enter)) {
                Some(Action::Send {
                    body,
                    parent_id,
                    quoted_commit_id,
                }) => {
                    assert_eq!(body, "A reply");
                    assert_eq!(parent_id.is_some(), expect_parent);
                    assert_eq!(quoted_commit_id.is_some(), !expect_parent);
                }
                _ => panic!("expected a send"),
            }
        }
    }

    #[test]
    fn paste_never_sends_and_discard_requires_confirmation() {
        let mut composer = Composer::new(Poster::Session, None);
        composer.paste("A draft\x13\nwith text");
        assert_eq!(composer.message(), "A draft\nwith text");
        assert!(composer.key(key(KeyCode::Esc)).is_none());
        composer.key(key(KeyCode::Esc));
        assert_eq!(composer.message(), "A draft\nwith text");
        composer.paste(&"x".repeat(4001));
        assert_eq!(composer.message(), "A draft\nwith text");
        composer.key(key(KeyCode::Esc));
        assert!(matches!(
            composer.key(key(KeyCode::Char('y'))),
            Some(Action::Close)
        ));
    }
}
