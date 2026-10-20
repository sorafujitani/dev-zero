mod compose;

use std::{io, path::PathBuf, sync::mpsc, thread, time::Duration};

use anyhow::Result;
use crossterm::{
    event::{
        self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers,
    },
    execute,
};
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Padding, Paragraph, Wrap},
};
use tui_textarea::{CursorMove, TextArea};

use crate::{
    api::{self, Api},
    model::{Commit, CommitId, Cursor, Feed, Log, Meta, Page, User, Username},
    output,
    post::{Poster, SignedPost},
};
use compose::{Composer, Relation, Target};

/// Where the list comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Source {
    User {
        username: Username,
        feed: Feed,
    },
    Log(Log),
    /// A commit followed by its replies, newest first.
    Thread(CommitId),
    Saved,
}

impl Source {
    fn needs_session(&self) -> bool {
        !matches!(self, Self::User { .. })
    }

    fn label(&self) -> String {
        match self {
            Self::User { username, feed } => format!("@{username} · {}", feed.label()),
            Self::Log(log) => log.label().to_owned(),
            Self::Thread(_) => "Thread".to_owned(),
            Self::Saved => "Saved".to_owned(),
        }
    }
}

#[derive(Clone)]
struct Request {
    source: Source,
    cursor: Option<Cursor>,
    /// Also fetch the profile (User sources only).
    profile: bool,
}

impl Request {
    fn first_page(source: Source) -> Self {
        let profile = matches!(source, Source::User { .. });
        Self {
            source,
            cursor: None,
            profile,
        }
    }
}

struct Fetched {
    profile: Option<User>,
    page: Page,
    requests_left: Option<u32>,
}

enum Work {
    Fetch(Request),
    PostWithKey(SignedPost),
    Create {
        body: String,
        parent_id: Option<String>,
        quoted_commit_id: Option<String>,
    },
    Star(CommitId, bool),
    Save(CommitId, bool),
    Follow(Username, bool),
    Delete(CommitId),
    Notifications,
    Me,
}

