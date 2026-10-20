use std::{
    io::{Read, Write},
    net::TcpListener,
    process::{Command, Output},
    thread,
    time::{Duration, Instant},
};

const USER: &str = include_str!("fixtures/user.json");
const COMMITS: &str = include_str!("fixtures/commits.json");
const COMMIT: &str = include_str!("fixtures/commit.json");

fn request(args: &[&str], status: &str, headers: &str, body: &str) -> (Output, String) {
    request_with_username(args, status, headers, body, None)
}

fn request_with_username(
    args: &[&str],
    status: &str,
    headers: &str,
    body: &str,
    username: Option<&str>,
) -> (Output, String) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{headers}Connection: close\r\n\r\n{body}",
        body.len()
    );
    let server = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        let (mut stream, _) = loop {
            match listener.accept() {
                Ok(connection) => break connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "client did not connect");
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("{error}"),
            }
        };
        // macOS hands accepted sockets the listener's non-blocking flag; read must block.
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut bytes = Vec::new();
        let mut buffer = [0; 1024];
        while !bytes.windows(4).any(|w| w == b"\r\n\r\n") {
            let count = stream.read(&mut buffer).unwrap();
            assert!(count > 0, "client closed early");
            bytes.extend_from_slice(&buffer[..count]);
        }
        stream.write_all(response.as_bytes()).unwrap();
        String::from_utf8(bytes).unwrap()
    });
    let mut command = Command::new(env!("CARGO_BIN_EXE_dz"));
    command.env_remove("DZ_USERNAME");
    command.env(
        "DZ_CONFIG_DIR",
        std::env::temp_dir().join("dz-cli-test-no-session"),
    );
    if let Some(username) = username {
        command.env("DZ_USERNAME", username);
    }
    let output = command
        .args(["--base-url", &base])
        .args(args)
        .output()
        .unwrap();
    (output, server.join().unwrap())
}

#[test]
fn username_defaults_and_explicit_overrides() {
    for (args, username, body, path) in [
        (vec!["user"], "@alice", USER, "/api/v1/users/alice"),
        (
            vec!["commits"],
            "alice",
            COMMITS,
            "/api/v1/users/alice/commits",
        ),
        (
            vec!["user", "alice"],
            "../invalid",
            USER,
            "/api/v1/users/alice",
        ),
        (
            vec!["commits", "alice"],
            "../invalid",
            COMMITS,
            "/api/v1/users/alice/commits",
        ),
        (
            vec!["commit", "00000000-0000-0000-0000-000000000001"],
            "../invalid",
            COMMIT,
            "/api/v1/commits/00000000-0000-0000-0000-000000000001",
        ),
    ] {
        let (output, request) = request_with_username(&args, "200 OK", "", body, Some(username));
        assert!(output.status.success(), "{output:?}");
        assert!(
            request.starts_with(&format!("GET {path} HTTP/1.1")),
            "{request}"
        );
    }
}

#[test]
fn missing_or_invalid_username_defaults_fail() {
    for args in [vec![], vec!["tui"], vec!["user"], vec!["commits"]] {
        for username in ["", "../invalid"] {
            let output = Command::new(env!("CARGO_BIN_EXE_dz"))
                .args(&args)
                .env("DZ_USERNAME", username)
                .output()
                .unwrap();
            assert!(!output.status.success());
            assert!(
                String::from_utf8_lossy(&output.stderr).contains("username"),
                "{output:?}"
            );
        }
    }
    for command in ["user", "commits"] {
        let output = Command::new(env!("CARGO_BIN_EXE_dz"))
            .arg(command)
            .env_remove("DZ_USERNAME")
            .output()
            .unwrap();
        assert!(!output.status.success());
    }
}

#[test]
fn profile_text_and_json() {
    let (output, request) = request(&["user", "@alice"], "200 OK", "", USER);
    assert!(output.status.success(), "{:?}", output);
    assert!(request.starts_with("GET /api/v1/users/alice HTTP/1.1"));
    assert!(String::from_utf8_lossy(&output.stdout).contains("2 commits · 3 followers"));
    let (output, _) = self::request(&["user", "alice", "--json"], "200 OK", "", USER);
    assert!(output.status.success());
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap(),
        serde_json::from_str::<serde_json::Value>(USER).unwrap()
    );
}

#[test]
fn commits_encode_cursor_and_keep_reply_filter() {
    let (output, request) = request(
        &["commits", "alice", "--replies", "--cursor", "a+/=&?"],
        "200 OK",
        "",
        COMMITS,
    );
    assert!(output.status.success(), "{:?}", output);
    assert!(
        request.starts_with(
            "GET /api/v1/users/alice/commits?replies=1&cursor=a%2B%2F%3D%26%3F HTTP/1.1"
        ),
        "{request}"
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("Hello, world!"));
    assert!(text.contains("Next page: --cursor next"));
    assert!(!text.contains("Previous page"));
}

