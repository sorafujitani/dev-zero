mod api;
mod model;
mod output;
mod post;
mod session;
mod sign;
mod tui;

use std::{
    io::{self, BufRead, IsTerminal, Read, Write},
    path::PathBuf,
    process::Command as Process,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use serde_json::Value;

use api::Api;
use model::{CommitId, Cursor, Feed, Log, User, Username};
use session::{Pkce, Session, Store};

#[derive(Parser)]
#[command(
    name = "dz",
    version,
    about = "dev-zero — a CLI and TUI for dev-null.sh"
)]
struct Cli {
    /// Print the unmodified API response as JSON
    #[arg(long, global = true)]
    json: bool,
    /// API server origin
    #[arg(
        long,
        global = true,
        env = "DZ_BASE_URL",
        default_value = "https://dev-null.sh"
    )]
    base_url: String,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Browse interactively: your logs when signed in, else a user's commits (default)
    Tui {
        #[arg(env = "DZ_USERNAME")]
        username: Option<Username>,
        /// GitHub SSH key for posting without a session
        #[arg(long, env = "DZ_SSH_KEY")]
        key: Option<PathBuf>,
    },
    /// Approve this machine as a device, so dz can read your logs and act as you
    Login {
        /// Name shown under Settings → Devices
        #[arg(long)]
        device_name: Option<String>,
        /// Print the approval URL without opening a browser
        #[arg(long)]
        no_browser: bool,
    },
    /// Forget the session and delete the device key (revoke it under Settings → Devices too)
    Logout,
    /// Receives the devnull:// link the browser opens after approval (installed by dz login)
    #[command(hide = true)]
    Callback { url: String },
    /// Show the signed-in profile
    Me,
    /// Post text, a reply, or a quote
    Post(Post),
    /// Delete one of your commits
    Delete {
        id: CommitId,
        /// Skip the confirmation prompt
        #[arg(long, short)]
        yes: bool,
    },
    /// Star / unstar a commit
    Star {
        id: CommitId,
        #[arg(long)]
        undo: bool,
    },
    /// Save / unsave a commit privately
    Save {
        id: CommitId,
        #[arg(long)]
        undo: bool,
    },
    /// Follow / unfollow a user
    Follow {
        username: Username,
        #[arg(long)]
        undo: bool,
    },
    /// Mute / unmute a user
    Mute {
        username: Username,
        #[arg(long)]
        undo: bool,
    },
    /// Block / unblock a user
    Block {
        username: Username,
        #[arg(long)]
        undo: bool,
    },
    /// Report a commit to the moderators
    Report {
        id: CommitId,
        #[arg(value_parser = ["spam", "harassment", "unsafe", "other"])]
        reason: String,
        /// Up to 500 characters
        #[arg(long)]
        details: Option<String>,
    },
    #[command(flatten)]
    Query(Query),
}

#[derive(Args)]
struct Post {
    /// Text to post (3–4000 characters)
    #[arg(required_unless_present = "stdin", conflicts_with = "stdin")]
    message: Option<String>,
    /// Read the message from standard input
    #[arg(long)]
    stdin: bool,
    /// Reply to a commit ID (or, with --key, a hash or URL)
    #[arg(long)]
    reply: Option<String>,
    /// Quote a commit by ID (session only)
    #[arg(long, conflicts_with = "reply")]
    quote: Option<CommitId>,
    /// Sign with a GitHub SSH key through the Action endpoint instead of the session
    #[arg(long, env = "DZ_SSH_KEY")]
    key: Option<PathBuf>,
}

/// Read commands that print one API response.
#[derive(Subcommand)]
enum Query {
    /// Show a public user profile
    User {
        #[arg(env = "DZ_USERNAME")]
        username: Username,
    },
    /// List one page of a user's commits, newest first
    Commits {
        #[arg(env = "DZ_USERNAME")]
        username: Username,
        /// Show this user's replies instead of top-level commits
        #[arg(long)]
        replies: bool,
        #[arg(long)]
        cursor: Option<Cursor>,
    },
    /// Show a commit by its full ID
    Commit { id: CommitId },
    /// List who follows a user
    Followers {
        #[arg(env = "DZ_USERNAME")]
        username: Username,
        #[arg(long)]
        cursor: Option<Cursor>,
    },
    /// List who a user follows
    Following {
        #[arg(env = "DZ_USERNAME")]
        username: Username,
        #[arg(long)]
        cursor: Option<Cursor>,
    },
    /// One page of your Following, Latest or Trending log (session)
    Log {
        #[arg(value_enum, default_value_t = Log::Following)]
        log: Log,
        #[arg(long)]
        cursor: Option<Cursor>,
    },
    /// Replies to a commit, newest first (session)
    Replies {
        id: CommitId,
        #[arg(long)]
        cursor: Option<Cursor>,
    },
    /// Your saved commits (session)
    Saved {
        #[arg(long)]
        cursor: Option<Cursor>,
    },
    /// Your notifications; listing marks them read, as on the web (session)
    Notifications {
        #[arg(long)]
        cursor: Option<Cursor>,
    },
}