enum Reply {
    Fetched(Request, Result<Box<Fetched>>),
    Posted(Result<String>),
    Starred(String, bool, Result<()>),
    Saved(String, bool, Result<()>),
    Followed(String, bool, Result<()>),
    Deleted(String, Result<()>),
    Text(&'static str, Result<String>),
}

enum Action {
    Work(Work),
    /// Sign the draft with the GitHub key outside raw mode.
    SignWithKey,
}

struct Dialog {
    title: String,
    text: String,
    scroll: u16,
}

#[derive(Default)]
enum View {
    #[default]
    Browse,
    Username(Box<TextArea<'static>>),
    Search(Box<TextArea<'static>>),
    Compose(Box<Composer>),
    Dialog(Dialog),
    ConfirmDelete(CommitId),
}

#[derive(Default)]
struct App {
    source: Option<Source>,
    history: Vec<Source>,
    profile: Option<User>,
    page: Option<Page>,
    requests_left: Option<u32>,
    selection: ListState,
    scroll: u16,
    detail_focus: bool,
    /// Case-insensitive text every listed post must contain; empty shows the whole page.
    filter: String,
    view: View,
    poster: Option<Poster>,
    /// The signed-in username, when the API is signed.
    me: Option<String>,
    loading: bool,
    status: String,
    quit: bool,
}

impl App {
    fn signed(&self) -> bool {
        self.me.is_some()
    }

    fn finish(&mut self, reply: Reply) {
        self.loading = false;
        match reply {
            Reply::Fetched(request, result) => match result {
                Ok(fetched) => {
                    let Fetched {
                        profile,
                        page,
                        requests_left,
                    } = *fetched;
                    if let Some(previous) = self.source.take()
                        && previous != request.source
                    {
                        self.history.push(previous);
                        self.history.truncate(20);
                    }
                    self.source = Some(request.source);
                    self.requests_left = requests_left;
                    if let Some(profile) = profile {
                        self.profile = Some(profile);
                    }
                    self.filter.clear();
                    self.selection.select((!page.data.is_empty()).then_some(0));
                    self.scroll = 0;
                    self.detail_focus = false;
                    self.page = Some(page);
                    self.status.clear();
                }
                Err(error) => self.status = format!("{error:#}"),
            },
            Reply::Posted(Ok(message)) => self.dialog("Posted", message),
            Reply::Posted(Err(error)) => {
                if let View::Compose(composer) = &mut self.view {
                    composer.failed(format!("Check the website before retrying.\n{error:#}"));
                }
            }
            Reply::Starred(id, on, result) => match result {
                Ok(()) => {
                    if let Some(commit) = self.commit_mut(&id) {
                        commit.starred = Some(on);
                        commit.stars_count = if on {
                            commit.stars_count.saturating_add(1)
                        } else {
                            commit.stars_count.saturating_sub(1)
                        };
                    }
                }
                Err(error) => self.status = format!("{error:#}"),
            },
            Reply::Saved(id, on, result) => match result {
                Ok(()) => {
                    if let Some(commit) = self.commit_mut(&id) {
                        commit.is_saved = Some(on);
                    }
                    self.status = if on { "Saved." } else { "Removed from saved." }.into();
                }
                Err(error) => self.status = format!("{error:#}"),
            },
            Reply::Followed(username, on, result) => match result {
                Ok(()) => {
                    if let Some(profile) = &mut self.profile
                        && profile.username == username
                    {
                        profile.is_followed = Some(on);
                        if let Some(count) = &mut profile.followers_count {
                            *count = if on {
                                count.saturating_add(1)
                            } else {
                                count.saturating_sub(1)
                            };
                        }
                    }
                    if let Some(page) = &mut self.page {
                        for author in page.data.iter_mut().filter_map(|c| c.author.as_mut()) {
                            if author.username == username {
                                author.is_followed = Some(on);
                            }
                        }
                    }
                    self.status = format!(
                        "{} @{username}.",
                        if on { "Following" } else { "Unfollowed" }
                    );
                }
                Err(error) => self.status = format!("{error:#}"),
            },
            Reply::Deleted(id, result) => match result {
                Ok(()) => {
                    if let Some(page) = &mut self.page {
                        page.data.retain(|commit| commit.id != id);
                    }
                    let count = self.visible().len();
                    self.selection.select(
                        (count > 0).then(|| self.selection.selected().unwrap_or(0).min(count - 1)),
                    );
                    self.status = "Deleted.".into();
                }
                Err(error) => self.status = format!("{error:#}"),
            },
            Reply::Text(title, Ok(text)) => self.dialog(title, text),
            Reply::Text(_, Err(error)) => self.status = format!("{error:#}"),
        }
    }

    fn commit_mut(&mut self, id: &str) -> Option<&mut Commit> {
        self.page.as_mut()?.data.iter_mut().find(|c| c.id == id)
    }

    fn dialog(&mut self, title: &str, text: String) {
        self.view = View::Dialog(Dialog {
            title: title.into(),
            text,
            scroll: 0,
        });
    }

    fn username_prompt(&mut self) {
        let current = match &self.source {
            Some(Source::User { username, .. }) => username.to_string(),
            _ => String::new(),
        };
        let mut input = TextArea::new(vec![current]);
        input.move_cursor(CursorMove::End);
        input.set_cursor_line_style(Style::default());
        input.set_placeholder_text("GitHub username");
        self.view = View::Username(Box::new(input));
        self.status.clear();
    }

    fn search_prompt(&mut self) {
        let mut input = TextArea::new(vec![self.filter.clone()]);
        input.move_cursor(CursorMove::End);
        input.set_cursor_line_style(Style::default());
        input.set_placeholder_text("Filter this page");
        self.view = View::Search(Box::new(input));
        self.status.clear();
    }

    fn set_filter(&mut self, filter: &str) {
        if self.filter != filter {
            self.filter = filter.to_owned();
            self.selection
                .select((!self.visible().is_empty()).then_some(0));
            self.scroll = 0;
        }
    }

    /// Posts on the current page that match the filter, in page order.
    fn visible(&self) -> Vec<&Commit> {
        let Some(page) = &self.page else {
            return Vec::new();
        };
        if self.filter.is_empty() {
            return page.data.iter().collect();
        }
        let needle = self.filter.to_lowercase();
        page.data
            .iter()
            .filter(|commit| {
                commit.body.to_lowercase().contains(&needle)
                    || commit.author_name().to_lowercase().contains(&needle)
            })
            .collect()
    }

    fn selected(&self) -> Option<&Commit> {
        self.visible().get(self.selection.selected()?).copied()
    }

    fn selected_id(&self) -> Option<CommitId> {
        self.selected()?.id.parse().ok()
    }

    /// The user an `f`/`a`/`u` key acts on: the selected author, else the shown profile.
    fn focus_user(&self) -> Option<&User> {
        self.selected()
            .and_then(|commit| commit.author.as_ref())
            .or(self.profile.as_ref())
    }

    fn compose(&mut self, relation: Option<Relation>) {
        let Some(poster) = self.poster.clone() else {
            self.status = "Run dz login to post, or set DZ_SSH_KEY / --key.".into();
            return;
        };
        if relation == Some(Relation::Quote) && poster != Poster::Session {
            self.status = "Quoting needs a session; run dz login.".into();
            return;
        }
        let target = match relation {
            None => None,
            Some(relation) => {
                let Some(commit) = self.selected().filter(|c| !c.is_deleted()) else {
                    return;
                };
                Some(Target {
                    relation,
                    id: commit.id.clone(),
                    username: commit.author_name().to_owned(),
                    reference: commit.html_url.clone(),
                    excerpt: output::safe(commit.body.lines().next().unwrap_or(""))
                        .chars()
                        .take(100)
                        .collect(),
                })
            }
        };
        self.view = View::Compose(Box::new(Composer::new(poster, target)));
        self.status.clear();
    }

    fn open(&self, source: Source) -> Option<Action> {
        if source.needs_session() && !self.signed() {
            return None;
        }
        Some(Action::Work(Work::Fetch(Request::first_page(source))))
    }

    fn key(&mut self, key: KeyEvent) -> Option<Action> {
        if key.kind == KeyEventKind::Release {
            return None;
        }
        let interrupt =
            key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL);
        match &mut self.view {
            View::Compose(composer) => {
                match composer.key(key) {
                    Some(compose::Action::Close) => self.view = View::Browse,
                    Some(compose::Action::SignWithKey) => return Some(Action::SignWithKey),
                    Some(compose::Action::Send {
                        body,
                        parent_id,
                        quoted_commit_id,
                    }) => {
                        return Some(Action::Work(Work::Create {
                            body,
                            parent_id,
                            quoted_commit_id,
                        }));
                    }
                    None => {}
                }
                return None;
            }
            View::Username(input) => {
                match key.code {
                    _ if interrupt => self.quit = true,
                    KeyCode::Esc => {
                        self.view = View::Browse;
                        if self.page.is_none() {
                            self.quit = true;
                        }
                    }
                    KeyCode::Enter => match input.lines()[0].parse::<Username>() {
                        Ok(username) => {
                            self.view = View::Browse;
                            return self.open(Source::User {
                                username,
                                feed: Feed::Commits,
                            });
                        }
                        Err(error) => self.status = error.to_string(),
                    },
                    _ => {
                        if input.input_without_shortcuts(key) && input.lines()[0].len() > 129 {
                            input.undo();
                        }
                    }
                }
                return None;
            }
            View::Search(input) => {
                match key.code {
                    _ if interrupt => self.quit = true,
                    KeyCode::Esc => {
                        self.view = View::Browse;
                        self.set_filter("");
                    }
                    KeyCode::Enter => self.view = View::Browse,
                    _ => {
                        if input.input_without_shortcuts(key) {
                            if input.lines()[0].chars().count() > 100 {
                                input.undo();
                            }
                            let filter = input.lines()[0].clone();
                            self.set_filter(&filter);
                        }
                    }
                }
                return None;
            }
            View::Dialog(dialog) => {
                match key.code {
                    _ if interrupt => self.quit = true,
                    KeyCode::Esc
                    | KeyCode::Enter
                    | KeyCode::Char('q' | '?' | 'i' | 'u' | 'N' | 'm') => self.view = View::Browse,
                    KeyCode::Down | KeyCode::Char('j') => {
                        dialog.scroll = dialog.scroll.saturating_add(1)
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        dialog.scroll = dialog.scroll.saturating_sub(1)
                    }
                    KeyCode::PageDown => dialog.scroll = dialog.scroll.saturating_add(10),
                    KeyCode::PageUp => dialog.scroll = dialog.scroll.saturating_sub(10),
                    _ => {}
                }
                return None;
            }
            View::ConfirmDelete(id) => {
                let id = id.clone();
                match key.code {
                    _ if interrupt => self.quit = true,
                    KeyCode::Char('y' | 'Y') => {
                        self.view = View::Browse;
                        self.loading = true;
                        return Some(Action::Work(Work::Delete(id)));
                    }
                    KeyCode::Esc | KeyCode::Char('n' | 'q') => self.view = View::Browse,
                    _ => {}
                }
                return None;
            }
            View::Browse => {}
        }
        match key.code {
            _ if interrupt => self.quit = true,
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Esc if !self.status.is_empty() => self.status.clear(),
            KeyCode::Esc if !self.filter.is_empty() => self.set_filter(""),
            KeyCode::Esc if self.detail_focus => {
                self.detail_focus = false;
                self.scroll = 0;
            }
            KeyCode::Esc | KeyCode::Backspace if !self.history.is_empty() && !self.loading => {
                let source = self.history.pop()?;
                // Going back is not a new place: drop the current source instead of stacking it.
                self.source = None;
                return Some(Action::Work(Work::Fetch(Request::first_page(source))));
            }
            KeyCode::Esc => self.quit = true,
            KeyCode::Tab => self.detail_focus = !self.detail_focus,
            KeyCode::Enter => self.detail_focus = true,
            KeyCode::PageDown => self.scroll = self.scroll.saturating_add(10),
            KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(10),
            KeyCode::Down | KeyCode::Char('j') if self.detail_focus => {
                self.scroll = self.scroll.saturating_add(1)
            }
            KeyCode::Up | KeyCode::Char('k') if self.detail_focus => {
                self.scroll = self.scroll.saturating_sub(1)
            }
            KeyCode::Down | KeyCode::Char('j') => self.select(1),
            KeyCode::Up | KeyCode::Char('k') => self.select(-1),
            KeyCode::Char('g') => {
                self.selection
                    .select((!self.visible().is_empty()).then_some(0));
                self.scroll = 0;
            }
            KeyCode::Char('G') => {
                self.selection.select(self.visible().len().checked_sub(1));
                self.scroll = 0;
            }
            KeyCode::Char('/') => self.search_prompt(),
            KeyCode::Char('?') => self.help(),
            KeyCode::Char('i') => {
                if let Some(commit) = self.selected() {
                    self.dialog("Post details", output::commit(commit));
                }
            }
            KeyCode::Char('u') => {
                if let Some(user) = self.focus_user() {
                    self.dialog("Profile", output::user(user));
                }
            }
            _ if self.loading => {}
            KeyCode::Char('c') => self.compose(None),
            KeyCode::Char('R') => self.compose(Some(Relation::Reply)),
            KeyCode::Char('Q') => self.compose(Some(Relation::Quote)),
            KeyCode::Char('@') => self.username_prompt(),
            KeyCode::Char('1') => return self.open(Source::Log(Log::Following)),
            KeyCode::Char('2') => return self.open(Source::Log(Log::Latest)),
            KeyCode::Char('3') => return self.open(Source::Log(Log::Trending)),
            KeyCode::Char('4') => return self.open(Source::Saved),
            KeyCode::Char('o') => {
                if !self.signed() {
                    self.status = "Threads need a session; run dz login.".into();
                } else if let Some(id) = self.selected_id() {
                    return self.open(Source::Thread(id));
                }
            }
            KeyCode::Char('a') => {
                if let Some(username) = self
                    .selected()
                    .and_then(|c| c.author.as_ref())
                    .and_then(|user| user.username.parse().ok())
                {
                    return self.open(Source::User {
                        username,
                        feed: Feed::Commits,
                    });
                }
            }
            KeyCode::Char('r') => {
                let source = self.source.clone()?;
                return Some(Action::Work(Work::Fetch(Request::first_page(source))));
            }
            KeyCode::Char('t') => {
                if let Some(Source::User { username, feed }) = &self.source {
                    return Some(Action::Work(Work::Fetch(Request {
                        source: Source::User {
                            username: username.clone(),
                            feed: feed.toggled(),
                        },
                        cursor: None,
                        profile: false,
                    })));
                }
            }
            KeyCode::Char('n') => return self.page_request(|meta| &meta.next_cursor),
            KeyCode::Char('p') => return self.page_request(|meta| &meta.prev_cursor),
            KeyCode::Char('s') => {
                if !self.signed() {
                    self.status = "Starring needs a session; run dz login.".into();
                } else if let Some(commit) = self.selected().filter(|c| !c.is_deleted()) {
                    let on = commit.starred != Some(true);
                    let id = commit.id.parse().ok()?;
                    return Some(Action::Work(Work::Star(id, on)));
                }
            }
            KeyCode::Char('b') => {
                if !self.signed() {
                    self.status = "Saving needs a session; run dz login.".into();
                } else if let Some(commit) = self.selected().filter(|c| !c.is_deleted()) {
                    let on = commit.is_saved != Some(true);
                    let id = commit.id.parse().ok()?;
                    return Some(Action::Work(Work::Save(id, on)));
                }
            }
            KeyCode::Char('f') => {
                if !self.signed() {
                    self.status = "Following needs a session; run dz login.".into();
                } else if let Some(user) = self.focus_user() {
                    if self.me.as_deref() == Some(user.username.as_str()) {
                        self.status = "That is you.".into();
                    } else {
                        let on = user.is_followed != Some(true);
                        let username = user.username.parse().ok()?;
                        return Some(Action::Work(Work::Follow(username, on)));
                    }
                }
            }
            KeyCode::Char('x') => {
                if let Some(commit) = self.selected()
                    && self.signed()
                    && self.me.as_deref() == Some(commit.author_name())
                {
                    self.view = View::ConfirmDelete(commit.id.parse().ok()?);
                } else if self.selected().is_some() {
                    self.status = "You can only delete your own posts.".into();
                }
            }
            KeyCode::Char('N') => {
                if self.signed() {
                    self.loading = true;
                    return Some(Action::Work(Work::Notifications));
                }
                self.status = "Notifications need a session; run dz login.".into();
            }
            KeyCode::Char('m') => {
                if self.signed() {
                    self.loading = true;
                    return Some(Action::Work(Work::Me));
                }
                self.status = "Not signed in; run dz login.".into();
            }
            _ => {}
        }
        None
    }

    fn paste(&mut self, text: &str) {
        match &mut self.view {
            View::Compose(composer) => composer.paste(text),
            View::Username(input) | View::Search(input) => {
                if text.chars().any(char::is_control) || input.lines()[0].len() + text.len() > 129 {
                    self.status = "Paste one line only.".into();
                } else {
                    input.insert_str(text);
                    if matches!(self.view, View::Search(_)) {
                        let filter = match &self.view {
                            View::Search(input) => input.lines()[0].clone(),
                            _ => unreachable!(),
                        };
                        self.set_filter(&filter);
                    }
                }
            }
            _ => {}
        }
    }

    fn help(&mut self) {
        let mut text = String::from(
            "Browse\n  j/k or arrows     Move / scroll\n  Enter / Tab       Open / switch pane\n  Esc / Backspace   Back\n  n / p             Next / previous page\n  /                 Filter this page\n  @                 A user's commits\n  a                 Selected author's commits\n  t                 Toggle a user's commits / replies\n  r                 Refresh\n  i / u             Post details / profile\n",
        );
        if self.signed() {
            text.push_str(
                "\nSigned in\n  1 / 2 / 3         Following / Latest / Trending\n  4                 Saved posts\n  o                 Open the thread\n  s / b             Star / save\n  f                 Follow / unfollow\n  x                 Delete your post\n  N / m             Notifications / your profile\n",
            );
        } else {
            text.push_str("\nRun dz login for logs, threads, stars, follows and notifications.\n");
        }
        text.push_str(
            "\nWrite\n  c                 New post\n  R / Q             Reply to / quote the selected post\n  Ctrl+S            Review draft before sending\n\n  q / Ctrl+C        Quit outside the editor",
        );
        if let Some(left) = self.requests_left {
            text.push_str(&format!(
                "\n\nAPI budget: {left} requests left this minute."
            ));
        }
        self.dialog("Help", text);
    }

    fn page_request(&self, pick: fn(&Meta) -> &Option<String>) -> Option<Action> {
        let cursor = pick(&self.page.as_ref()?.meta).as_deref()?.parse().ok()?;
        Some(Action::Work(Work::Fetch(Request {
            source: self.source.clone()?,
            cursor: Some(cursor),
            profile: false,
        })))
    }

    fn select(&mut self, delta: isize) {
        let count = self.visible().len();
        if count > 0 {
            let index = self
                .selection
                .selected()
                .unwrap_or(0)
                .saturating_add_signed(delta)
                .min(count - 1);
            self.selection.select(Some(index));
            self.scroll = 0;
        }
    }
}

fn fetch(api: &Api, request: &Request) -> Result<Fetched> {
    let cursor = request.cursor.as_ref();
    let mut profile = None;
    let response = match &request.source {
        Source::User { username, feed } => {
            if request.profile {
                profile = Some(api::data(api.user(username)?.body)?);
            }
            api.commits(username, *feed, cursor)?
        }
        Source::Log(log) => api.log(*log, cursor)?,
        Source::Saved => api.saved(cursor)?,
        Source::Thread(id) => {
            let response = api.replies(id, cursor)?;
            if cursor.is_none() {
                let root: Commit = api::data(api.commit(id)?.body)?;
                let mut page = api::page(response.body)?;
                page.data.insert(0, root);
                return Ok(Fetched {
                    profile,
                    page,
                    requests_left: response.requests_left,
                });
            }
            response
        }
    };
    Ok(Fetched {
        profile,
        page: api::page(response.body)?,
        requests_left: response.requests_left,
    })
}

fn work(api: &Api, work: Work) -> Reply {
    match work {
        Work::Fetch(request) => {
            let result = fetch(api, &request).map(Box::new);
            Reply::Fetched(request, result)
        }
        Work::PostWithKey(post) => Reply::Posted(post.send(api)),
        Work::Create {
            body,
            parent_id,
            quoted_commit_id,
        } => Reply::Posted(
            api.create_commit(&body, parent_id.as_deref(), quoted_commit_id.as_deref())
                .and_then(|response| {
                    let commit: Commit = api::data(response.body)?;
                    Ok(format!(
                        "Posted {}{}.\n{}",
                        commit.short_hash,
                        commit
                            .status
                            .as_deref()
                            .filter(|s| *s != "approved")
                            .map(|s| format!(" ({s})"))
                            .unwrap_or_default(),
                        commit.html_url
                    ))
                }),
        ),
        Work::Star(id, on) => {
            Reply::Starred(id.as_str().to_owned(), on, api.set_star(&id, on).map(drop))
        }
        Work::Save(id, on) => {
            Reply::Saved(id.as_str().to_owned(), on, api.set_saved(&id, on).map(drop))
        }
        Work::Follow(username, on) => Reply::Followed(
            username.to_string(),
            on,
            api.set_follow(&username, on).map(drop),
        ),
        Work::Delete(id) => {
            Reply::Deleted(id.as_str().to_owned(), api.delete_commit(&id).map(drop))
        }
        Work::Notifications => Reply::Text(
            "Notifications",
            api.notifications(None)
                .and_then(|r| api::notifications_page(r.body))
                .map(|page| output::notifications(&page)),
        ),
        Work::Me => Reply::Text(
            "You",
            api.me()
                .and_then(|r| api::data::<User>(r.body))
                .map(|user| output::user(&user)),
        ),
    }
}

fn enter_terminal() -> io::Result<DefaultTerminal> {
    let terminal = ratatui::try_init()?;
    execute!(io::stdout(), EnableBracketedPaste)?;
    Ok(terminal)
}

fn resume_terminal(terminal: &mut DefaultTerminal) -> io::Result<()> {
    crossterm::terminal::enable_raw_mode()?;
    execute!(
        io::stdout(),
        crossterm::terminal::EnterAlternateScreen,
        EnableBracketedPaste
    )?;
    terminal.clear()?;
    terminal.hide_cursor()
}

fn restore_terminal() {
    let _ = execute!(io::stdout(), DisableBracketedPaste);
    ratatui::restore();
}

struct RestoreTerminal;
impl Drop for RestoreTerminal {
    fn drop(&mut self) {
        restore_terminal();
    }
}

pub fn run(
    api: Api,
    username: Option<Username>,
    signing_key: Option<PathBuf>,
    me: Option<String>,
) -> Result<()> {
    let (requests, queue) = mpsc::channel::<Work>();
    let (results, replies) = mpsc::channel();
    let signed = api.is_signed();
    // One worker serializes API reads and writes; a signed post is consumed exactly once.
    thread::spawn(move || {
        while let Ok(job) = queue.recv() {
            if results.send(work(&api, job)).is_err() {
                break;
            }
        }
    });
    let poster = if signed {
        Some(Poster::Session)
    } else {
        signing_key.map(Poster::GitHubKey)
    };
    let mut app = App {
        poster,
        me: if signed { me } else { None },
        ..Default::default()
    };
    let start = match username {
        Some(username) => Some(Source::User {
            username,
            feed: Feed::Commits,
        }),
        None if signed => Some(Source::Log(Log::Following)),
        None => None,
    };
    match start {
        Some(source) => {
            requests.send(Work::Fetch(Request::first_page(source)))?;
            app.loading = true;
        }
        None => app.username_prompt(),
    }
    let _restore = RestoreTerminal;
    let mut terminal = enter_terminal()?;
    while !app.quit {
        if let Ok(reply) = replies.try_recv() {
            app.finish(reply);
        }
        terminal.draw(|frame| draw(frame, &mut app))?;
        if !event::poll(Duration::from_millis(100))? {
            continue;
        }
        let action = match event::read()? {
            Event::Key(key) => app.key(key),
            Event::Paste(text) => {
                app.paste(&text);
                None
            }
            _ => None,
        };
        match action {
            Some(Action::Work(job)) => {
                requests
                    .send(job)
                    .map_err(|_| io::Error::other("API worker stopped"))?;
                app.loading = true;
                app.status.clear();
            }
            Some(Action::SignWithKey) => {
                if let View::Compose(composer) = &mut app.view {
                    restore_terminal();
                    eprintln!("Signing with SSH. Complete any passphrase prompt below.");
                    let signed = composer.sign();
                    resume_terminal(&mut terminal)?;
                    match signed {
                        Ok(post) => {
                            if requests.send(Work::PostWithKey(post)).is_err() {
                                composer.failed("API worker stopped; nothing was sent.".into());
                            }
                        }
                        Err(error) => composer.failed(format!("Not sent. {error:#}")),
                    }
                }
            }
            None => {}
        }
    }
    Ok(())
}

// Shared look: one accent for focus and keys, dim text for metadata.
const ACCENT: Color = Color::Cyan;
const DIM: Color = Color::DarkGray;
const SELECTED_BG: Color = Color::Indexed(24);

pub(super) fn panel<'a>(title: impl Into<Line<'a>>, focused: bool) -> Block<'a> {
    Block::default()
        .title_top(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(if focused { ACCENT } else { DIM }))
}

