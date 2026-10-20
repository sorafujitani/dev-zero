use std::{fmt, str::FromStr};

use serde::Deserialize;

/// A validated dev-null username without the leading `@`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Username(String);

impl Username {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Username {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for Username {
    type Err = InvalidInput;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let value = value.strip_prefix('@').unwrap_or(value);
        if value.is_empty()
            || value.len() > 128
            || !value
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'))
        {
            return Err(InvalidInput::Username);
        }
        Ok(Self(value.to_owned()))
    }
}

/// A validated public commit ID, safe to place in a URL path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitId(String);

impl CommitId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for CommitId {
    type Err = InvalidInput;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.is_empty()
            || value.len() > 128
            || !value
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-')
        {
            return Err(InvalidInput::CommitId);
        }
        Ok(Self(value.to_owned()))
    }
}

/// A pagination cursor. Accepts `meta.next_cursor` as is, or a `links.next` URL
/// from which the `cursor` query parameter is taken.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cursor(String);

impl Cursor {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for Cursor {
    type Err = InvalidInput;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let cursor = if value.starts_with("http://") || value.starts_with("https://") {
            reqwest::Url::parse(value)
                .ok()
                .and_then(|url| {
                    url.query_pairs()
                        .find(|(key, _)| key == "cursor")
                        .map(|(_, cursor)| cursor.into_owned())
                })
                .ok_or(InvalidInput::Cursor)?
        } else {
            value.to_owned()
        };
        if cursor.is_empty() || cursor.len() > 1024 {
            return Err(InvalidInput::Cursor);
        }
        Ok(Self(cursor))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InvalidInput {
    Username,
    CommitId,
    Cursor,
}

impl fmt::Display for InvalidInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Username => "username must contain only letters, digits, hyphens and underscores",
            Self::CommitId => "commit ID must contain only letters, digits and hyphens",
            Self::Cursor => "cursor must be meta.next_cursor or a links.next URL",
        })
    }
}

impl std::error::Error for InvalidInput {}

/// Which of a user's timelines to list.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Feed {
    #[default]
    Commits,
    Replies,
}

impl Feed {
    pub fn toggled(self) -> Self {
        match self {
            Self::Commits => Self::Replies,
            Self::Replies => Self::Commits,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Commits => "Commits",
            Self::Replies => "Replies",
        }
    }
}

/// One of the three home timelines; signed requests only.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum Log {
    #[default]
    Following,
    Latest,
    Trending,
}

impl Log {
    pub fn path(self) -> &'static str {
        match self {
            Self::Following => "following",
            Self::Latest => "latest",
            Self::Trending => "trending",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Following => "Following",
            Self::Latest => "Latest",
            Self::Trending => "Trending",
        }
    }
}

#[derive(Debug, Default, Deserialize, Clone)]
pub struct User {
    pub username: String,
    #[serde(default)]
    pub name: String,
    pub bio: Option<String>,
    pub company: Option<String>,
    pub location: Option<String>,
    pub website: Option<String>,
    #[serde(default)]
    pub html_url: String,
    pub feed_url: Option<String>,
    pub commits_count: Option<u64>,
    pub followers_count: Option<u64>,
    pub following_count: Option<u64>,
    /// Only in signed responses.
    pub is_followed: Option<bool>,
    pub follows_you: Option<bool>,
    pub unread_notifications_count: Option<u64>,
}

/// A commit, or the tombstone a signed thread shows for a deleted one (`deleted_at` set, no author or body).
#[derive(Debug, Default, Deserialize, Clone)]
pub struct Commit {
    pub id: String,
    #[serde(default)]
    pub short_hash: String,
    #[serde(default)]
    pub body: String,
    pub parent_id: Option<String>,
    #[serde(default)]
    pub html_url: String,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub stars_count: u64,
    #[serde(default)]
    pub replies_count: u64,
    pub author: Option<User>,
    #[serde(default)]
    pub images: Vec<Image>,
    pub link_preview: Option<LinkPreview>,
    pub deleted_at: Option<String>,
    /// Only in signed responses.
    pub starred: Option<bool>,
    pub is_saved: Option<bool>,
    pub quoted_commit_id: Option<String>,
    pub quoted_commit: Option<Box<Commit>>,
    pub status: Option<String>,
}

impl Commit {
    pub fn is_deleted(&self) -> bool {
        self.deleted_at.is_some()
    }

    /// The author's username, or a placeholder for a tombstone.
    pub fn author_name(&self) -> &str {
        self.author
            .as_ref()
            .map(|user| user.username.as_str())
            .unwrap_or("deleted")
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct Image {
    pub url: String,
    pub width: Option<u64>,
    pub height: Option<u64>,
}

#[derive(Debug, Default, Deserialize, Clone)]
pub struct LinkPreview {
    pub url: Option<String>,
    pub title: Option<String>,
    pub description: Option<String>,
    pub embed: Option<Embed>,
}

/// The player the web embeds for a link, e.g. a YouTube video.
#[derive(Debug, Default, Deserialize, Clone)]
pub struct Embed {
    pub provider: Option<String>,
    pub kind: Option<String>,
    pub src: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct Page {
    pub data: Vec<Commit>,
    #[serde(default)]
    pub meta: Meta,
    /// Logs only: Trending is ranked rather than newest first.
    pub is_ranked: Option<bool>,
    /// An empty Following log suggests a few commits.
    #[serde(default)]
    pub suggestions: Vec<Commit>,
}

/// Server-side pagination state; both cursors are `None` on a single page.
#[derive(Debug, Default, Deserialize)]
pub struct Meta {
    pub next_cursor: Option<String>,
    #[serde(default)]
    pub prev_cursor: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct UsersPage {
    pub data: Vec<User>,
    #[serde(default)]
    pub meta: Meta,
}

#[derive(Debug, Deserialize)]
pub struct Notification {
    #[serde(rename = "type")]
    pub kind: String,
    pub created_at: String,
    pub read_at: Option<String>,
    pub actor: Option<User>,
    #[serde(default)]
    pub actors_count: Option<u64>,
    pub commit: Option<Commit>,
}

#[derive(Debug, Default, Deserialize)]
pub struct NotificationsPage {
    pub data: Vec<Notification>,
    #[serde(default)]
    pub meta: Meta,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_username_and_commit_id() {
        assert_eq!("@someone".parse::<Username>().unwrap().as_str(), "someone");
        for username in ["", "@", "../admin", "a/b", "a?b"] {
            assert!(username.parse::<Username>().is_err());
        }
        assert!("abc-123".parse::<CommitId>().is_ok());
        for id in ["", "../id", "a b"] {
            assert!(id.parse::<CommitId>().is_err());
        }
    }

    #[test]
    fn cursor_accepts_raw_value_or_links_next_url() {
        assert_eq!("abc".parse::<Cursor>().unwrap().as_str(), "abc");
        assert_eq!(
            "https://dev-null.sh/api/v1/users/a/commits?replies=1&cursor=a%2Bb"
                .parse::<Cursor>()
                .unwrap()
                .as_str(),
            "a+b"
        );
        for value in ["", "https://dev-null.sh/api/v1/users/a/commits"] {
            assert!(value.parse::<Cursor>().is_err());
        }
    }
}
