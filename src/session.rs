//! The signed-in device: an ed25519 key dz generates, approved by the user on the web.
//!
//! Files live in `$DZ_CONFIG_DIR`, else `$XDG_CONFIG_HOME/dz`, else `~/.config/dz`:
//! `device_key`, `device_key.pub` and `session.json`.

use std::{
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::sign;

pub const NAMESPACE: &str = "devnull-api";

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct Session {
    pub username: String,
    pub base_url: String,
}

/// Signs API requests with the device key; exists only while a session is saved.
#[derive(Clone, Debug)]
pub struct Signer {
    key: PathBuf,
}

impl Signer {
    /// The three headers for one request. `uri` is the path with its query string exactly as sent.
    pub fn headers(
        &self,
        method: &str,
        uri: &str,
        body: &str,
    ) -> Result<[(&'static str, String); 3]> {
        let timestamp = sign::timestamp()?.to_string();
        let nonce = sign::nonce()?;
        let payload = format!("{timestamp}\n{nonce}\n{method}\n{uri}\n{body}");
        let signature = sign::ssh_sign(NAMESPACE, &payload, &self.key)?;
        Ok([
            ("X-Devnull-Timestamp", timestamp),
            ("X-Devnull-Nonce", nonce),
            ("X-SSH-Signature", signature),
        ])
    }
}

pub struct Store {
    dir: PathBuf,
}

impl Store {
    pub fn locate() -> Result<Self> {
        let dir = if let Some(dir) = std::env::var_os("DZ_CONFIG_DIR") {
            PathBuf::from(dir)
        } else if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
            PathBuf::from(xdg).join("dz")
        } else {
            PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?)
                .join(".config")
                .join("dz")
        };
        Ok(Self { dir })
    }

    fn key_path(&self) -> PathBuf {
        self.dir.join("device_key")
    }

    fn session_path(&self) -> PathBuf {
        self.dir.join("session.json")
    }

    pub fn session(&self) -> Result<Option<Session>> {
        match fs::read(self.session_path()) {
            Ok(bytes) => Ok(Some(
                serde_json::from_slice(&bytes).context("session.json is corrupt; run dz logout")?,
            )),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).context("could not read session.json"),
        }
    }

    /// The signer for `base`, when a session for that origin exists and its key is still there.
    pub fn signer(&self, base: &Url) -> Result<Option<Signer>> {
        let Some(session) = self.session()? else {
            return Ok(None);
        };
        if session.base_url.trim_end_matches('/') != base.as_str().trim_end_matches('/') {
            return Ok(None);
        }
        let key = self.key_path();
        if !key.exists() {
            bail!("the device key is missing; run dz logout, then dz login");
        }
        Ok(Some(Signer { key }))
    }

    /// Generates the ed25519 device key when missing and returns the signer for it.
    pub fn ensure_key(&self) -> Result<Signer> {
        let key = self.key_path();
        if !key.exists() {
            fs::create_dir_all(&self.dir).context("could not create the dz config directory")?;
            restrict(&self.dir)?;
            let host = hostname().unwrap_or_else(|| "this machine".into());
            let status = Command::new("ssh-keygen")
                .args(["-q", "-t", "ed25519", "-N", "", "-C"])
                .arg(format!("dz on {host}"))
                .arg("-f")
                .arg(&key)
                .status()
                .context("could not start ssh-keygen; install OpenSSH 8.2 or newer")?;
            if !status.success() {
                bail!("ssh-keygen could not create the device key");
            }
        }
        Ok(Signer { key })
    }

    /// `SHA256:…` of the device public key, as the approval page shows it.
    pub fn fingerprint(&self) -> Result<String> {
        let output = Command::new("ssh-keygen")
            .args(["-l", "-E", "sha256", "-f"])
            .arg(self.key_path().with_extension("pub"))
            .output()
            .context("could not run ssh-keygen -l")?;
        if !output.status.success() {
            bail!("could not read the device key fingerprint");
        }
        String::from_utf8_lossy(&output.stdout)
            .split_whitespace()
            .find(|word| word.starts_with("SHA256:"))
            .map(str::to_owned)
            .context("ssh-keygen printed no SHA256 fingerprint")
    }

    pub fn save(&self, session: &Session) -> Result<()> {
        fs::create_dir_all(&self.dir).context("could not create the dz config directory")?;
        restrict(&self.dir)?;
        let path = self.session_path();
        let mut file = fs::File::create(&path).context("could not write session.json")?;
        restrict(&path)?;
        file.write_all(&serde_json::to_vec_pretty(session)?)?;
        file.write_all(b"\n")?;
        Ok(())
    }

    /// Removes the session and the device key; the server-side device stays until revoked on the web.
    pub fn clear(&self) -> Result<bool> {
        let mut removed = false;
        for path in [
            self.session_path(),
            self.key_path(),
            self.key_path().with_extension("pub"),
        ] {
            match fs::remove_file(&path) {
                Ok(()) => removed = true,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("could not remove {}", path.display()));
                }
            }
        }
        Ok(removed)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Where the `devnull://` handler drops the callback URL for a waiting `dz login`.
    pub fn callback_path(&self) -> PathBuf {
        self.dir.join("callback.url")
    }

    /// Called by the URL handler: hands the callback to the waiting `dz login`.
    pub fn deliver_callback(&self, url: &str) -> Result<()> {
        if !url.starts_with("devnull://") || url.len() > 4096 {
            bail!("not a devnull:// callback");
        }
        fs::create_dir_all(&self.dir).context("could not create the dz config directory")?;
        let path = self.callback_path();
        fs::write(&path, url).context("could not write the callback")?;
        restrict(&path)
    }

    /// Takes a delivered callback, if any.
    pub fn take_callback(&self) -> Result<Option<String>> {
        let path = self.callback_path();
        match fs::read_to_string(&path) {
            Ok(url) => {
                let _ = fs::remove_file(&path);
                Ok(Some(url))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).context("could not read the callback"),
        }
    }

    /// Registers `dz callback` as the system handler for `devnull://` links, so the browser
    /// returns the approval to the terminal. Returns what was installed, or `None` where the
    /// platform is unsupported.
    pub fn install_url_handler(&self) -> Result<Option<String>> {
        let dz = std::env::current_exe().context("could not locate the dz binary")?;
        fs::create_dir_all(&self.dir).context("could not create the dz config directory")?;
        handler::install(&self.dir, &dz)
    }
}