fn api(cli: &Cli, store: &Store) -> Result<Api> {
    let api = Api::new(&cli.base_url)?;
    let signer = store.signer(api.base())?;
    Ok(api.with_signer(signer))
}

fn run(mut cli: Cli) -> Result<()> {
    let store = Store::locate()?;
    let command = match cli.command.take() {
        Some(command) => command,
        None => {
            let username = match std::env::var("DZ_USERNAME") {
                Ok(value) => Some(
                    value
                        .parse::<Username>()
                        .map_err(anyhow::Error::msg)
                        .context("invalid DZ_USERNAME")?,
                ),
                Err(std::env::VarError::NotPresent) => None,
                Err(error) => return Err(error).context("invalid DZ_USERNAME"),
            };
            Command::Tui {
                username,
                key: std::env::var_os("DZ_SSH_KEY").map(PathBuf::from),
            }
        }
    };
    let mut out = io::stdout().lock();
    match command {
        Command::Tui { username, key } => {
            if cli.json {
                bail!("--json does not apply to the TUI");
            }
            if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
                bail!("TUI requires a terminal; use dz log, dz commits or dz commit for pipes");
            }
            let api = api(&cli, &store)?;
            let me = store.session()?.map(|session| session.username);
            tui::run(api, username, key, me)
        }
        Command::Login {
            device_name,
            no_browser,
        } => login(&cli, &store, device_name, no_browser),
        Command::Callback { url } => store.deliver_callback(&url),
        Command::Logout => {
            let removed = store.clear()?;
            writeln!(
                out,
                "{}",
                if removed {
                    "Signed out. Revoke the device under Settings → Devices on the web as well."
                } else {
                    "No session to sign out of."
                }
            )?;
            Ok(())
        }
        Command::Me => {
            let response = api(&cli, &store)?.me()?.body;
            print_value(&mut out, cli.json, response, |value| {
                Ok(output::user(&api::data(value)?))
            })
        }
        Command::Post(post) => post_command(&cli, &store, post),
        Command::Delete { id, yes } => {
            let api = api(&cli, &store)?;
            if !yes && !confirm(&format!("Delete commit {}? [y/N] ", id.as_str()))? {
                bail!("cancelled");
            }
            api.delete_commit(&id)?;
            writeln!(out, "Deleted {}.", id.as_str())?;
            Ok(())
        }
        Command::Star { id, undo } => {
            let response = api(&cli, &store)?.set_star(&id, !undo)?.body;
            print_value(&mut out, cli.json, response, |_| {
                Ok(format!(
                    "{} {}.",
                    if undo { "Unstarred" } else { "Starred" },
                    id.as_str()
                ))
            })
        }
        Command::Save { id, undo } => {
            let response = api(&cli, &store)?.set_saved(&id, !undo)?.body;
            print_value(&mut out, cli.json, response, |_| {
                Ok(format!(
                    "{} {}.",
                    if undo { "Unsaved" } else { "Saved" },
                    id.as_str()
                ))
            })
        }
        Command::Follow { username, undo } => {
            let response = api(&cli, &store)?.set_follow(&username, !undo)?.body;
            print_value(&mut out, cli.json, response, |_| {
                Ok(format!(
                    "{} @{username}.",
                    if undo { "Unfollowed" } else { "Following" }
                ))
            })
        }
        Command::Mute { username, undo } => {
            let response = api(&cli, &store)?.set_mute(&username, !undo)?.body;
            print_value(&mut out, cli.json, response, |_| {
                Ok(format!(
                    "{} @{username}.",
                    if undo { "Unmuted" } else { "Muted" }
                ))
            })
        }
        Command::Block { username, undo } => {
            let response = api(&cli, &store)?.set_block(&username, !undo)?.body;
            print_value(&mut out, cli.json, response, |_| {
                Ok(format!(
                    "{} @{username}.",
                    if undo { "Unblocked" } else { "Blocked" }
                ))
            })
        }
        Command::Report {
            id,
            reason,
            details,
        } => {
            if details.as_ref().is_some_and(|d| d.chars().count() > 500) {
                bail!("--details must be at most 500 characters");
            }
            api(&cli, &store)?.report(&id, &reason, details.as_deref())?;
            writeln!(out, "Reported {}.", id.as_str())?;
            Ok(())
        }
        Command::Query(query) => {
            let text = render(&api(&cli, &store)?, query, cli.json)?;
            writeln!(out, "{text}")?;
            Ok(())
        }
    }
}

