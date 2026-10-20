use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::{
    Method, StatusCode, Url,
    blocking::{Client, RequestBuilder},
};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};

use crate::{
    model::{CommitId, Cursor, Feed, Log, NotificationsPage, Page, Username, UsersPage},
    session::Signer,
};

/// A successful API response with the rate limit budget the server reported.
pub struct Response {
    pub body: Value,
    /// `X-RateLimit-Remaining`: requests left in the current minute, when the server says.
    pub requests_left: Option<u32>,
}

/// The `message` body dev-null returns with every error status, with a stable `code` for the ones apps tell apart.
#[derive(Deserialize)]
struct ApiError {
    message: String,
    code: Option<String>,
}

#[derive(Clone)]
pub struct Api {
    client: Client,
    base: Url,
    signer: Option<Signer>,
}

impl Api {
    pub fn new(base: &str) -> Result<Self> {
        let base = Url::parse(base).context("invalid API origin")?;
        if !matches!(base.scheme(), "http" | "https")
            || base.host_str().is_none()
            || base.path() != "/"
            || base.query().is_some()
            || base.fragment().is_some()
            || !base.username().is_empty()
            || base.password().is_some()
        {
            bail!("base URL must be an HTTP(S) origin, e.g. https://dev-null.sh");
        }
        Ok(Self {
            client: Client::builder()
                .user_agent(concat!("dev-zero/", env!("CARGO_PKG_VERSION")))
                .timeout(Duration::from_secs(15))
                .connect_timeout(Duration::from_secs(5))
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            base,
            signer: None,
        })
    }

    /// Signs every request from now on; public reads then return the user's view too.
    pub fn with_signer(mut self, signer: Option<Signer>) -> Self {
        self.signer = signer;
        self
    }

    pub fn is_signed(&self) -> bool {
        self.signer.is_some()
    }

    pub fn base(&self) -> &Url {
        &self.base
    }

    // Public reads, personalised when signed.

    pub fn user(&self, username: &Username) -> Result<Response> {
        self.get(&["users", username.as_str()], &[])
    }

    pub fn commits(
        &self,
        username: &Username,
        feed: Feed,
        cursor: Option<&Cursor>,
    ) -> Result<Response> {
        let mut query = Vec::new();
        if feed == Feed::Replies {
            query.push(("replies", "1"));
        }
        push_cursor(&mut query, cursor);
        self.get(&["users", username.as_str(), "commits"], &query)
    }

    pub fn followers(&self, username: &Username, cursor: Option<&Cursor>) -> Result<Response> {
        let mut query = Vec::new();
        push_cursor(&mut query, cursor);
        self.get(&["users", username.as_str(), "followers"], &query)
    }

    pub fn following(&self, username: &Username, cursor: Option<&Cursor>) -> Result<Response> {
        let mut query = Vec::new();
        push_cursor(&mut query, cursor);
        self.get(&["users", username.as_str(), "following"], &query)
    }

    pub fn commit(&self, id: &CommitId) -> Result<Response> {
        self.get(&["commits", id.as_str()], &[])
    }

    // Signed reads.

    pub fn me(&self) -> Result<Response> {
        self.require_session()?;
        self.get(&["me"], &[])
    }

    pub fn log(&self, log: Log, cursor: Option<&Cursor>) -> Result<Response> {
        self.require_session()?;
        let mut query = Vec::new();
        push_cursor(&mut query, cursor);
        self.get(&["log", log.path()], &query)
    }

    /// Replies to a commit, newest first like the web's thread view.
    pub fn replies(&self, id: &CommitId, cursor: Option<&Cursor>) -> Result<Response> {
        self.require_session()?;
        let mut query = vec![("newest", "1")];
        push_cursor(&mut query, cursor);
        self.get(&["commits", id.as_str(), "replies"], &query)
    }

    pub fn saved(&self, cursor: Option<&Cursor>) -> Result<Response> {
        self.require_session()?;
        let mut query = Vec::new();
        push_cursor(&mut query, cursor);
        self.get(&["saved"], &query)
    }

    /// Lists notifications and, as on the web, marks them all read.
    pub fn notifications(&self, cursor: Option<&Cursor>) -> Result<Response> {
        self.require_session()?;
        let mut query = Vec::new();
        push_cursor(&mut query, cursor);
        self.get(&["notifications"], &query)
    }

