use std::{
    collections::HashSet,
    path::Path,
    process::{Command, Output},
    time::{SystemTime, UNIX_EPOCH},
};

use super::{BINARY, http::Server, run_with_input};

fn invoke(server: &Server, base: &str, key: &Path, args: &[&str], input: Option<&str>) -> Output {
    run_with_input(
        Command::new(BINARY)
            .args(["--base-url", base, "post"])
            .args(args)
            .env_remove("DZ_BASE_URL")
            .env("DZ_SSH_KEY", key)
            .env("DZ_CONFIG_DIR", server.directory.path().join("no-session")),
        input,
    )
}

#[test]
fn posting_signatures_stdin_replies_and_errors() {
    let server = Server::new();
    let key = server.key.to_str().unwrap();
    let mut nonces = HashSet::new();
    for (args, input, reply, message) in [
        (vec!["Hello from dz"], None, "", "Hello from dz"),
        (
            vec!["--stdin", "--reply", "abc1234"],
            Some("  A multiline\nreply!\n"),
            "abc1234",
            "A multiline\nreply!",
        ),
        (
            vec!["Explicit key wins", "--key", key],
            None,
            "",
            "Explicit key wins",
        ),
    ] {
        let env_key = if args.contains(&"--key") {
            server.directory.path().join("missing")
        } else {
            server.key.clone()
        };
        let output = invoke(&server, &server.base, &env_key, &args, input);
        assert!(output.status.success(), "{output:?}");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("Committed:") && stdout.contains("pending review"));
        let posts = server.posts();
        let request = posts.last().unwrap();
        assert_eq!(request.path, "/api/cli/commits");
        assert!(request.valid);
        assert_eq!(request.content_type, "text/plain; charset=utf-8");
        let parts: Vec<_> = request.payload.splitn(4, '\n').collect();
        let timestamp: u64 = parts[0].parse().unwrap();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert!(timestamp.abs_diff(now) < 10);
        assert_eq!(parts[1].len(), 32);
        assert!(
            parts[1]
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        );
        assert!(nonces.insert(parts[1].to_owned()));
        assert_eq!(parts[2..], [reply, message]);
    }

    for status in [401, 422, 429, 500, 302, 0] {
        server.set_status(status);
        let before = server.posts().len();
        let output = invoke(
            &server,
            &server.base,
            &server.key,
            &["No automatic retry"],
            None,
        );
        assert!(
            !output.status.success() && output.stdout.is_empty(),
            "{output:?}"
        );
        assert_eq!(server.posts().len(), before + 1);
        let expected = if status == 0 {
            "may have succeeded".to_owned()
        } else {
            format!("HTTP {status}")
        };
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(&expected),
            "{output:?}"
        );
    }
    server.set_status(201);

    let before = server.requests().len();
    let long_body = "x".repeat(4001);
    let long_stdin = "x".repeat(16001);
    let missing = server.directory.path().join("missing");
    for (args, input) in [
        (vec!["ab"], None),
        (vec![long_body.as_str()], None),
        (vec!["--stdin"], Some(long_stdin.as_str())),
        (vec!["Text and stdin", "--stdin"], Some("body")),
        (vec!["A valid body", "--reply", "id\ninjected"], None),
        (
            vec!["A valid body", "--key", missing.to_str().unwrap()],
            None,
        ),
        (vec!["--json", "A valid body"], None),
    ] {
        let output = invoke(&server, &server.base, &server.key, &args, input);
        assert!(
            !output.status.success() && output.stdout.is_empty(),
            "{output:?}"
        );
    }
    let output = invoke(
        &server,
        "http://example.invalid",
        &server.key,
        &["Refuse insecure posting"],
        None,
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("requires HTTPS"));
    assert_eq!(server.requests().len(), before);
}