fn print_value(
    out: &mut impl Write,
    json: bool,
    value: Value,
    text: impl FnOnce(Value) -> Result<String>,
) -> Result<()> {
    let line = if json {
        serde_json::to_string_pretty(&value)?
    } else {
        text(value)?
    };
    writeln!(out, "{line}")?;
    Ok(())
}

fn confirm(prompt: &str) -> Result<bool> {
    if !io::stdin().is_terminal() {
        bail!("confirmation needs a terminal; pass --yes");
    }
    eprint!("{prompt}");
    let mut answer = String::new();
    io::stdin().lock().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes"))
}

fn post_command(cli: &Cli, store: &Store, post: Post) -> Result<()> {
    if cli.json && post.key.is_some() {
        bail!("--json does not apply to --key posting");
    }
    let message = if post.stdin {
        let mut text = String::new();
        io::stdin()
            .take(16_001)
            .read_to_string(&mut text)
            .context("could not read UTF-8 message from stdin")?;
        if text.len() > 16_000 {
            bail!("message exceeds {} characters", post::MAX_CHARS);
        }
        text
    } else {
        post.message.context("provide a message or --stdin")?
    };
    let api = api(cli, store)?;
    let mut out = io::stdout().lock();
    if let Some(key) = &post.key
        && !api.is_signed()
    {
        if post.quote.is_some() {
            bail!("--quote needs a session; run dz login");
        }
        let text = post::sign(&message, post.reply.as_deref(), key)?.send(&api)?;
        writeln!(out, "{}", output::safe(text.trim_end()))?;
        return Ok(());
    }
    if !api.is_signed() {
        bail!("posting needs a session (dz login) or --key / DZ_SSH_KEY");
    }
    let body = post::validate(&message)?;
    let parent = post
        .reply
        .as_deref()
        .map(|reply| {
            reply.parse::<CommitId>().map_err(|e| {
                anyhow::anyhow!("--reply must be a commit ID when posting with a session: {e}")
            })
        })
        .transpose()?;
    let response = api
        .create_commit(
            body,
            parent.as_ref().map(CommitId::as_str),
            post.quote.as_ref().map(CommitId::as_str),
        )?
        .body;
    print_value(&mut out, cli.json, response, |value| {
        let commit: model::Commit = api::data(value)?;
        Ok(format!(
            "Posted {}{}\n{}",
            commit.short_hash,
            commit
                .status
                .as_deref()
                .filter(|s| *s != "approved")
                .map(|s| format!(" ({s})"))
                .unwrap_or_default(),
            commit.html_url
        ))
    })
}