#[test]
fn cursor_accepts_links_next_url_and_shows_prev_cursor() {
    let body = COMMITS.replace("\"prev_cursor\": null", "\"prev_cursor\": \"back\"");
    let (output, request) = request(
        &[
            "commits",
            "alice",
            "--cursor",
            "https://dev-null.sh/api/v1/users/alice/commits?cursor=next",
        ],
        "200 OK",
        "",
        &body,
    );
    assert!(output.status.success(), "{:?}", output);
    assert!(
        request.starts_with("GET /api/v1/users/alice/commits?cursor=next HTTP/1.1"),
        "{request}"
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("Previous page: --cursor back"), "{text}");
}

#[test]
fn single_commit_includes_media_and_parent() {
    let body = COMMIT.replace("\"parent_id\": null", "\"parent_id\": \"parent-id\"");
    let (output, request) = request(
        &["commit", "00000000-0000-0000-0000-000000000001"],
        "200 OK",
        "",
        &body,
    );
    assert!(output.status.success());
    assert!(
        request.starts_with("GET /api/v1/commits/00000000-0000-0000-0000-000000000001 HTTP/1.1")
    );
    let text = String::from_utf8_lossy(&output.stdout);
    for expected in [
        "Image: https://example.com/image.png (640\u{d7}480)",
        "Embed: YouTube video https://example.com/embed",
        "Reply to: parent-id",
    ] {
        assert!(text.contains(expected), "{text}");
    }
}

#[test]
fn empty_page_and_unknown_json_fields() {
    let (output, _) = request(
        &["commits", "alice"],
        "200 OK",
        "",
        r#"{"data":[],"meta":{"next_cursor":null}}"#,
    );
    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout), "No commits.\n");
    let body = r#"{"data":{"future_field":42},"new_metadata":true}"#;
    let (output, _) = request(&["user", "alice", "--json"], "200 OK", "", body);
    assert!(output.status.success());
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap(),
        serde_json::from_str::<serde_json::Value>(body).unwrap()
    );
}

#[test]
fn errors_go_to_stderr_without_success_output() {
    for (status, headers, body, expected) in [
        ("404 Not Found", "", "{}", "not found"),
        (
            "429 Too Many Requests",
            "Retry-After: 30\r\n",
            "{}",
            "retry after 30",
        ),
        ("503 Service Unavailable", "", "{}", "HTTP 503"),
        (
            "403 Forbidden",
            "",
            r#"{"message":"This profile is private."}"#,
            "HTTP 403 Forbidden: This profile is private.",
        ),
        (
            "302 Found",
            "Location: http://127.0.0.1:1/\r\n",
            "{}",
            "HTTP 302",
        ),
        ("200 OK", "", "<html>", "invalid JSON"),
        ("200 OK", "", "{}", "no data"),
    ] {
        let (output, _) = request(&["user", "alice"], status, headers, body);
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(expected),
            "{:?}",
            output
        );
    }
}

#[test]
fn rejects_invalid_arguments_and_non_terminal_tui() {
    for args in [
        vec!["user", "../alice"],
        vec!["commit", "../id"],
        vec!["--json", "tui"],
        vec!["tui"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_dz"))
            .args(args)
            .env_remove("DZ_USERNAME")
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(!output.stderr.is_empty());
    }
}

/// A signed-in session in a temporary config dir: the device key dz would generate on `dz login`.
struct SessionDir {
    dir: std::path::PathBuf,
}

impl SessionDir {
    fn create(base: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "dz-cli-session-{}-{}",
            std::process::id(),
            base.rsplit(':').next().unwrap()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let status = Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-f"])
            .arg(dir.join("device_key"))
            .status()
            .unwrap();
        assert!(status.success());
        std::fs::write(
            dir.join("session.json"),
            format!(r#"{{"username":"alice","base_url":"{base}"}}"#),
        )
        .unwrap();
        Self { dir }
    }

    /// Verifies an SSHSIG over `payload` in the `devnull-api` namespace with the device key.
    fn verify(&self, signature: &str, payload: &str) -> bool {
        let public = std::fs::read_to_string(self.dir.join("device_key.pub")).unwrap();
        let key = public
            .split_whitespace()
            .take(2)
            .collect::<Vec<_>>()
            .join(" ");
        std::fs::write(self.dir.join("allowed_signers"), format!("dz {key}\n")).unwrap();
        let mut pem = String::from("-----BEGIN SSH SIGNATURE-----\n");
        for chunk in signature.as_bytes().chunks(70) {
            pem.push_str(std::str::from_utf8(chunk).unwrap());
            pem.push('\n');
        }
        pem.push_str("-----END SSH SIGNATURE-----\n");
        std::fs::write(self.dir.join("signature"), pem).unwrap();
        let mut child = Command::new("ssh-keygen")
            .args(["-Y", "verify", "-n", "devnull-api", "-I", "dz", "-f"])
            .arg(self.dir.join("allowed_signers"))
            .arg("-s")
            .arg(self.dir.join("signature"))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.as_bytes())
            .unwrap();
        child.wait().unwrap().success()
    }
}

impl Drop for SessionDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

struct Captured {
    output: Output,
    request_line: String,
    headers: Vec<(String, String)>,
    body: String,
}

/// Runs dz with a session against a one-shot server and captures the whole request.
fn signed_request(args: &[&str], status: &str, response_body: &str) -> Captured {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let session = SessionDir::create(&base);
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
        response_body.len()
    );
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut bytes = Vec::new();
        let mut buffer = [0; 4096];
        let header_end = loop {
            if let Some(index) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                break index + 4;
            }
            let count = stream.read(&mut buffer).unwrap();
            assert!(count > 0);
            bytes.extend_from_slice(&buffer[..count]);
        };
        let head = String::from_utf8(bytes[..header_end].to_vec()).unwrap();
        let mut lines = head.lines();
        let request_line = lines.next().unwrap().to_owned();
        let headers: Vec<(String, String)> = lines
            .filter_map(|line| line.split_once(": "))
            .map(|(k, v)| (k.to_ascii_lowercase(), v.to_owned()))
            .collect();
        let length: usize = headers
            .iter()
            .find(|(k, _)| k == "content-length")
            .map(|(_, v)| v.parse().unwrap())
            .unwrap_or(0);
        while bytes.len() < header_end + length {
            let count = stream.read(&mut buffer).unwrap();
            assert!(count > 0);
            bytes.extend_from_slice(&buffer[..count]);
        }
        stream.write_all(response.as_bytes()).unwrap();
        let body = String::from_utf8(bytes[header_end..].to_vec()).unwrap();
        (request_line, headers, body)
    });
    let output = Command::new(env!("CARGO_BIN_EXE_dz"))
        .env_remove("DZ_USERNAME")
        .env_remove("DZ_SSH_KEY")
        .env("DZ_CONFIG_DIR", &session.dir)
        .args(["--base-url", &base])
        .args(args)
        .output()
        .unwrap();
    let (request_line, headers, body) = server.join().unwrap();
    let header = |name: &str| {
        headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    };
    let (method, uri) = {
        let mut parts = request_line.split(' ');
        (parts.next().unwrap(), parts.next().unwrap())
    };
    let payload = format!(
        "{}\n{}\n{method}\n{uri}\n{body}",
        header("x-devnull-timestamp"),
        header("x-devnull-nonce")
    );
    assert!(
        session.verify(&header("x-ssh-signature"), &payload),
        "signature did not verify for {request_line}"
    );
    Captured {
        output,
        request_line,
        headers,
        body,
    }
}