/// Platform glue for the `devnull://` URL scheme.
mod handler {
    use std::path::Path;

    use anyhow::Result;

    /// Builds an AppleScript applet in `~/Applications` that claims `devnull://` and runs
    /// `dz callback`. LaunchServices refuses URL claims from temporary directories, and the
    /// bundle must be re-signed after its Info.plist changes.
    #[cfg(target_os = "macos")]
    pub fn install(dir: &Path, dz: &Path) -> Result<Option<String>> {
        use anyhow::{Context, bail};
        use std::{fs, process::Command};

        let applications =
            std::path::PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?)
                .join("Applications");
        fs::create_dir_all(&applications)?;
        let app = applications.join("dz Callback.app");
        let script = dir.join("callback.applescript");
        fs::write(&script, applescript(dz, dir))?;
        let run = |program: &str, args: &[&std::ffi::OsStr]| -> Result<()> {
            let output = Command::new(program)
                .args(args)
                .output()
                .with_context(|| format!("could not run {program}"))?;
            if !output.status.success() {
                bail!(
                    "{program} failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }
            Ok(())
        };
        run(
            "osacompile",
            &["-o".as_ref(), app.as_os_str(), script.as_os_str()],
        )?;
        let plist = app.join("Contents/Info.plist");
        let bundle_id = "sh.dev-null.dz.callback";
        let commands = [
            "Delete :CFBundleIdentifier".to_owned(),
            format!("Add :CFBundleIdentifier string {bundle_id}"),
            "Delete :CFBundleURLTypes".to_owned(),
            "Add :CFBundleURLTypes array".to_owned(),
            "Add :CFBundleURLTypes:0 dict".to_owned(),
            "Add :CFBundleURLTypes:0:CFBundleURLName string dev-null device approval".to_owned(),
            "Add :CFBundleURLTypes:0:CFBundleURLSchemes array".to_owned(),
            format!("Add :CFBundleURLTypes:0:CFBundleURLSchemes:0 string {SCHEME}"),
            "Delete :LSUIElement".to_owned(),
            "Add :LSUIElement bool true".to_owned(),
        ];
        for command in commands {
            // Deletes of absent keys fail; that is fine.
            let _ = Command::new("/usr/libexec/PlistBuddy")
                .arg("-c")
                .arg(&command)
                .arg(&plist)
                .output();
        }
        run(
            "codesign",
            &[
                "--force".as_ref(),
                "--sign".as_ref(),
                "-".as_ref(),
                app.as_os_str(),
            ],
        )?;
        run(LSREGISTER, &["-f".as_ref(), app.as_os_str()])?;
        Ok(Some(app.display().to_string()))
    }

    #[cfg(target_os = "macos")]
    const LSREGISTER: &str = "/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister";

    #[cfg(all(unix, not(target_os = "macos")))]
    pub fn install(_dir: &Path, dz: &Path) -> Result<Option<String>> {
        use anyhow::Context;
        use std::{fs, process::Command};

        let applications = std::env::var_os("XDG_DATA_HOME")
            .filter(|v| !v.is_empty())
            .map(std::path::PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".local/share"))
            })
            .context("HOME is not set")?
            .join("applications");
        fs::create_dir_all(&applications)?;
        let entry = applications.join("dz-callback.desktop");
        fs::write(&entry, desktop_entry(dz, _dir))?;
        let _ = Command::new("update-desktop-database")
            .arg(&applications)
            .status();
        let status = Command::new("xdg-mime")
            .args([
                "default",
                "dz-callback.desktop",
                &format!("x-scheme-handler/{SCHEME}"),
            ])
            .status()
            .context("could not run xdg-mime; install xdg-utils")?;
        if !status.success() {
            anyhow::bail!("xdg-mime could not register the devnull:// handler");
        }
        Ok(Some(entry.display().to_string()))
    }

    #[cfg(not(unix))]
    pub fn install(_dir: &Path, _dz: &Path) -> Result<Option<String>> {
        Ok(None)
    }

    pub const SCHEME: &str = "devnull";

    /// Quotes a path for AppleScript and Desktop Entry strings (both escape `\` and `"`).
    fn quote(path: &Path) -> String {
        format!(
            "\"{}\"",
            path.display()
                .to_string()
                .replace('\\', "\\\\")
                .replace('"', "\\\"")
        )
    }

    /// An applet that forwards the opened URL to `dz callback`. The handler runs outside the
    /// login shell, so the config dir travels with it.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn applescript(dz: &Path, dir: &Path) -> String {
        format!(
            "on open location theURL\n\tdo shell script \"DZ_CONFIG_DIR=\" & quoted form of {dir} & \" \" & quoted form of {dz} & \" callback \" & quoted form of theURL\nend open location\n",
            dir = quote(dir),
            dz = quote(dz),
        )
    }

    #[cfg_attr(not(all(unix, not(target_os = "macos"))), allow(dead_code))]
    pub fn desktop_entry(dz: &Path, dir: &Path) -> String {
        format!(
            "[Desktop Entry]\nType=Application\nName=dz callback\nNoDisplay=true\nExec=env DZ_CONFIG_DIR={} {} callback %u\nMimeType=x-scheme-handler/{SCHEME};\n",
            quote(dir),
            quote(dz)
        )
    }
}