fn login(cli: &Cli, store: &Store, device_name: Option<String>, no_browser: bool) -> Result<()> {
    if !io::stdin().is_terminal() {
        bail!("dz login is interactive; run it in a terminal");
    }
    let api = Api::new(&cli.base_url)?;
    if let Some(session) = store.session()?
        && session.base_url.trim_end_matches('/') == api.base().as_str().trim_end_matches('/')
    {
        eprintln!(
            "Already signed in as @{}; run dz logout first to switch accounts.",
            session.username
        );
        return Ok(());
    }
    let signer = store.ensure_key()?;
    let fingerprint = store.fingerprint()?;
    let pkce = Pkce::generate()?;
    let device_name = device_name.unwrap_or_else(|| {
        let host = Process::new("hostname")
            .arg("-s")
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
            .filter(|h| !h.is_empty())
            .unwrap_or_else(|| "this machine".into());
        format!("dz on {host}")
    });
    let mut url = api.base().join("mobile/authorize")?;
    url.query_pairs_mut()
        .append_pair("fingerprint", &fingerprint)
        .append_pair("device_name", &device_name)
        .append_pair("code_challenge", &pkce.challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &pkce.state)
        .append_pair("redirect_uri", "devnull://auth/callback");
    // The site answers approval with a devnull:// link; register dz to receive it.
    let _ = store.take_callback();
    let handler = match store.install_url_handler() {
        Ok(Some(path)) => {
            eprintln!("Registered {path} to receive the approval.");
            true
        }
        Ok(None) => false,
        Err(error) => {
            eprintln!("Could not register a devnull:// handler ({error:#}).");
            false
        }
    };
    eprintln!("\nApprove this device in your browser:\n\n  {url}\n");
    if !no_browser {
        open_browser(url.as_str());
    }
    if handler {
        eprintln!(
            "After Approve, allow the browser to open \"dz Callback\" and come back here.\n\
             If nothing happens, paste the devnull:// address the browser shows instead."
        );
    } else {
        eprintln!(
            "After Approve, the browser is sent to a devnull://auth/callback address it cannot\n\
             open. Copy that address (or just the code= value) and paste it here."
        );
    }
    eprint!("Waiting… ");
    let input = wait_for_callback(store, Duration::from_secs(300))?;
    eprintln!();
    let code = session::parse_callback(&input, &pkce.state)?;
    let user: User = api::data(api.register_device(&signer, &code, &pkce.verifier)?.body)?;
    store.save(&Session {
        username: user.username.clone(),
        base_url: api.base().as_str().trim_end_matches('/').to_owned(),
    })?;
    eprintln!(
        "Signed in as @{}. Session stored in {}.",
        user.username,
        store.dir().display()
    );
    Ok(())
}

/// The callback from the URL handler, or a line pasted on the terminal, whichever comes first.
fn wait_for_callback(store: &Store, timeout: Duration) -> Result<String> {
    let (pasted, lines) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        if io::stdin().lock().read_line(&mut line).is_ok() {
            let _ = pasted.send(line);
        }
    });
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Some(url) = store.take_callback()? {
            return Ok(url);
        }
        match lines.recv_timeout(Duration::from_millis(200)) {
            Ok(line) if !line.trim().is_empty() => return Ok(line),
            Ok(_) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                // stdin closed: keep waiting for the handler only.
                std::thread::sleep(Duration::from_millis(200));
            }
        }
        if std::time::Instant::now() >= deadline {
            bail!("no approval arrived within 5 minutes; run dz login again");
        }
    }
}

fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let opener = "open";
    #[cfg(not(target_os = "macos"))]
    let opener = "xdg-open";
    let _ = Process::new(opener)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

fn render(api: &Api, query: Query, json: bool) -> Result<String> {
    let response = match &query {
        Query::User { username } => api.user(username)?,
        Query::Commits {
            username,
            replies,
            cursor,
        } => {
            let feed = if *replies {
                Feed::Replies
            } else {
                Feed::Commits
            };
            api.commits(username, feed, cursor.as_ref())?
        }
        Query::Commit { id } => api.commit(id)?,
        Query::Followers { username, cursor } => api.followers(username, cursor.as_ref())?,
        Query::Following { username, cursor } => api.following(username, cursor.as_ref())?,
        Query::Log { log, cursor } => api.log(*log, cursor.as_ref())?,
        Query::Replies { id, cursor } => api.replies(id, cursor.as_ref())?,
        Query::Saved { cursor } => api.saved(cursor.as_ref())?,
        Query::Notifications { cursor } => api.notifications(cursor.as_ref())?,
    }
    .body;
    if json {
        return Ok(serde_json::to_string_pretty(&response)?);
    }
    Ok(match query {
        Query::User { .. } => output::user(&api::data(response)?),
        Query::Commit { .. } => output::commit(&api::data(response)?),
        Query::Commits { .. } | Query::Log { .. } | Query::Replies { .. } | Query::Saved { .. } => {
            output::page(&api::page(response)?)
        }
        Query::Followers { .. } => output::users(&api::users_page(response)?, "followers"),
        Query::Following { .. } => output::users(&api::users_page(response)?, "followed users"),
        Query::Notifications { .. } => output::notifications(&api::notifications_page(response)?),
    })
}

fn main() {
    if let Err(error) = run(Cli::parse()) {
        if error
            .downcast_ref::<io::Error>()
            .is_some_and(|e| e.kind() == io::ErrorKind::BrokenPipe)
        {
            return;
        }
        eprintln!("dz: {}", output::safe(&format!("{error:#}")));
        std::process::exit(1);
    }
}