/// A key legend such as `c post  R reply`, with keys highlighted.
pub(super) fn hints(pairs: &[(&str, &str)]) -> Line<'static> {
    let mut spans = Vec::with_capacity(pairs.len() * 3 + 1);
    for (index, (key, label)) in pairs.iter().enumerate() {
        spans.push(Span::raw(if index > 0 { "  " } else { " " }));
        spans.push(Span::styled(
            (*key).to_owned(),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            format!(" {label}"),
            Style::default().fg(Color::Gray),
        ));
    }
    spans.push(Span::raw(" "));
    Line::from(spans)
}

pub(super) fn popup(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    )
}

pub(super) fn render_paragraph(
    frame: &mut Frame,
    area: Rect,
    paragraph: Paragraph,
    scroll: &mut u16,
) {
    *scroll = (*scroll).min(
        paragraph
            .line_count(area.width)
            .saturating_sub(usize::from(area.height))
            .min(usize::from(u16::MAX)) as u16,
    );
    frame.render_widget(paragraph.scroll((*scroll, 0)), area);
}

/// `2026-01-01T00:00:00Z` → `2026-01-01 00:00`, or the raw value when it is not ISO 8601.
fn date_time(created_at: &str) -> String {
    created_at
        .get(..16)
        .filter(|s| s.as_bytes().get(10) == Some(&b'T'))
        .map(|s| s.replace('T', " "))
        .unwrap_or_else(|| created_at.to_owned())
}

