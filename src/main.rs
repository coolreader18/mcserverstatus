use std::fs::File;
use std::future::Future;
use std::io::{self, BufReader};
use std::path::PathBuf;
use std::process::{ExitCode, Termination};
use std::time::Duration;

use anyhow::Context;
use clap::{AppSettings, ArgGroup, Parser};
use dialoguer::{theme::ColorfulTheme, Select};
use itertools::Itertools;
use serde::Deserialize;

#[derive(Deserialize)]
struct ServersDat {
    servers: Vec<Server>,
}

#[derive(Deserialize)]
struct Server {
    // #[serde_as(as = "Base64")]
    // icon: Vec<u8>
    ip: String,
    name: String,
}

impl ToString for Server {
    fn to_string(&self) -> String {
        format!("{} (ip: {})", self.name, self.ip)
    }
}

#[derive(Parser)]
#[clap(author, version, about, long_about = None)]
#[clap(setting(AppSettings::DeriveDisplayOrder))]
#[clap(group(
    ArgGroup::new("server-choice").args(&["instance", "server", "servers-file"])
))]
struct Args {
    /// Path to the folder for your minecraft instance [default: the standard .minecraft folder]
    #[clap(short, long, parse(from_os_str))]
    instance: Option<PathBuf>,

    /// IP for the minecraft server to query
    #[clap(short, long)]
    server: Option<String>,

    /// Path to the servers.dat file you want to choose a server from.
    #[clap(short = 'f', long, parse(from_os_str))]
    servers_file: Option<PathBuf>,

    /// Connection timeout in seconds
    #[clap(long, short, default_value = "2.0")]
    timeout: f64,
}

fn get_minecraft_dir() -> anyhow::Result<PathBuf> {
    let mc_dir = if cfg!(any(windows, target_os = "macos")) {
        dirs_next::data_dir()
    } else {
        dirs_next::home_dir()
    };
    let mc_dir = mc_dir.and_then(|mut mc_dir| {
        mc_dir.push(if cfg!(target_os = "macos") {
            "minecraft"
        } else {
            ".minecraft"
        });
        mc_dir.is_dir().then(|| mc_dir)
    });
    mc_dir.context(
        "Couldn't resolve .minecraft directory, please check \
         that it exists or pass the path explicitly.",
    )
}

#[tokio::main]
async fn main() -> ExitCode {
    let term = console::Term::stderr();
    tokio::select! {
        res = tokio::signal::ctrl_c() => res.unwrap(),
        res = app(&term) => {
            match res {
                Err(e) if e.is::<CtrlC>() => {}
                res => return res.report(),
            }
        }
    }
    // ctrl-C, reset term and exit with generic error code
    let _ = term.show_cursor();
    ExitCode::FAILURE
}

#[derive(Debug, thiserror::Error)]
#[error("ctrl-c")]
struct CtrlC;

async fn app(term: &console::Term) -> anyhow::Result<()> {
    let args = Args::parse();

    let timeout = Duration::from_secs_f64(args.timeout);

    let server_str = if let Some(server) = args.server {
        server
    } else {
        let term = term.clone();
        tokio::task::spawn_blocking(move || {
            let servers_path = if let Some(path) = args.servers_file {
                path
            } else {
                let mut path = match args.instance {
                    Some(x) => x,
                    None => get_minecraft_dir()?,
                };
                path.push("servers.dat");
                path
            };

            let dat: ServersDat = nbt::from_reader(BufReader::new(File::open(servers_path)?))?;

            let theme = ColorfulTheme::default();
            let selection = Select::with_theme(&theme)
                .with_prompt("Which server?")
                .items(&dat.servers)
                .default(0)
                .interact_on(&term);

            let selection = match selection {
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {
                    anyhow::bail!(CtrlC)
                }
                x => x?,
            };

            let choice = dat.servers.into_iter().nth(selection).unwrap().ip;
            anyhow::Ok(choice)
        })
        .await??
    };

    let (ip, port) = match server_str.split_once(':') {
        Some((ip, port)) => {
            let port = port
                .parse::<u16>()
                .context("Could not parse port as integer")?;
            (ip, Some(port))
        }
        None => (&*server_str, None),
    };

    let mut ping_conf = async_minecraft_ping::ConnectionConfig::build(ip).with_timeout(timeout);
    if let Some(port) = port {
        ping_conf = ping_conf.with_port(port);
    }

    let spinner = &indicatif::ProgressBar::new_spinner();
    spinner.set_draw_target(indicatif::ProgressDrawTarget::term(term.clone(), Some(15)));

    let (online, max, players) = spin(&spinner, async move {
        spinner.set_message("Connecting...");
        let conn = ping_conf.connect().await?;
        spinner.set_message("Fetching status...");
        let conn = conn.status().await?;

        let players = &conn.status.players;
        let (online, max) = (players.online, players.max);
        let players = players
            .sample
            .as_deref()
            .filter(|v| !v.is_empty())
            .map(|players| players.iter().map(|player| &*player.name).join(" "));

        spinner.set_message("Pinging...");
        conn.ping(0x8008135).await?;

        anyhow::Ok((online, max, players))
    })
    .await?;

    println!(
        "{online}/{max} online{}",
        if players.is_some() { ":" } else { "" }
    );
    if let Some(players) = players {
        let options = textwrap::Options::new(60)
            .initial_indent("    ")
            .subsequent_indent("    ");
        for line in textwrap::wrap(&players, options) {
            println!("{line}");
        }
    }

    Ok(())
}

async fn spin<T, F: Future<Output = T>>(spinner: &indicatif::ProgressBar, fut: F) -> T {
    let mut int = tokio::time::interval(Duration::from_millis(100));
    tokio::pin!(fut);
    loop {
        tokio::select! {
            res = &mut fut => {
                spinner.finish_and_clear();
                return res;
            }
            _ = int.tick() => spinner.tick(),
        }
    }
}
