mod api;
mod auth;
mod cache;
mod par;
mod snapshot;

use std::io::{self, BufRead, IsTerminal};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use reqwest::blocking::Client;

use crate::auth::NeedsLogin;
use crate::snapshot::{Snapshot, State};

/// See what your PlayStation Network friends are doing.
#[derive(Parser)]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Sign in with an NPSSO token (prompted for, or piped on stdin).
    Login,
    /// Show friends' status (the default).
    Status {
        /// Machine-readable output, used by the GNOME extension.
        #[arg(long)]
        json: bool,
        /// Include offline friends.
        #[arg(long, short)]
        all: bool,
    },
    /// Forget the saved sign-in.
    Logout,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let command = cli.command.unwrap_or(Command::Status {
        json: false,
        all: false,
    });
    let json = matches!(command, Command::Status { json: true, .. });

    match run(command) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            let needs_login = err.is::<NeedsLogin>();
            if json {
                let out =
                    serde_json::json!({ "error": format!("{err:#}"), "needs_login": needs_login });
                println!("{out}");
            } else {
                eprintln!("error: {err:#}");
            }
            ExitCode::from(if needs_login { 2 } else { 1 })
        }
    }
}

fn run(command: Command) -> Result<()> {
    let client = http_client()?;
    match command {
        Command::Login => login(&client),
        Command::Logout => {
            if auth::logout()? {
                println!("Signed out.");
            } else {
                println!("Wasn't signed in.");
            }
            Ok(())
        }
        Command::Status { json, all } => {
            let snap = fetch(&client)?;
            if json {
                println!("{}", serde_json::to_string(&snap)?);
            } else {
                print_human(&snap, all);
            }
            Ok(())
        }
    }
}

fn http_client() -> Result<Client> {
    Client::builder()
        // The sign-in step needs to read the redirect instead of following it.
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(20))
        .user_agent(concat!("psn-friends/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("building HTTP client")
}

fn login(client: &Client) -> Result<()> {
    let npsso = if io::stdin().is_terminal() {
        eprintln!("1. Sign in at https://www.playstation.com in your browser.");
        eprintln!("2. In the same browser, open https://ca.account.sony.com/api/v1/ssocookie");
        eprintln!("3. Copy the \"npsso\" value (64 characters) and paste it below.\n");
        rpassword::prompt_password("NPSSO (input hidden): ")?
    } else {
        let mut line = String::new();
        io::stdin().lock().read_line(&mut line)?;
        line
    };
    let npsso = extract_npsso(&npsso);
    anyhow::ensure!(!npsso.is_empty(), "no NPSSO token given");

    auth::login(client, &npsso)?;
    let tokens_path = auth::tokens_path()?;
    eprintln!("Signed in. Tokens saved to {}", tokens_path.display());
    eprintln!("Checking it works...");
    let snap = fetch(client)?;
    print_human(&snap, false);
    Ok(())
}

/// Accept either the bare token or the whole `{"npsso":"..."}` JSON page.
fn extract_npsso(input: &str) -> String {
    let input = input.trim();
    serde_json::from_str::<serde_json::Value>(input)
        .ok()
        .and_then(|v| v.get("npsso")?.as_str().map(str::to_string))
        .unwrap_or_else(|| input.trim_matches('"').to_string())
}

fn fetch(client: &Client) -> Result<Snapshot> {
    let tokens = auth::valid_tokens(client)?;
    let api = api::Api::new(client.clone(), tokens.access_token);
    let ids = api.friend_ids().context("fetching friends list")?;
    let presences = api.presences(&ids).context("fetching presence")?;
    let profiles = cache::profiles(&api, &ids).context("fetching profiles")?;
    let images = cache::images(client, snapshot::image_urls(&ids, &profiles, &presences))?;
    Ok(snapshot::build(&ids, &profiles, &presences, &images))
}

fn print_human(snap: &Snapshot, all: bool) {
    let color = io::stdout().is_terminal();
    let paint = |code: &str, s: &str| {
        if color {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    };

    let active = snap
        .friends
        .iter()
        .filter(|f| f.state != State::Offline)
        .count();
    println!("{active} of {} friends online", snap.friends.len());

    let width = snap
        .friends
        .iter()
        .map(|f| f.online_id.chars().count())
        .max()
        .unwrap_or(0);
    for f in &snap.friends {
        let name = format!("{:width$}", f.online_id);
        let platform = f.platform.as_deref().unwrap_or("");
        match f.state {
            State::Playing => {
                let game = f.game.as_ref().expect("playing implies game");
                let status = game
                    .status
                    .as_deref()
                    .map(|s| format!(" — {s}"))
                    .unwrap_or_default();
                println!(
                    "{} {name}  {} {}{status}",
                    paint("34", "▶"),
                    paint("1", &game.title),
                    paint("2", platform)
                );
            }
            State::Online => println!(
                "{} {name}  {}",
                paint("32", "●"),
                paint("2", &format!("online {platform}"))
            ),
            State::Offline if all => {
                let seen = f
                    .last_online
                    .as_deref()
                    .map(ago)
                    .unwrap_or_else(|| "offline".into());
                println!(
                    "{} {}",
                    paint("2", "○"),
                    paint("2", &format!("{name}  {seen}"))
                );
            }
            State::Offline => {}
        }
    }
    if !all && active < snap.friends.len() {
        println!("{}", paint("2", "(--all to include offline friends)"));
    }
}

fn ago(iso: &str) -> String {
    let Ok(then) = chrono::DateTime::parse_from_rfc3339(iso) else {
        return iso.to_string();
    };
    let secs = (chrono::Utc::now() - then.with_timezone(&chrono::Utc))
        .num_seconds()
        .max(0);
    match secs {
        s if s < 60 => "last seen just now".into(),
        s if s < 3600 => format!("last seen {}m ago", s / 60),
        s if s < 86400 => format!("last seen {}h ago", s / 3600),
        s if s < 86400 * 30 => format!("last seen {}d ago", s / 86400),
        _ => format!("last seen {}", then.format("%-d %b %Y")),
    }
}

#[cfg(test)]
mod tests {
    use super::extract_npsso;

    #[test]
    fn npsso_from_json_or_bare() {
        assert_eq!(extract_npsso("  abc123\n"), "abc123");
        assert_eq!(extract_npsso("{\"npsso\":\"abc123\"}"), "abc123");
        assert_eq!(extract_npsso("\"abc123\""), "abc123");
    }
}