fn count(value: u64, singular: &str) -> String {
    if value == 1 {
        format!("{value} {singular}")
    } else {
        format!("{value} {singular}s")
    }
}

fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .split(area);
    draw_header(frame, rows[0], app);
    if area.width >= 80 {
        let list_width = (area.width * 38 / 100).clamp(30, 52);
        let columns =
            Layout::horizontal([Constraint::Length(list_width), Constraint::Min(1)]).split(rows[1]);
        draw_list(frame, columns[0], app);
        draw_post(frame, columns[1], app);
    } else if app.detail_focus {
        draw_post(frame, rows[1], app);
    } else {
        draw_list(frame, rows[1], app);
    }
    draw_footer(frame, rows[2], app);
    match &mut app.view {
        View::Browse | View::Search(_) => {}
        View::Compose(composer) => composer.draw(frame),
        View::Username(input) => {
            let area = popup(area, 50, 3);
            let block = panel(" Username ", true)
                .title_bottom(hints(&[("Enter", "open"), ("Esc", "cancel")]));
            let inner = block.inner(area);
            frame.render_widget(Clear, area);
            frame.render_widget(block, area);
            frame.render_widget(&**input, inner);
        }
        View::ConfirmDelete(_) => {
            let area = popup(area, 50, 3);
            let block = panel(" Delete this post? ", true)
                .title_bottom(hints(&[("y", "delete"), ("Esc", "keep")]));
            let inner = block.inner(area);
            frame.render_widget(Clear, area);
            frame.render_widget(block, area);
            frame.render_widget(
                Paragraph::new(" This cannot be undone.").style(Style::default().fg(Color::Yellow)),
                inner,
            );
        }
        View::Dialog(dialog) => {
            let height = (area.height * 4 / 5).max(8);
            let area = popup(area, 80, height);
            let block = panel(format!(" {} ", dialog.title), true)
                .title_bottom(hints(&[("↑/↓", "scroll"), ("Esc", "close")]))
                .padding(Padding::horizontal(1));
            let inner = block.inner(area);
            frame.render_widget(Clear, area);
            frame.render_widget(block, area);
            render_paragraph(
                frame,
                inner,
                Paragraph::new(output::safe(&dialog.text)).wrap(Wrap { trim: false }),
                &mut dialog.scroll,
            );
        }
    }
}