    // Signed writes.

    pub fn create_commit(
        &self,
        body: &str,
        parent_id: Option<&str>,
        quoted_commit_id: Option<&str>,
    ) -> Result<Response> {
        self.require_session()?;
        let mut payload = json!({ "body": body });
        if let Some(parent) = parent_id {
            payload["parent_id"] = json!(parent);
        }
        if let Some(quoted) = quoted_commit_id {
            payload["quoted_commit_id"] = json!(quoted);
        }
        self.send(Method::POST, &["commits"], &[], Some(payload))
    }

    pub fn delete_commit(&self, id: &CommitId) -> Result<Response> {
        self.require_session()?;
        self.send(Method::DELETE, &["commits", id.as_str()], &[], None)
    }

    pub fn set_star(&self, id: &CommitId, on: bool) -> Result<Response> {
        self.toggle(&["commits", id.as_str(), "star"], on, Method::POST)
    }

    pub fn set_saved(&self, id: &CommitId, on: bool) -> Result<Response> {
        self.toggle(&["commits", id.as_str(), "bookmark"], on, Method::PUT)
    }

    pub fn set_follow(&self, username: &Username, on: bool) -> Result<Response> {
        self.toggle(&["users", username.as_str(), "follow"], on, Method::POST)
    }

    pub fn set_mute(&self, username: &Username, on: bool) -> Result<Response> {
        self.toggle(&["users", username.as_str(), "mute"], on, Method::POST)
    }

    pub fn set_block(&self, username: &Username, on: bool) -> Result<Response> {
        self.toggle(&["users", username.as_str(), "block"], on, Method::POST)
    }

    pub fn report(&self, id: &CommitId, reason: &str, details: Option<&str>) -> Result<Response> {
        self.require_session()?;
        let mut payload = json!({ "reason": reason });
        if let Some(details) = details {
            payload["details"] = json!(details);
        }
        self.send(
            Method::POST,
            &["commits", id.as_str(), "reports"],
            &[],
            Some(payload),
        )
    }

    /// Redeems the code the user approved on the web; the key is signed with but not yet approved.
    pub fn register_device(&self, signer: &Signer, code: &str, verifier: &str) -> Result<Response> {
        let signed = self.clone().with_signer(Some(signer.clone()));
        signed.send(
            Method::POST,
            &["devices"],
            &[],
            Some(json!({ "code": code, "code_verifier": verifier })),
        )
    }