#[cfg(unix)]
fn restrict(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = if path.is_dir() { 0o700 } else { 0o600 };
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .with_context(|| format!("could not restrict permissions on {}", path.display()))
}

#[cfg(not(unix))]
fn restrict(_path: &Path) -> Result<()> {
    Ok(())
}

fn hostname() -> Option<String> {
    let output = Command::new("hostname").arg("-s").output().ok()?;
    let name = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (!name.is_empty()).then_some(name)
}

/// PKCE material for one sign-in attempt (RFC 7636, S256).
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
    pub state: String,
}

impl Pkce {
    pub fn generate() -> Result<Self> {
        // 48 random bytes → 64 unreserved characters, inside the 43–128 range the server accepts.
        let mut random = [0_u8; 48];
        fs::File::open("/dev/urandom")
            .context("could not open the system random source")?
            .read_exact(&mut random)
            .context("could not generate the PKCE verifier")?;
        let verifier = URL_SAFE_NO_PAD.encode(random);
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        Ok(Self {
            verifier,
            challenge,
            state: sign::nonce()?,
        })
    }
}

/// The authorization code from what the user pastes: the `devnull://auth/callback?code=…` URL or the bare code.
pub fn parse_callback(input: &str, expected_state: &str) -> Result<String> {
    let input = input.trim();
    if input.is_empty() {
        bail!("nothing was pasted");
    }
    if !input.contains("://") && !input.contains('?') {
        return Ok(input.to_owned());
    }
    let query = input.split_once('?').map(|(_, q)| q).unwrap_or(input);
    let parsed = Url::parse(&format!("http://callback/?{query}"))
        .context("could not parse the pasted address")?;
    let mut code = None;
    let mut state = None;
    for (key, value) in parsed.query_pairs() {
        match key.as_ref() {
            "code" => code = Some(value.into_owned()),
            "state" => state = Some(value.into_owned()),
            "error" => bail!("the browser reported {value}; approval was cancelled"),
            _ => {}
        }
    }
    if let Some(state) = state
        && state != expected_state
    {
        bail!("the state does not match this sign-in attempt; start again");
    }
    code.context("no code in the pasted address")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_is_s256_of_verifier() {
        let pkce = Pkce::generate().unwrap();
        assert!((43..=128).contains(&pkce.verifier.len()));
        assert!(
            pkce.verifier
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-._~".contains(&c))
        );
        assert_eq!(
            pkce.challenge,
            URL_SAFE_NO_PAD.encode(Sha256::digest(pkce.verifier.as_bytes()))
        );
    }

    #[test]
    fn callback_accepts_url_or_bare_code_and_checks_state() {
        assert_eq!(parse_callback("  abc123 \n", "s").unwrap(), "abc123");
        assert_eq!(
            parse_callback("devnull://auth/callback?code=xyz&state=s", "s").unwrap(),
            "xyz"
        );
        assert!(parse_callback("devnull://auth/callback?code=xyz&state=other", "s").is_err());
        assert!(
            parse_callback("devnull://auth/callback?error=access_denied&state=s", "s").is_err()
        );
        assert!(parse_callback("", "s").is_err());
    }

    #[test]
    fn callbacks_are_delivered_once_and_only_for_devnull_links() {
        let dir = std::env::temp_dir().join(format!("dz-callback-test-{}", std::process::id()));
        let store = Store { dir: dir.clone() };
        assert!(
            store
                .deliver_callback("https://evil.example/?code=x")
                .is_err()
        );
        assert!(store.take_callback().unwrap().is_none());
        store
            .deliver_callback("devnull://auth/callback?code=abc&state=s")
            .unwrap();
        assert_eq!(
            store.take_callback().unwrap().as_deref(),
            Some("devnull://auth/callback?code=abc&state=s")
        );
        assert!(store.take_callback().unwrap().is_none());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn handler_templates_quote_the_binary_path() {
        let dir = Path::new("/Users/me/.config/dz");
        let script = handler::applescript(Path::new("/Applications/my dz/dz"), dir);
        assert!(script.contains("on open location theURL"));
        assert!(script.contains("\"/Applications/my dz/dz\""));
        assert!(script.contains("DZ_CONFIG_DIR="));
        let entry = handler::desktop_entry(Path::new("/usr/local/bin/dz"), dir);
        assert!(entry.contains(
            "Exec=env DZ_CONFIG_DIR=\"/Users/me/.config/dz\" \"/usr/local/bin/dz\" callback %u"
        ));
        assert!(entry.contains("x-scheme-handler/devnull;"));
    }

    #[test]
    fn store_round_trips_sessions_in_an_isolated_dir() {
        let dir = std::env::temp_dir().join(format!("dz-session-test-{}", std::process::id()));
        let store = Store { dir: dir.clone() };
        assert!(store.session().unwrap().is_none());
        let session = Session {
            username: "alice".into(),
            base_url: "https://dev-null.sh".into(),
        };
        store.save(&session).unwrap();
        assert_eq!(store.session().unwrap(), Some(session));
        let base = Url::parse("https://dev-null.sh/").unwrap();
        // No key yet: a saved session without its key is an error, not a silent guest.
        assert!(store.signer(&base).is_err());
        assert!(
            store
                .signer(&Url::parse("https://example.com/").unwrap())
                .unwrap()
                .is_none()
        );
        assert!(store.clear().unwrap());
        assert!(store.session().unwrap().is_none());
        let _ = fs::remove_dir_all(dir);
    }
}