fn draw_header(frame: &mut Frame, area: Rect, app: &App) {
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(DIM);
    let mut spans = vec![
        Span::raw(" "),
        Span::styled("dz", bold.fg(ACCENT)),
        Span::raw("  "),
    ];
    match &app.source {
        Some(Source::User { .. }) | None if app.profile.is_some() => {
            let user = app.profile.as_ref().expect("checked");
            spans.push(Span::styled(output::safe(&user.name), bold));
            spans.push(Span::styled(
                format!(" @{}", output::safe(&user.username)),
                Style::default().fg(Color::Gray),
            ));
            let mut stats = [
                (user.commits_count, "commit"),
                (user.followers_count, "follower"),
            ]
            .into_iter()
            .filter_map(|(value, noun)| value.map(|v| count(v, noun)))
            .collect::<Vec<_>>();
            if user.is_followed == Some(true) {
                stats.push("following".into());
            }
            if user.follows_you == Some(true) {
                stats.push("follows you".into());
            }
            if !stats.is_empty() {
                spans.push(Span::styled(format!("  ·  {}", stats.join(" · ")), dim));
            }
        }
        Some(source) => spans.push(Span::styled(source.label(), bold)),
        None if app.signed() => spans.push(Span::styled("Following", bold)),
        None => spans.push(Span::styled("Press @ to choose a user", dim)),
    }
    let right = if app.loading {
        Span::styled("Loading…", Style::default().fg(Color::Yellow))
    } else if let Some(me) = &app.me {
        Span::styled(format!("@{me}"), dim)
    } else {
        Span::styled("guest", dim)
    };
    let right_width = right.width() as u16 + 1;
    let columns =
        Layout::horizontal([Constraint::Min(1), Constraint::Length(right_width)]).split(area);
    frame.render_widget(Paragraph::new(Line::from(spans)), columns[0]);
    frame.render_widget(
        Paragraph::new(Line::from(right)).alignment(Alignment::Right),
        columns[1],
    );
}

