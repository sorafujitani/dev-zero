//! Posting with a GitHub SSH key through `/api/cli/commits`, the GitHub Action's endpoint.
//!
//! Signed-in sessions post through `/api/v1/commits` instead; see `Api::create_commit`.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use crate::{api::Api, sign};

pub const NAMESPACE: &str = "devnull-commit";
pub const MAX_CHARS: usize = 4000;

pub struct SignedPost {
    payload: String,
    signature: String,
}

impl SignedPost {
    pub fn send(self, api: &Api) -> Result<String> {
        api.post_with_github_key(self.payload, &self.signature)
    }
}

/// Signing may ask for a passphrase, so the TUI calls this outside raw mode.
pub fn sign(message: &str, reply: Option<&str>, key: &Path) -> Result<SignedPost> {
    let payload = payload(message, reply)?;
    let signature = sign::ssh_sign(NAMESPACE, &payload, key)?;
    Ok(SignedPost { payload, signature })
}

/// The message as the server will store it: trimmed, 3–4000 characters.
pub fn validate(message: &str) -> Result<&str> {
    let message = message.trim();
    if !(3..=MAX_CHARS).contains(&message.chars().count()) {
        bail!("message must contain 3–{MAX_CHARS} characters after trimming");
    }
    Ok(message)
}

fn validate_reply(reply: Option<&str>) -> Result<&str> {
    let reply = reply.unwrap_or("");
    if reply.len() > 2048
        || !reply
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b":/._@-".contains(&c))
    {
        bail!("--reply must be a commit hash or URL");
    }
    Ok(reply)
}

fn payload(message: &str, reply: Option<&str>) -> Result<String> {
    let message = validate(message)?;
    let reply = validate_reply(reply)?;
    Ok(format!(
        "{}\n{}\n{reply}\n{message}",
        sign::timestamp()?,
        sign::nonce()?
    ))
}

/// Where a post is signed: a session device key through the API, or a GitHub key through the CLI endpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Poster {
    Session,
    GitHubKey(PathBuf),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_preserves_multiline_text_and_uses_fresh_nonces() {
        let first = payload("  Hello\nworld!\n", Some("abc1234")).unwrap();
        let second = payload("Hello\nworld!", Some("abc1234")).unwrap();
        let parts: Vec<_> = first.splitn(4, '\n').collect();
        assert!(parts[0].parse::<u64>().unwrap() > 0);
        assert_eq!(parts[1].len(), 32);
        assert_ne!(parts[1], second.split('\n').nth(1).unwrap());
        assert_eq!(parts[2], "abc1234");
        assert_eq!(parts[3], "Hello\nworld!");
        for body in ["", " ab ", &"x".repeat(4001)] {
            assert!(payload(body, None).is_err());
        }
        assert!(payload("hello", Some("id\ninjected")).is_err());
    }
}
