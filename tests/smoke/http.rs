use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU16, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::Value;
use tempfile::TempDir;

use super::{checked, run_with_input};

#[derive(Clone, Debug)]
pub struct Request {
    pub path: String,
    pub payload: String,
    pub valid: bool,
    pub content_type: String,
}

pub struct Server {
    pub directory: TempDir,
    pub key: PathBuf,
    pub base: String,
    requests: Arc<Mutex<Vec<Request>>>,
    status: Arc<AtomicU16>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl Server {
    pub fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let key = directory.path().join("test key");
        generate_key(&key, "");
        allow_key(directory.path(), &key);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let status = Arc::new(AtomicU16::new(201));
        let stop = Arc::new(AtomicBool::new(false));
        let worker = {
            let requests = Arc::clone(&requests);
            let status = Arc::clone(&status);
            let stop = Arc::clone(&stop);
            let directory = directory.path().to_owned();
            thread::spawn(move || {
                let mut handlers = Vec::new();
                while !stop.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let requests = Arc::clone(&requests);
                            let status = Arc::clone(&status);
                            let directory = directory.clone();
                            handlers.push(thread::spawn(move || {
                                serve(stream, &directory, &requests, &status)
                            }));
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(error) => panic!("{error}"),
                    }
                }
                for handler in handlers {
                    handler.join().unwrap();
                }
            })
        };
        Self {
            directory,
            key,
            base,
            requests,
            status,
            stop,
            worker: Some(worker),
        }
    }

    pub fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }

    pub fn posts(&self) -> Vec<Request> {
        self.requests()
            .into_iter()
            .filter(|request| !request.payload.is_empty())
            .collect()
    }

    pub fn set_status(&self, status: u16) {
        self.status.store(status, Ordering::SeqCst);
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let result = self.worker.take().unwrap().join();
        if !thread::panicking() {
            result.unwrap();
        }
    }
}

pub fn generate_key(key: &Path, passphrase: &str) {
    checked(
        Command::new("ssh-keygen")
            .args([
                "-q",
                "-t",
                "ed25519",
                "-N",
                passphrase,
                "-C",
                "dev-zero-test",
                "-f",
            ])
            .arg(key),
    );
}

pub fn allow_key(directory: &Path, key: &Path) {
    let public = fs::read_to_string(key.with_extension("pub")).unwrap();
    fs::write(directory.join("allowed_signers"), format!("test {public}")).unwrap();
}

fn serve(
    mut stream: TcpStream,
    directory: &Path,
    requests: &Mutex<Vec<Request>>,
    status: &AtomicU16,
) {
    // Accepted sockets inherit O_NONBLOCK on macOS.
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut bytes = Vec::new();
    let header_end = loop {
        if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break index + 4;
        }
        let mut buffer = [0; 4096];
        let count = stream.read(&mut buffer).unwrap();
        assert!(count > 0, "client closed before sending headers");
        bytes.extend_from_slice(&buffer[..count]);
        assert!(bytes.len() < 65536, "oversized request headers");
    };
    let headers = String::from_utf8(bytes[..header_end].to_vec()).unwrap();
    let mut first_line = headers.lines().next().unwrap().split_whitespace();
    let method = first_line.next().unwrap();
    let path = first_line.next().unwrap().to_owned();
    let header = |name: &str| -> &str {
        headers
            .lines()
            .skip(1)
            .filter_map(|line| line.split_once(':'))
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map_or("", |(_, value)| value.trim())
    };
    let length = header("Content-Length").parse::<usize>().unwrap_or(0);
    assert!(length <= 65536);
    while bytes.len() < header_end + length {
        let mut buffer = [0; 4096];
        let count = stream.read(&mut buffer).unwrap();
        assert!(count > 0, "client closed before sending body");
        bytes.extend_from_slice(&buffer[..count]);
    }
    let payload = String::from_utf8(bytes[header_end..header_end + length].to_vec()).unwrap();
    if method == "POST" {
        let signature = header("X-SSH-Signature");
        let file = tempfile::NamedTempFile::new_in(directory).unwrap();
        let mut pem = String::from("-----BEGIN SSH SIGNATURE-----\n");
        for chunk in signature.as_bytes().chunks(70) {
            pem.push_str(std::str::from_utf8(chunk).unwrap());
            pem.push('\n');
        }
        pem.push_str("-----END SSH SIGNATURE-----\n");
        fs::write(file.path(), pem).unwrap();
        let verified = run_with_input(
            Command::new("ssh-keygen")
                .args(["-Y", "verify", "-f"])
                .arg(directory.join("allowed_signers"))
                .args(["-I", "test", "-n", "devnull-commit", "-s"])
                .arg(file.path()),
            Some(&payload),
        );
        let valid = STANDARD
            .decode(signature)
            .is_ok_and(|value| value.starts_with(b"SSHSIG"))
            && verified.status.success();
        requests.lock().unwrap().push(Request {
            path,
            payload,
            valid,
            content_type: header("Content-Type").to_owned(),
        });
        let status = status.load(Ordering::SeqCst);
        if status != 0 {
            let body = if status == 201 {
                "Committed: https://dev-null.sh/@test/commits/test-id (pending review)\n"
            } else {
                "Mock rejection.\n"
            };
            respond(&mut stream, status, "text/plain; charset=utf-8", body);
        }
    } else {
        requests.lock().unwrap().push(Request {
            path: path.clone(),
            payload,
            valid: false,
            content_type: String::new(),
        });
        let route = path.split('?').next().unwrap();
        if route.ends_with("/slow") {
            thread::sleep(Duration::from_secs(2));
        }
        if route.ends_with("/missing") {
            respond(&mut stream, 404, "text/plain", "Not found");
            return;
        }
        let commits = route.ends_with("/commits");
        let mut body: Value = serde_json::from_str(if commits {
            include_str!("../fixtures/commits.json")
        } else {
            include_str!("../fixtures/user.json")
        })
        .unwrap();
        if commits {
            let url = reqwest::Url::parse(&format!("http://localhost{path}")).unwrap();
            if url
                .query_pairs()
                .any(|(key, value)| key == "cursor" && value != "first")
            {
                body["data"][0]["body"] = "SECOND_PAGE".into();
                body["meta"]["next_cursor"] = Value::Null;
                body["meta"]["prev_cursor"] = "first".into();
            }
            if url.query_pairs().any(|(key, _)| key == "replies") {
                body["data"][0]["body"] = "REPLY_BODY".into();
            }
        }
        respond(&mut stream, 200, "application/json", &body.to_string());
    }
}

fn respond(stream: &mut TcpStream, status: u16, content_type: &str, body: &str) {
    let location = if status == 302 {
        "Location: /redirected\r\n"
    } else {
        ""
    };
    let response = format!(
        "HTTP/1.1 {status} Mock\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n{location}Connection: close\r\n\r\n{body}",
        body.len()
    );
    // Quitting during a slow read intentionally closes the client first.
    let _ = stream.write_all(response.as_bytes());
}