fn draw_footer(frame: &mut Frame, area: Rect, app: &App) {
    if let View::Search(input) = &app.view {
        let columns = Layout::horizontal([Constraint::Length(3), Constraint::Min(1)]).split(area);
        frame.render_widget(
            Paragraph::new(" /").style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
            columns[0],
        );
        frame.render_widget(&**input, columns[1]);
        return;
    }
    if !app.status.is_empty() {
        frame.render_widget(
            Paragraph::new(format!(" {}", output::safe(&app.status)))
                .style(Style::default().fg(Color::Yellow)),
            area,
        );
        return;
    }
    let line = if !app.filter.is_empty() && !app.detail_focus {
        hints(&[
            ("↑/↓", "select"),
            ("Enter", "read"),
            ("/", "edit filter"),
            ("Esc", "clear filter"),
            ("?", "help"),
        ])
    } else if app.detail_focus {
        hints(&[
            ("↑/↓", "scroll"),
            ("R", "reply"),
            ("i", "details"),
            ("Esc", "back"),
            ("?", "help"),
        ])
    } else if area.width < 80 {
        hints(&[
            ("Enter", "read"),
            ("n/p", "page"),
            ("c", "post"),
            ("@", "user"),
            ("?", "help"),
            ("q", "quit"),
        ])
    } else if app.signed() {
        hints(&[
            ("1-4", "logs"),
            ("o", "thread"),
            ("s", "star"),
            ("f", "follow"),
            ("c", "post"),
            ("R", "reply"),
            ("@", "user"),
            ("N", "notifs"),
            ("?", "help"),
            ("q", "quit"),
        ])
    } else {
        hints(&[
            ("↑/↓", "select"),
            ("Enter", "read"),
            ("n/p", "page"),
            ("t", "replies"),
            ("c", "post"),
            ("R", "reply"),
            ("@", "user"),
            ("/", "filter"),
            ("?", "help"),
            ("q", "quit"),
        ])
    };
    frame.render_widget(Paragraph::new(line), area);
}

fn draw_list(frame: &mut Frame, area: Rect, app: &mut App) {
    let focused = !app.detail_focus;
    let active = Style::default().fg(ACCENT).add_modifier(Modifier::BOLD);
    let inactive = Style::default().fg(DIM);
    let title = match &app.source {
        Some(Source::User { feed, .. }) => Line::from(
            [Feed::Commits, Feed::Replies]
                .into_iter()
                .map(|tab| {
                    Span::styled(
                        format!(" {} ", tab.label()),
                        if tab == *feed { active } else { inactive },
                    )
                })
                .collect::<Vec<_>>(),
        ),
        Some(Source::Thread(_)) => Line::from(Span::styled(" Thread ", active)),
        Some(source @ (Source::Log(_) | Source::Saved)) => {
            let mut spans = Vec::new();
            for (tab, label) in [
                (Source::Log(Log::Following), "Following"),
                (Source::Log(Log::Latest), "Latest"),
                (Source::Log(Log::Trending), "Trending"),
                (Source::Saved, "Saved"),
            ] {
                spans.push(Span::styled(
                    format!(" {label} "),
                    if tab == *source { active } else { inactive },
                ));
            }
            Line::from(spans)
        }
        None => Line::from(Span::styled(" Posts ", inactive)),
    };
    let meta = app.page.as_ref().map(|page| &page.meta);
    let mut paging = Vec::new();
    if meta.is_some_and(|m| m.prev_cursor.is_some()) {
        paging.push(("p", "‹ prev"));
    }
    if meta.is_some_and(|m| m.next_cursor.is_some()) {
        paging.push(("n", "next ›"));
    }
    let mut block = panel(title, focused);
    if !paging.is_empty() {
        block = block.title_bottom(hints(&paging).alignment(Alignment::Right));
    }
    let visible = app.visible();
    if !app.filter.is_empty() {
        block = block.title_bottom(Line::from(vec![
            Span::styled(" / ", active),
            Span::raw(output::safe(&app.filter)),
            Span::styled(
                format!(
                    "  {} {} ",
                    visible.len(),
                    if visible.len() == 1 {
                        "match"
                    } else {
                        "matches"
                    }
                ),
                inactive,
            ),
        ]));
    }
    let root_id = match &app.source {
        Some(Source::Thread(id)) => Some(id.as_str()),
        _ => None,
    };
    let items: Vec<ListItem> = visible
        .iter()
        .map(|commit| {
            if commit.is_deleted() {
                return ListItem::new(vec![
                    Line::styled("(deleted)", inactive),
                    Line::styled(format!("  {}", commit.id), inactive),
                ]);
            }
            let first_line = commit
                .body
                .lines()
                .find(|line| !line.trim().is_empty())
                .unwrap_or("(empty)");
            let date = date_time(&commit.created_at);
            let mut meta = format!(
                "  @{} · {} · {}{} · ↩ {}",
                commit.author_name(),
                date.get(..10).unwrap_or(&date),
                if commit.starred == Some(true) {
                    "★"
                } else {
                    "☆"
                },
                commit.stars_count,
                commit.replies_count
            );
            if commit.is_saved == Some(true) {
                meta.push_str(" · saved");
            }
            let marker = if root_id == Some(commit.id.as_str()) {
                "▲ "
            } else if root_id.is_some() {
                "↳ "
            } else {
                ""
            };
            ListItem::new(vec![
                Line::from(format!("{marker}{}", output::safe(first_line))),
                Line::styled(output::safe(&meta), inactive),
            ])
        })
        .collect();
    if items.is_empty() {
        let message = if app.loading {
            "Loading…"
        } else if !app.filter.is_empty() {
            "No posts match."
        } else if matches!(app.source, Some(Source::Log(Log::Following))) {
            "Nobody you follow has posted yet. Try 2 Latest."
        } else if app.source.is_some() {
            "Nothing here yet."
        } else {
            "Press @ to choose a user."
        };
        frame.render_widget(
            Paragraph::new(message)
                .style(inactive)
                .alignment(Alignment::Center)
                .wrap(Wrap { trim: true })
                .block(block.padding(Padding::top(area.height.saturating_sub(3) / 2))),
            area,
        );
    } else {
        frame.render_stateful_widget(
            List::new(items)
                .block(block)
                .highlight_symbol("▌ ")
                .highlight_style(
                    Style::default()
                        .fg(Color::White)
                        .bg(SELECTED_BG)
                        .add_modifier(Modifier::BOLD),
                ),
            area,
            &mut app.selection,
        );
    }
}

