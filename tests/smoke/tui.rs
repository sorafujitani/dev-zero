use std::{
    fs,
    time::{Duration, Instant},
};

use serde_json::Value;

use super::{
    http::{self, Server},
    pty::Terminal,
};

#[test]
fn browse_pagination_replies_and_missing_user() {
    let server = Server::new();
    let mut terminal = Terminal::start(&server, &[], None, None);
    terminal.wait_for("Username");
    terminal.send(b"alice\r");
    terminal.wait_for("Hello, world!");
    terminal.send(b"n");
    terminal.wait_for("SECOND_PAGE");
    terminal.send(b"p");
    terminal.wait_for("Hello, world!");
    terminal.send(b"t");
    terminal.wait_for("REPLY_BODY");
    terminal.send(b"@\x7f\x7f\x7f\x7f\x7fmissing\r");
    terminal.wait_for("not found");
    terminal.send(b"@\x7f\x7f\x7f\x7f\x7falice\r");
    terminal.wait_for("Hello, world!");
    terminal.send(b"c");
    terminal.wait_for("DZ_SSH_KEY");
    terminal.send(b"q");
    terminal.finish();
    let requests = server.requests();
    for path in [
        "/api/v1/users/alice/commits?cursor=next",
        "/api/v1/users/alice/commits?cursor=first",
        "/api/v1/users/alice/commits?replies=1",
    ] {
        assert!(
            requests.iter().any(|request| request.path == path),
            "{requests:?}"
        );
    }
}

#[test]
fn username_defaults_and_explicit_override() {
    let server = Server::new();
    for (args, username) in [
        (vec![], "@alice"),
        (vec!["tui"], "alice"),
        (vec!["tui", "alice"], "../invalid"),
    ] {
        let first_request = server.requests().len();
        let mut terminal = Terminal::start(&server, &args, Some(username), None);
        terminal.wait_for("Hello, world!");
        assert_eq!(server.requests()[first_request].path, "/api/v1/users/alice");
        terminal.send(b"q");
        terminal.finish();
    }
}

#[test]
fn quit_while_loading() {
    let server = Server::new();
    let mut terminal = Terminal::start(&server, &["tui", "slow"], None, None);
    terminal.wait_for("Loading");
    let started = Instant::now();
    terminal.send(b"q");
    assert!(terminal.wait_exit(Duration::from_secs(1)).success());
    assert!(started.elapsed() < Duration::from_secs(1));
    terminal.finish();
}

#[test]
fn compose_paste_review_and_cancel_discard() {
    let server = Server::new();
    let mut terminal = Terminal::start(&server, &[], Some("alice"), Some(&server.key));
    terminal.wait_for("Hello, world!");
    terminal.send(b"c");
    terminal.wait_for("New post");
    terminal.send(b"\x1b[200~A TUI draft\x13\nsecond line\x1b[201~");
    terminal.send(b"\x13");
    terminal.wait_for("Review post");
    assert!(server.posts().is_empty());
    terminal.send(b"\x1b");
    terminal.wait_for("New post");
    terminal.send(b"\x1b");
    terminal.wait_for("Discard draft?");
    terminal.send(b"\x1b");
    terminal.wait_for("New post");
    terminal.send(b"\x13");
    terminal.wait_for("Review post");
    terminal.send(b"\r");
    terminal.wait_for("Posted");
    let posts = server.posts();
    assert_eq!(posts.len(), 1);
    assert!(posts[0].valid);
    assert_eq!(
        posts[0].payload.splitn(4, '\n').skip(2).collect::<Vec<_>>(),
        ["", "A TUI draft\nsecond line"]
    );
    terminal.send(b"\r");
    terminal.wait_for("Hello, world!");
    terminal.send(b"q");
    terminal.finish();
}

#[test]
fn reply_and_recover_failed_send() {
    let server = Server::new();
    let mut terminal = Terminal::start(
        &server,
        &["tui", "alice", "--key", server.key.to_str().unwrap()],
        None,
        Some(std::path::Path::new("missing-key")),
    );
    terminal.wait_for("Hello, world!");
    terminal.send(b"R");
    terminal.wait_for("Reply to @alice");
    terminal.send(b"A retained reply\x13");
    terminal.wait_for("Review reply");
    server.set_status(500);
    terminal.send(b"\r");
    terminal.wait_for("Mock rejection");
    assert_eq!(server.posts().len(), 1);
    server.set_status(201);
    terminal.send(b"\x13");
    terminal.wait_for("Review reply");
    terminal.send(b"\r");
    terminal.wait_for("Posted");
    let posts = server.posts();
    assert_eq!(posts.len(), 2);
    assert!(posts[1].valid);
    let original: Value = serde_json::from_str(include_str!("../fixtures/commit.json")).unwrap();
    assert_eq!(
        posts[1].payload.splitn(4, '\n').skip(2).collect::<Vec<_>>(),
        [
            original["data"]["html_url"].as_str().unwrap(),
            "A retained reply"
        ],
    );
    terminal.send(b"\rq");
    terminal.finish();
}

#[test]
fn recover_draft_after_signing_failure() {
    let server = Server::new();
    let missing = server.directory.path().join("key-added-later");
    let mut terminal = Terminal::start(&server, &[], Some("alice"), Some(&missing));
    terminal.wait_for("Hello, world!");
    terminal.send(b"c");
    terminal.wait_for("New post");
    terminal.send(b"Keep my draft after signing fails\x13");
    terminal.wait_for("Review post");
    terminal.send(b"\r");
    terminal.wait_for("Not sent.");
    assert!(server.posts().is_empty());
    fs::copy(&server.key, missing).unwrap();
    terminal.send(b"\x13");
    terminal.wait_for("Review post");
    terminal.send(b"\r");
    terminal.wait_for("Posted");
    let posts = server.posts();
    assert_eq!(posts.len(), 1);
    assert_eq!(
        posts[0].payload.splitn(4, '\n').nth(3).unwrap(),
        "Keep my draft after signing fails"
    );
    assert!(posts[0].valid);
    terminal.send(b"\rq");
    terminal.finish();
}

#[test]
fn encrypted_key_passphrase_prompt() {
    let server = Server::new();
    let key = server.directory.path().join("encrypted");
    http::generate_key(&key, "test-passphrase");
    http::allow_key(server.directory.path(), &key);
    let mut terminal = Terminal::start(&server, &[], Some("alice"), Some(&key));
    terminal.wait_for("Hello, world!");
    terminal.send(b"c");
    terminal.wait_for("New post");
    terminal.send(b"Encrypted key post\x13");
    terminal.wait_for("Review post");
    terminal.send(b"\r");
    terminal.wait_for("Enter passphrase");
    terminal.send(b"test-passphrase\r");
    terminal.wait_for("Posted");
    let posts = server.posts();
    assert_eq!(posts.len(), 1);
    assert!(posts[0].valid);
    terminal.send(b"\rq");
    terminal.finish();
}
