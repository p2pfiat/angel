//! Standalone watcher (Docker / server).

use std::path::PathBuf;

use anyhow::{bail, Context as _, Result};
use nostr::prelude::*;
use warn_watcher::{invite_link, load_or_create_key, run, test_alert, Config};

const USAGE: &str = "usage: warn-watcher [--config PATH] <run|pubkey|test-alert NAME|invite NAME|remove-vip NAME>\n\
                     config defaults to $WARN_CONFIG or /data/config.toml";

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn_watcher=info,warn".into()),
        )
        .init();

    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut config_path: PathBuf = std::env::var("WARN_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/data/config.toml"));
    if args.first().map(String::as_str) == Some("--config") {
        if args.len() < 2 {
            bail!(USAGE);
        }
        config_path = PathBuf::from(args.remove(1));
        args.remove(0);
    }

    let cfg = Config::load(&config_path)?;
    let keys = load_or_create_key(&cfg.data_dir)?;

    match args.first().map(String::as_str) {
        Some("pubkey") => {
            println!("{}", keys.public_key().to_bech32()?);
            Ok(())
        }
        Some("test-alert") => {
            let name = args.get(1).context(USAGE)?;
            test_alert(&cfg, name).await
        }
        Some("invite") => {
            println!("{}", invite_link(&cfg, args.get(1).context(USAGE)?).await?);
            Ok(())
        }
        Some("remove-vip") => {
            let name = args.get(1).context(USAGE)?;
            warn_watcher::vips::remove_from(&cfg.data_dir, name)?;
            println!("{name} is no longer watched once the watcher restarts.");
            Ok(())
        }
        Some("run") | None => run(cfg, keys).await,
        _ => bail!(USAGE),
    }
}