fn draw_post(frame: &mut Frame, area: Rect, app: &mut App) {
    let Some(commit) = app.selected() else {
        frame.render_widget(
            Paragraph::new("Select a post to read.")
                .style(Style::default().fg(DIM))
                .alignment(Alignment::Center)
                .block(
                    panel(" Post ", app.detail_focus)
                        .padding(Padding::top(area.height.saturating_sub(3) / 2)),
                ),
            area,
        );
        return;
    };
    if commit.is_deleted() {
        frame.render_widget(
            Paragraph::new("This post was deleted.")
                .style(Style::default().fg(DIM))
                .alignment(Alignment::Center)
                .block(
                    panel(" Post ", app.detail_focus)
                        .padding(Padding::top(area.height.saturating_sub(3) / 2)),
                ),
            area,
        );
        return;
    }
    let title = Line::from(vec![
        Span::raw(" "),
        Span::styled(
            format!("@{}", output::safe(commit.author_name())),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  {} ", date_time(&commit.created_at)),
            Style::default().fg(Color::Gray),
        ),
    ]);
    let mut footer = vec![
        format!(
            "{} {}",
            if commit.starred == Some(true) {
                "★"
            } else {
                "☆"
            },
            commit.stars_count
        ),
        format!("↩ {}", commit.replies_count),
    ];
    if !commit.images.is_empty() {
        footer.push(count(commit.images.len() as u64, "image"));
    }
    if commit.is_saved == Some(true) {
        footer.push("saved".into());
    }
    if commit.parent_id.is_some() {
        footer.push("reply".into());
    }
    if let Some(status) = commit.status.as_deref().filter(|s| *s != "approved") {
        footer.push(status.to_owned());
    }
    let block = panel(title, app.detail_focus)
        .title_bottom(
            Line::styled(
                format!(" {} ", footer.join(" · ")),
                Style::default().fg(Color::Gray),
            )
            .alignment(Alignment::Right),
        )
        .padding(Padding::horizontal(1));
    let inner = block.inner(area);
    let mut text = Text::from(output::safe(&commit.body));
    if let Some(quoted) = &commit.quoted_commit {
        text.push_line(Line::raw(""));
        let quote_style = Style::default().fg(Color::Gray);
        if quoted.is_deleted() {
            text.push_line(Line::styled("┃ (quoted post was deleted)", quote_style));
        } else {
            text.push_line(Line::styled(
                format!(
                    "┃ @{}  {}",
                    output::safe(quoted.author_name()),
                    date_time(&quoted.created_at)
                ),
                quote_style.add_modifier(Modifier::BOLD),
            ));
            for line in output::safe(&quoted.body).lines().take(6) {
                text.push_line(Line::styled(format!("┃ {line}"), quote_style));
            }
        }
    }
    if let Some(preview_title) = commit
        .link_preview
        .as_ref()
        .and_then(|preview| preview.title.as_deref())
        .filter(|title| !title.trim().is_empty())
    {
        text.push_line(Line::raw(""));
        text.push_line(Line::styled(
            format!("Link: {}", output::safe(preview_title)),
            Style::default().fg(DIM),
        ));
    }
    frame.render_widget(block, area);
    render_paragraph(
        frame,
        inner,
        Paragraph::new(text).wrap(Wrap { trim: false }),
        &mut app.scroll,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }
    fn alice() -> Source {
        Source::User {
            username: "alice".parse().unwrap(),
            feed: Feed::Commits,
        }
    }
    fn fixture_page() -> Page {
        serde_json::from_str(include_str!("../tests/fixtures/commits.json")).unwrap()
    }
    pub(super) fn populated() -> App {
        let mut app = App::default();
        app.finish(Reply::Fetched(
            Request::first_page(alice()),
            Ok(Box::new(Fetched {
                profile: Some(
                    api::data(
                        serde_json::from_str(include_str!("../tests/fixtures/user.json")).unwrap(),
                    )
                    .unwrap(),
                ),
                page: fixture_page(),
                requests_left: Some(57),
            })),
        ));
        app
    }
    fn signed_in() -> App {
        let mut app = App {
            me: Some("alice".into()),
            poster: Some(Poster::Session),
            ..Default::default()
        };
        app.finish(Reply::Fetched(
            Request::first_page(Source::Log(Log::Following)),
            Ok(Box::new(Fetched {
                profile: None,
                page: fixture_page(),
                requests_left: Some(100),
            })),
        ));
        app
    }
    fn screen(terminal: &Terminal<TestBackend>) -> String {
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    #[test]
    fn screens_are_safe_at_small_sizes() {
        for mut app in [populated(), signed_in()] {
            app.poster = Some(Poster::GitHubKey("test-key".into()));
            for (w, h) in [(100, 26), (40, 15), (10, 4), (1, 1)] {
                let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
                for view in [' ', '@', '/', '?', 'c', 'R', 'i', 'u', 'x'] {
                    app.view = View::Browse;
                    app.key(key(view));
                    terminal.draw(|f| draw(f, &mut app)).unwrap();
                }
            }
        }
    }

    #[test]
    fn browse_hides_metadata_and_details_are_on_demand() {
        let mut app = populated();
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let screen = screen(&terminal);
        assert!(screen.contains("Hello, world!"));
        // The header carries the profile summary; everything noisier stays behind `i`/`u`.
        assert!(screen.contains("Alice @alice"));
        assert!(screen.contains("3 followers"));
        for hidden in ["ID:", "https://", "57", "Embed:"] {
            assert!(!screen.contains(hidden), "{hidden}");
        }
        app.key(key('i'));
        assert!(matches!(&app.view, View::Dialog(dialog) if dialog.text.contains("https://")));
        app.view = View::Browse;
        app.key(key('?'));
        assert!(matches!(&app.view, View::Dialog(dialog) if dialog.text.contains("57 requests")));
    }

    #[test]
    fn narrow_terminals_switch_between_list_and_body() {
        let mut app = populated();
        let mut terminal = Terminal::new(TestBackend::new(40, 15)).unwrap();
        for (code, shows_body) in [
            (KeyCode::Null, false),
            (KeyCode::Enter, true),
            (KeyCode::Esc, false),
        ] {
            app.key(KeyEvent::new(code, KeyModifiers::NONE));
            terminal.draw(|f| draw(f, &mut app)).unwrap();
            assert_eq!(screen(&terminal).contains("A second line."), shows_body);
            assert!(!app.quit);
        }
    }

    #[test]
    fn filter_narrows_selection_and_esc_clears_it() {
        let mut app = populated();
        let total = app.visible().len();
        app.key(key('/'));
        for c in "second".chars() {
            app.key(key(c));
        }
        assert!(matches!(app.view, View::Search(_)));
        assert_eq!(app.visible().len(), 1);
        assert!(app.selected().unwrap().body.contains("A second line."));
        app.key(key('x'));
        assert!(app.visible().is_empty());
        assert!(app.selected().is_none());
        app.key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(app.view, View::Browse));
        assert_eq!(app.filter, "second");
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.filter.is_empty());
        assert_eq!(app.visible().len(), total);
        assert!(!app.quit);
    }

    #[test]
    fn failed_navigation_preserves_current_source() {
        let mut app = populated();
        app.loading = true;
        app.finish(Reply::Fetched(
            Request::first_page(Source::User {
                username: "bob".parse().unwrap(),
                feed: Feed::Replies,
            }),
            Err(anyhow::anyhow!("offline")),
        ));
        assert_eq!(app.source, Some(alice()));
        assert!(!app.loading);
        assert!(app.status.contains("offline"));
    }

    #[test]
    fn loading_and_missing_keys_do_not_start_writes() {
        let mut app = populated();
        app.loading = true;
        for c in ['c', 'R', '@', 'r', 'n', 's', 'f', 'o'] {
            assert!(app.key(key(c)).is_none());
        }
        assert!(matches!(app.view, View::Browse));
        app.loading = false;
        app.key(key('c'));
        assert!(app.status.contains("DZ_SSH_KEY"));
        assert!(matches!(app.view, View::Browse));
        app.key(key('q'));
        assert!(app.quit);
    }

    #[test]
    fn guests_cannot_reach_signed_sources_or_actions() {
        let mut app = populated();
        for c in ['1', '2', '3', '4'] {
            assert!(app.key(key(c)).is_none());
        }
        for (c, hint) in [
            ('o', "session"),
            ('s', "session"),
            ('f', "session"),
            ('N', "session"),
        ] {
            app.key(key(c));
            assert!(app.status.contains(hint), "{c}: {}", app.status);
        }
        assert!(matches!(app.view, View::Browse));
    }

    #[test]
    fn signed_in_navigation_and_actions() {
        let mut app = signed_in();
        assert_eq!(app.source, Some(Source::Log(Log::Following)));
        // Logs, threads and the author's page are one key away.
        assert!(matches!(
            app.key(key('2')),
            Some(Action::Work(Work::Fetch(Request {
                source: Source::Log(Log::Latest),
                ..
            })))
        ));
        assert!(matches!(
            app.key(key('o')),
            Some(Action::Work(Work::Fetch(Request {
                source: Source::Thread(_),
                ..
            })))
        ));
        assert!(matches!(
            app.key(key('a')),
            Some(Action::Work(Work::Fetch(Request {
                source: Source::User { .. },
                ..
            })))
        ));
        // Star toggles from the current state and the reply updates the row.
        let id = app.selected().unwrap().id.clone();
        assert!(matches!(
            app.key(key('s')),
            Some(Action::Work(Work::Star(_, true)))
        ));
        app.finish(Reply::Starred(id.clone(), true, Ok(())));
        assert_eq!(app.selected().unwrap().starred, Some(true));
        assert_eq!(app.selected().unwrap().stars_count, 6);
        assert!(matches!(
            app.key(key('s')),
            Some(Action::Work(Work::Star(_, false)))
        ));
        // Only your own posts can be deleted, after confirmation.
        app.key(key('x'));
        assert!(matches!(app.view, View::ConfirmDelete(_)));
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        app.key(key('x'));
        assert!(matches!(
            app.key(key('y')),
            Some(Action::Work(Work::Delete(_)))
        ));
        app.finish(Reply::Deleted(id, Ok(())));
        assert!(app.visible().is_empty());
        // Following yourself is refused; quoting opens the composer.
        app.me = Some("bob".into());
        app.finish(Reply::Fetched(
            Request::first_page(Source::Log(Log::Latest)),
            Ok(Box::new(Fetched {
                profile: None,
                page: fixture_page(),
                requests_left: None,
            })),
        ));
        assert!(matches!(
            app.key(key('f')),
            Some(Action::Work(Work::Follow(_, true)))
        ));
        app.key(key('Q'));
        assert!(matches!(app.view, View::Compose(_)));
    }

    #[test]
    fn back_returns_to_the_previous_source_without_stacking_it() {
        let mut app = signed_in();
        let page = || fixture_page();
        app.finish(Reply::Fetched(
            Request::first_page(alice()),
            Ok(Box::new(Fetched {
                profile: None,
                page: page(),
                requests_left: None,
            })),
        ));
        assert_eq!(app.history, vec![Source::Log(Log::Following)]);
        let back = app.key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert!(matches!(
            back,
            Some(Action::Work(Work::Fetch(Request {
                source: Source::Log(Log::Following),
                ..
            })))
        ));
        app.finish(Reply::Fetched(
            Request::first_page(Source::Log(Log::Following)),
            Ok(Box::new(Fetched {
                profile: None,
                page: page(),
                requests_left: None,
            })),
        ));
        assert!(app.history.is_empty());
        assert_eq!(app.source, Some(Source::Log(Log::Following)));
    }
}