    /// `POST /api/cli/commits`: the GitHub Action's endpoint, signed with a GitHub SSH key.
    pub fn post_with_github_key(&self, payload: String, signature: &str) -> Result<String> {
        let host = self.base.host_str().unwrap_or_default();
        let loopback = host == "localhost"
            || host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback());
        if self.base.scheme() != "https" && !loopback {
            bail!("posting requires HTTPS (HTTP is allowed only for loopback testing)");
        }
        let response = self.client.post(self.base.join("api/cli/commits")?)
            .header("Content-Type", "text/plain; charset=utf-8")
            .header("Accept", "text/plain")
            .header("X-SSH-Signature", signature)
            .body(payload)
            .send()
            .context("posting request failed; it may have succeeded — check your profile before retrying")?;
        let status = response.status();
        let text = response
            .text()
            .context("could not read the posting response; check your profile before retrying")?;
        if status != StatusCode::CREATED {
            bail!("posting failed (HTTP {status}): {}", text.trim());
        }
        Ok(text)
    }

    fn require_session(&self) -> Result<()> {
        if self.signer.is_none() {
            bail!("this needs a signed-in session; run dz login");
        }
        Ok(())
    }

    fn toggle(&self, segments: &[&str], on: bool, method_on: Method) -> Result<Response> {
        self.require_session()?;
        let method = if on { method_on } else { Method::DELETE };
        self.send(method, segments, &[], None)
    }

    fn get(&self, segments: &[&str], query: &[(&str, &str)]) -> Result<Response> {
        self.send(Method::GET, segments, query, None)
    }

    fn url(&self, segments: &[&str], query: &[(&str, &str)]) -> Url {
        let mut url = self.base.clone();
        url.path_segments_mut()
            // `new` only accepts HTTP(S) origins, which are never cannot-be-a-base URLs.
            .expect("HTTP origin")
            .extend(["api", "v1"])
            .extend(segments);
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query);
        }
        url
    }

    fn send(
        &self,
        method: Method,
        segments: &[&str],
        query: &[(&str, &str)],
        body: Option<Value>,
    ) -> Result<Response> {
        let url = self.url(segments, query);
        let body = body.map(|value| value.to_string()).unwrap_or_default();
        let mut request: RequestBuilder = self
            .client
            .request(method.clone(), url.clone())
            .header("Accept", "application/json");
        if let Some(signer) = &self.signer {
            // The signature covers the path and query exactly as sent.
            let uri = match url.query() {
                Some(query) => format!("{}?{query}", url.path()),
                None => url.path().to_owned(),
            };
            for (name, value) in signer.headers(method.as_str(), &uri, &body)? {
                request = request.header(name, value);
            }
        } else if !body.is_empty() {
            bail!("this needs a signed-in session; run dz login");
        }
        if !body.is_empty() {
            request = request
                .header("Content-Type", "application/json")
                .body(body);
        }
        let response = request.send().context("could not reach dev-null API")?;
        let status = response.status();
        let header = |name: &str| {
            response
                .headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
        };
        let requests_left = header("x-ratelimit-remaining").and_then(|v| v.parse().ok());
        if status == StatusCode::TOO_MANY_REQUESTS {
            let retry = header("retry-after").unwrap_or_else(|| "a short wait".into());
            bail!("API rate limit reached; retry after {retry}");
        }
        if status == StatusCode::NO_CONTENT {
            return Ok(Response {
                body: Value::Null,
                requests_left,
            });
        }
        if !status.is_success() {
            let error = response.json::<ApiError>().ok();
            match error.as_ref().and_then(|e| e.code.as_deref()) {
                Some("device_not_approved") => {
                    bail!("this device is no longer approved; run dz logout, then dz login")
                }
                Some("invalid_signature") => bail!(
                    "the request signature was rejected; check the clock and run dz login again"
                ),
                Some("user_blocked") => {
                    bail!("you blocked this user; unblock them to see their posts")
                }
                Some("user_unavailable") => bail!("this profile is not available to you"),
                Some("commit_unavailable") => bail!("this commit is not available"),
                _ => {}
            }
            // The 404 body names the server model, so keep our own wording for it.
            if status == StatusCode::NOT_FOUND {
                bail!("not found: the user or commit does not exist");
            }
            let detail = error
                .map(|e| format!(": {}", e.message))
                .unwrap_or_default();
            bail!("API returned HTTP {status}{detail}");
        }
        let body = response.json().context("API returned invalid JSON")?;
        Ok(Response {
            body,
            requests_left,
        })
    }
}

fn push_cursor<'a>(query: &mut Vec<(&'a str, &'a str)>, cursor: Option<&'a Cursor>) {
    if let Some(cursor) = cursor {
        query.push(("cursor", cursor.as_str()));
    }
}

/// Extracts and decodes the `data` envelope of a single-resource response.
pub fn data<T: DeserializeOwned>(mut response: Value) -> Result<T> {
    let data = response
        .get_mut("data")
        .context("API response has no data")?
        .take();
    serde_json::from_value(data).context("unexpected API data")
}

/// Decodes a paginated commits response.
pub fn page(response: Value) -> Result<Page> {
    serde_json::from_value(response).context("unexpected API page")
}

pub fn users_page(response: Value) -> Result<UsersPage> {
    serde_json::from_value(response).context("unexpected API user list")
}

pub fn notifications_page(response: Value) -> Result<NotificationsPage> {
    serde_json::from_value(response).context("unexpected API notification list")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_origin_base_urls() {
        for url in [
            "file:///tmp/a",
            "https://example.com/api",
            "https://a:b@example.com",
            "https://example.com/?x=1",
        ] {
            assert!(Api::new(url).is_err());
        }
    }

    #[test]
    fn signed_only_endpoints_refuse_guests_before_any_request() {
        let api = Api::new("https://dev-null.sh").unwrap();
        let id: CommitId = "abc".parse().unwrap();
        assert!(api.log(Log::Latest, None).is_err());
        assert!(api.set_star(&id, true).is_err());
        assert!(api.create_commit("hello", None, None).is_err());
    }
}