#[test]
fn session_signs_reads_and_writes_the_way_the_server_verifies() {
    let latest = signed_request(&["log", "latest", "--cursor", "c+1"], "200 OK", COMMITS);
    assert!(latest.output.status.success(), "{:?}", latest.output);
    assert!(
        latest
            .request_line
            .starts_with("GET /api/v1/log/latest?cursor=c%2B1 HTTP/1.1"),
        "{}",
        latest.request_line
    );
    assert!(latest.body.is_empty());
    assert!(String::from_utf8_lossy(&latest.output.stdout).contains("Hello, world!"));

    let star = signed_request(
        &["star", "abc-123"],
        "200 OK",
        r#"{"data":{"starred":true}}"#,
    );
    assert!(star.output.status.success(), "{:?}", star.output);
    assert!(
        star.request_line
            .starts_with("POST /api/v1/commits/abc-123/star HTTP/1.1")
    );
    assert!(String::from_utf8_lossy(&star.output.stdout).contains("Starred abc-123"));

    let unstar = signed_request(&["star", "abc-123", "--undo"], "204 No Content", "");
    assert!(unstar.output.status.success(), "{:?}", unstar.output);
    assert!(
        unstar
            .request_line
            .starts_with("DELETE /api/v1/commits/abc-123/star HTTP/1.1")
    );

    let reply = signed_request(
        &["post", "Hello from a session", "--reply", "parent-1"],
        "201 Created",
        COMMIT,
    );
    assert!(reply.output.status.success(), "{:?}", reply.output);
    assert!(
        reply
            .request_line
            .starts_with("POST /api/v1/commits HTTP/1.1")
    );
    assert!(
        reply
            .headers
            .iter()
            .any(|(k, v)| k == "content-type" && v.starts_with("application/json"))
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&reply.body).unwrap(),
        serde_json::json!({"body": "Hello from a session", "parent_id": "parent-1"})
    );
    assert!(String::from_utf8_lossy(&reply.output.stdout).contains("Posted 0000001"));

    // A revoked device is explained, not shown as a bare 401.
    let revoked = signed_request(
        &["me"],
        "401 Unauthorized",
        r#"{"message":"This key isn't approved.","code":"device_not_approved"}"#,
    );
    assert!(!revoked.output.status.success());
    assert!(String::from_utf8_lossy(&revoked.output.stderr).contains("dz login"));
}

#[test]
fn signed_only_commands_explain_the_missing_session() {
    for args in [
        vec!["log"],
        vec!["me"],
        vec!["star", "abc"],
        vec!["notifications"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_dz"))
            .env(
                "DZ_CONFIG_DIR",
                std::env::temp_dir().join("dz-cli-test-no-session"),
            )
            .args(["--base-url", "http://127.0.0.1:1"])
            .args(args)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("dz login"));
    }
}