#[cfg(test)]
mod preview {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    fn dump(terminal: &Terminal<TestBackend>) -> String {
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// `cargo test preview -- --ignored --nocapture` prints the main screens for eyeballing.
    #[test]
    #[ignore]
    fn print_screens() {
        let mut app = super::tests::populated();
        app.me = Some("sora".into());
        app.poster = Some(Poster::Session);
        let mut terminal = Terminal::new(TestBackend::new(110, 28)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        println!("{}\n", dump(&terminal));
        app.key(KeyEvent::new(KeyCode::Char('R'), KeyModifiers::NONE));
        if let View::Compose(c) = &mut app.view {
            c.paste("Thanks, trying this out from dev-zero.");
        }
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        println!("{}\n", dump(&terminal));
        app.view = View::Browse;
        app.key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE));
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        println!("{}\n", dump(&terminal));
        app.view = View::Browse;
        app.finish(Reply::Fetched(
            Request::first_page(Source::Log(Log::Latest)),
            Ok(Box::new(Fetched {
                profile: None,
                page: serde_json::from_str(include_str!("../tests/fixtures/commits.json")).unwrap(),
                requests_left: Some(99),
            })),
        ));
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        println!("{}\n", dump(&terminal));
        let mut narrow = Terminal::new(TestBackend::new(60, 14)).unwrap();
        narrow.draw(|f| draw(f, &mut app)).unwrap();
        println!("{}\n", dump(&narrow));
    }
}
