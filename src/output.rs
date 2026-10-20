use crate::model::{Commit, Notification, NotificationsPage, Page, User, UsersPage};

// API text is untrusted: never pass terminal control sequences through to the screen.
pub fn safe(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .collect()
}

pub fn user(user: &User) -> String {
    let mut text = format!("{} (@{})\n", user.name, user.username);
    if let Some(bio) = &user.bio {
        text.push_str(&format!("{bio}\n"));
    }
    text.push_str(&format!(
        "\n{} commits · {} followers · {} following\n",
        user.commits_count.unwrap_or(0),
        user.followers_count.unwrap_or(0),
        user.following_count.unwrap_or(0)
    ));
    let relation = [
        (user.is_followed == Some(true), "you follow them"),
        (user.follows_you == Some(true), "follows you"),
    ]
    .into_iter()
    .filter_map(|(on, label)| on.then_some(label))
    .collect::<Vec<_>>();
    if !relation.is_empty() {
        text.push_str(&format!("{}\n", relation.join(" · ")));
    }
    if let Some(unread) = user.unread_notifications_count {
        text.push_str(&format!("{unread} unread notifications\n"));
    }
    for (label, value) in [
        ("Company", &user.company),
        ("Location", &user.location),
        ("Website", &user.website),
        ("Feed", &user.feed_url),
    ] {
        if let Some(value) = value {
            text.push_str(&format!("{label}: {value}\n"));
        }
    }
    text.push_str(&user.html_url);
    safe(&text)
}

/// One line per user, for follower and following lists.
pub fn users(page: &UsersPage, what: &str) -> String {
    let mut text = page
        .data
        .iter()
        .map(|user| {
            let mut line = format!("@{}", user.username);
            if !user.name.is_empty() {
                line.push_str(&format!("  {}", user.name));
            }
            if let Some(bio) = user.bio.as_deref().map(str::trim).filter(|b| !b.is_empty()) {
                line.push_str(&format!("  — {}", bio.lines().next().unwrap_or("")));
            }
            line
        })
        .collect::<Vec<_>>()
        .join("\n");
    if text.is_empty() {
        text = format!("No {what}.");
    }
    push_cursors(&mut text, &page.meta);
    safe(&text)
}

pub fn commit(commit: &Commit) -> String {
    if commit.is_deleted() {
        return safe(&format!("(deleted commit)\nID: {}", commit.id));
    }
    let mut text = format!(
        "{} · @{} · {}\n\n{}\n\n{} stars · {} replies",
        commit.short_hash,
        commit.author_name(),
        commit.created_at,
        commit.body,
        commit.stars_count,
        commit.replies_count,
    );
    let mine = [
        (commit.starred == Some(true), "starred"),
        (commit.is_saved == Some(true), "saved"),
    ]
    .into_iter()
    .filter_map(|(on, label)| on.then_some(label))
    .collect::<Vec<_>>();
    if !mine.is_empty() {
        text.push_str(&format!(" · {}", mine.join(" · ")));
    }
    text.push_str(&format!("\nID: {}\n", commit.id));
    if let Some(status) = commit.status.as_deref().filter(|s| *s != "approved") {
        text.push_str(&format!("Status: {status}\n"));
    }
    if let Some(parent) = &commit.parent_id {
        text.push_str(&format!("Reply to: {parent}\n"));
    }
    if let Some(quoted) = &commit.quoted_commit {
        let first_line = quoted.body.lines().next().unwrap_or("");
        text.push_str(&format!("Quotes @{}: {first_line}\n", quoted.author_name()));
    } else if let Some(quoted) = &commit.quoted_commit_id {
        text.push_str(&format!("Quotes: {quoted}\n"));
    }
    if let Some(preview) = &commit.link_preview {
        for value in [&preview.title, &preview.description, &preview.url]
            .into_iter()
            .flatten()
        {
            text.push_str(&format!("{value}\n"));
        }
        if let Some(embed) = &preview.embed {
            let parts: Vec<&str> = [&embed.provider, &embed.kind, &embed.src]
                .into_iter()
                .flatten()
                .map(String::as_str)
                .collect();
            text.push_str(&format!("Embed: {}\n", parts.join(" ")));
        }
    }
    for image in &commit.images {
        text.push_str(&format!("Image: {}", image.url));
        if let (Some(width), Some(height)) = (image.width, image.height) {
            text.push_str(&format!(" ({width}\u{d7}{height})"));
        }
        text.push('\n');
    }
    text.push_str(&commit.html_url);
    safe(&text)
}

pub fn page(page: &Page) -> String {
    let mut text = page
        .data
        .iter()
        .map(commit)
        .collect::<Vec<_>>()
        .join("\n\n---\n\n");
    if page.is_ranked == Some(true) && !text.is_empty() {
        text.insert_str(0, "Ranked by trend, not by date.\n\n");
    }
    if text.is_empty() {
        text.push_str("No commits.");
        if !page.suggestions.is_empty() {
            text.push_str("\n\nSuggested:\n\n");
            text.push_str(
                &page
                    .suggestions
                    .iter()
                    .map(commit)
                    .collect::<Vec<_>>()
                    .join("\n\n---\n\n"),
            );
        }
    }
    push_cursors(&mut text, &page.meta);
    text
}

pub fn notification(notification: &Notification) -> String {
    let actor = notification
        .actor
        .as_ref()
        .map(|user| format!("@{}", user.username))
        .unwrap_or_else(|| "Someone".into());
    let others = match notification.actors_count {
        Some(count) if count > 1 => format!(" and {} others", count - 1),
        _ => String::new(),
    };
    let what = match notification.kind.as_str() {
        "star" | "stars" => "starred your commit",
        "reply" | "replies" => "replied to your commit",
        "quote" | "quotes" => "quoted your commit",
        "mention" | "mentions" => "mentioned you",
        "follow" | "follows" => "followed you",
        "moderation" => "moderation update",
        other => other,
    };
    let mut line = format!(
        "{} {actor}{others} {what}",
        notification.created_at.get(..10).unwrap_or("")
    );
    if notification.read_at.is_none() {
        line.push_str("  (new)");
    }
    if let Some(commit) = &notification.commit {
        let first_line = commit.body.lines().next().unwrap_or("");
        if !first_line.is_empty() {
            line.push_str(&format!("\n    {first_line}"));
        }
    }
    line
}

pub fn notifications(page: &NotificationsPage) -> String {
    let mut text = page
        .data
        .iter()
        .map(notification)
        .collect::<Vec<_>>()
        .join("\n");
    if text.is_empty() {
        text.push_str("No notifications.");
    }
    push_cursors(&mut text, &page.meta);
    safe(&text)
}

fn push_cursors(text: &mut String, meta: &crate::model::Meta) {
    for (label, cursor) in [("Next", &meta.next_cursor), ("Previous", &meta.prev_cursor)] {
        if let Some(cursor) = cursor {
            text.push_str(&format!("\n\n{label} page: --cursor {}", safe(cursor)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_terminal_controls_but_keeps_multiline_text() {
        assert_eq!(
            safe("hi\x1b]52;c;secret\x07\n日本語\t\u{009b}"),
            "hi]52;c;secret\n日本語\t"
        );
    }

    #[test]
    fn tombstones_render_without_an_author() {
        let tombstone: Commit = serde_json::from_str(
            r#"{"id":"x","parent_id":null,"deleted_at":"2026-01-01T00:00:00Z","replies_count":0}"#,
        )
        .unwrap();
        assert!(commit(&tombstone).starts_with("(deleted commit)"));
    }
}
