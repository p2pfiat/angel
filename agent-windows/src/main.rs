//! Silent session agent for Windows.
//!
//! On logon and on every unlock it publishes a `session_start`; the secret hotkey publishes
//! `session_ok`. Nothing is ever shown on screen.

#![cfg_attr(windows, windows_subsystem = "windows")]

mod config;
mod hotkey;
mod location;
mod publisher;
mod state;
#[cfg(windows)]
mod win;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context as _, Result};
use nostr::prelude::*;
use warn_proto::{new_session_id, Envelope, Message};

use config::Config;

const USAGE: &str = "usage: warn-agent [--home DIR] <init [--device NAME] | run | test [--ok|--duress] | test-alert NAME | invite NAME | remove-vip NAME | show>\n\
                     DIR defaults to %LOCALAPPDATA%\\warn";

/// Pause before restarting a crashed agent; doubles on each quick crash, up to `RESTART_MAX`.
const RESTART_MIN: Duration = Duration::from_secs(2);
const RESTART_MAX: Duration = Duration::from_secs(300);
/// An agent that ran this long counts as healthy: the pause starts small again.
const HEALTHY_RUN: Duration = Duration::from_secs(300);

fn default_home() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("warn")
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let is_run = args.iter().any(|a| a == "run" || a == "agent");
    #[cfg(windows)]
    if !is_run {
        win::attach_console_if_needed();
    }
    if let Err(e) = real_main(&mut args, is_run) {
        eprintln!("error: {e:#}");
        tracing::error!("fatal: {e:#}");
        std::process::exit(1);
    }
}

fn real_main(args: &mut Vec<String>, is_run: bool) -> Result<()> {
    let mut home = default_home();
    if args.first().map(String::as_str) == Some("--home") {
        if args.len() < 2 {
            bail!(USAGE);
        }
        home = PathBuf::from(args.remove(1));
        args.remove(0);
    }
    init_logging(&home, is_run);

    match args.first().map(String::as_str) {
        Some("init") => {
            let device = match args.get(1).map(String::as_str) {
                Some("--device") => args.get(2).context(USAGE)?.clone(),
                _ => "laptop".to_string(),
            };
            init(&home, &device)
        }
        Some("show") => show(&home),
        Some("test") => test(&home, args.get(1).map(String::as_str)),
        Some("test-alert") => test_alert(&home, args.get(1).context(USAGE)?),
        Some("invite") => invite(&home, args.get(1).context(USAGE)?),
        Some("remove-vip") => {
            let name = args.get(1).context(USAGE)?;
            warn_watcher::vips::remove_from(&home, name)?;
            println!("{name} is no longer watched once the agent restarts.");
            Ok(())
        }
        Some("run") => supervise(&home),
        // Internal: the agent itself, started (and restarted) by `run`.
        Some("agent") => run(&home, args.get(1).map(String::as_str) == Some("--restarted")),
        // Internal: one position lookup, printed as JSON (see location.rs).
        Some("locate") => {
            println!("{}", serde_json::to_string(&location::measure_now())?);
            Ok(())
        }
        _ => bail!(USAGE),
    }
}

fn init_logging(home: &std::path::Path, to_file: bool) {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "warn_agent=info,warn_watcher=info,warn".into());
    let builder = tracing_subscriber::fmt().with_env_filter(filter).with_ansi(false);
    if to_file {
        let _ = std::fs::create_dir_all(home);
        if let Ok(file) = std::fs::OpenOptions::new().create(true).append(true).open(home.join("agent.log")) {
            builder.with_writer(std::sync::Mutex::new(file)).init();
            return;
        }
    }
    builder.with_writer(std::io::stderr).init();
}

fn init(home: &std::path::Path, device: &str) -> Result<()> {
    if Config::path(home).exists() {
        bail!("{} already exists; edit it or remove the folder to start over", Config::path(home).display());
    }
    std::fs::create_dir_all(home)?;
    let keys = config::load_or_create_key(home)?;
    let cfg = Config::new_default(device);
    std::fs::write(Config::path(home), toml::to_string_pretty(&cfg)?)?;
    println!("Created {}", home.display());
    println!();
    println!("Device key (give this to your watchers):");
    println!("  {}", keys.public_key().to_bech32()?);
    println!();
    println!("Secret check-in hotkey (memorize it, tell nobody):");
    println!("  {}", cfg.ok_hotkey);
    println!();
    println!("Next: add a [watcher] section and/or other watchers' keys to `watchers` in");
    println!("  {}", Config::path(home).display());
    println!("then run `warn-agent test` to check the whole chain.");
    Ok(())
}

fn show(home: &std::path::Path) -> Result<()> {
    let cfg = Config::load(home)?;
    let keys = config::load_or_create_key(home)?;
    println!("device key:     {}", keys.public_key().to_bech32()?);
    println!("check-in key:   {}", cfg.ok_hotkey);
    println!("duress key:     {}", cfg.duress_hotkey.as_deref().unwrap_or("(none)"));
    println!("settings key:   {}", cfg.settings_hotkey.as_deref().unwrap_or("(none)"));
    println!("timeout:        {} s", cfg.timeout_seconds);
    let c = cfg.check_in();
    let mut ways = vec![];
    if c.sound {
        ways.push("sound");
    }
    if c.notice {
        ways.push("notice");
    }
    if c.telegram {
        ways.push("telegram");
    }
    match c.reminder_on() {
        true => println!("reminder:       after {} s ({})", c.reminder_seconds, ways.join(", ")),
        false => println!("reminder:       off"),
    }
    println!("watchers:       {}", cfg.watchers.len());
    match &cfg.watcher {
        Some(w) => {
            let names: Vec<&str> = w.persons.iter().map(|p| p.name.as_str()).collect();
            println!("built-in watcher: on, watching {}", names.join(", "));
        }
        None => println!("built-in watcher: off"),
    }
    Ok(())
}

/// Send the test text to one person's contacts via the built-in watcher's channels.
fn test_alert(home: &std::path::Path, name: &str) -> Result<()> {
    let cfg = Config::load(home)?;
    let w = cfg.local_watcher(home).context("no [watcher] section in the config")?;
    tokio::runtime::Runtime::new()?.block_on(warn_watcher::test_alert(&w, name))
}

/// Find the owner's join link for the settings window; retried until online.
async fn fetch_invite_link(w: warn_watcher::Config, name: String) {
    loop {
        match warn_watcher::invite_link(&w, &name).await {
            Ok(link) => {
                let _ = win::INVITE_LINK.set(link);
                return;
            }
            Err(e) => tracing::debug!("invite link not available yet: {e:#}"),
        }
        tokio::time::sleep(Duration::from_secs(60)).await;
    }
}

/// Print the join link a new contact taps to receive this person's alarms.
fn invite(home: &std::path::Path, name: &str) -> Result<()> {
    let cfg = Config::load(home)?;
    let w = cfg.local_watcher(home).context("no [watcher] section in the config")?;
    println!("{}", tokio::runtime::Runtime::new()?.block_on(warn_watcher::invite_link(&w, name))?);
    Ok(())
}

/// Send a test session_start (and optionally its OK, or a duress) and report delivery.
fn test(home: &std::path::Path, flag: Option<&str>) -> Result<()> {
    let cfg = Config::load(home)?;
    let keys = config::load_or_create_key(home)?;
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let mut sender = publisher::Sender::connect(&cfg, keys, None).await?;
        if cfg.detect_location {
            sender = sender.with_location(home);
        }
        let t = std::time::Instant::now();
        if flag == Some("--duress") {
            sender
                .deliver(&Envelope::new(Message::Duress { context: sender.context(&cfg, "test").await }))
                .await?;
            println!("duress sent in {} ms. Watchers should alert immediately.", t.elapsed().as_millis());
        } else {
            let session = new_session_id();
            sender
                .deliver(&Envelope::new(Message::SessionStart {
                    session: session.clone(),
                    timeout_secs: cfg.timeout_seconds,
                    context: sender.context(&cfg, "test").await,
                }))
                .await?;
            println!("session_start sent in {} ms.", t.elapsed().as_millis());
            if flag == Some("--ok") {
                let t = std::time::Instant::now();
                sender.deliver(&Envelope::new(Message::SessionOk { session })).await?;
                println!("session_ok sent in {} ms. Watchers should stay quiet.", t.elapsed().as_millis());
            } else {
                println!("No OK will be sent: watchers should alert in about {} s.", cfg.timeout_seconds);
            }
        }
        // Give the remaining relays a moment to receive their copy too.
        tokio::time::sleep(Duration::from_secs(3)).await;
        Ok(())
    })
}

/// Keep the agent running: start it as a child process and start it again if it stops
/// abnormally (a crash must not leave the laptop unwatched).
#[cfg(windows)]
fn supervise(home: &std::path::Path) -> Result<()> {
    let cfg = Config::load(home)?;
    if !win::single_instance(&format!("{}-guard", cfg.instance_id)) {
        tracing::info!("already running");
        return Ok(());
    }
    let exe = std::env::current_exe()?;
    let mut pause = RESTART_MIN;
    let mut restarted = false;
    loop {
        let started = std::time::Instant::now();
        let mut cmd = std::process::Command::new(&exe);
        cmd.arg("--home").arg(home).arg("agent");
        if restarted {
            cmd.arg("--restarted");
        }
        match cmd.status() {
            Ok(s) if s.success() => {
                tracing::info!("agent stopped");
                return Ok(());
            }
            Ok(s) => tracing::error!(
                "agent stopped unexpectedly (exit code {:#x}); starting it again in {} s",
                s.code().unwrap_or(-1) as u32,
                pause.as_secs()
            ),
            Err(e) => tracing::error!("agent could not be started: {e}; trying again in {} s", pause.as_secs()),
        }
        if started.elapsed() > HEALTHY_RUN {
            pause = RESTART_MIN;
        }
        std::thread::sleep(pause);
        pause = (pause * 2).min(RESTART_MAX);
        restarted = true;
    }
}

#[cfg(not(windows))]
fn supervise(_home: &std::path::Path) -> Result<()> {
    bail!("`run` is only available on Windows")
}

#[cfg(windows)]
fn run(home: &std::path::Path, restarted: bool) -> Result<()> {
    let cfg = Config::load(home)?;
    if !win::single_instance(&cfg.instance_id) {
        tracing::info!("already running");
        return Ok(());
    }
    let keys = config::load_or_create_key(home)?;
    let ok = hotkey::Hotkey::parse(&cfg.ok_hotkey)?;
    let duress = cfg.duress_hotkey.as_deref().map(hotkey::Hotkey::parse).transpose()?;
    let settings = cfg.settings_hotkey.as_deref().map(hotkey::Hotkey::parse).transpose()?;
    let class_name = format!("W{}", cfg.instance_id);

    let rt = tokio::runtime::Runtime::new()?;
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    // Built-in watcher: same identity as the device, so one key per installation.
    if let Some(w) = cfg.local_watcher(home) {
        rt.spawn(supervise_watcher(w, home.to_path_buf(), keys.clone()));
    }
    // The settings window may rename this device's owner.
    let own_key = keys.public_key();
    let name_target = cfg.own_name(&own_key).map(|_| (home.to_path_buf(), own_key));
    let mut sender = rt.block_on(publisher::Sender::connect(&cfg, keys, None))?;
    if cfg.detect_location {
        sender = sender.with_location(home);
    }
    if !restarted {
        tracing::info!("agent started");
        // We are started at logon: that counts as the first session.
        tx.send(publisher::Trigger::Logon).ok();
    } else if win::session_locked() == Some(true) {
        // Nobody can be at the keyboard: the next unlock starts a session as usual.
        tracing::warn!("agent started again after a crash (screen locked)");
    } else {
        // Someone may be using the laptop right now: ask for a check-in.
        tracing::warn!("agent started again after a crash: check-in required");
        tx.send(publisher::Trigger::Restart).ok();
    }
    state::set_check_in(cfg.check_in());
    let window = win::Setup {
        home: home.to_path_buf(),
        name_target,
        ok_feedback: cfg.ok_feedback,
        has_bot: cfg.bot_token().is_some(),
    };
    if let (Some(w), Some(name)) = (cfg.local_watcher(home), cfg.own_name(&own_key)) {
        if w.telegram.is_some() {
            rt.spawn(fetch_invite_link(w, name));
        }
    }
    rt.spawn(publisher::run(cfg, sender, rx));
    win::message_loop(&class_name, ok, duress, settings, window, tx)
}

/// Run the built-in watcher; restart it with the config on disk when the settings change.
/// Its state (alarms sent, check-ins seen) is saved in the folder, so a restart repeats
/// no alarm.
#[cfg(windows)]
async fn supervise_watcher(mut w: warn_watcher::Config, home: PathBuf, keys: Keys) {
    loop {
        let mut task = tokio::spawn(warn_watcher::run(w.clone(), keys.clone()));
        loop {
            tokio::select! {
                r = &mut task => {
                    match r {
                        Ok(Ok(())) => tracing::info!("built-in watcher stopped"),
                        Ok(Err(e)) => tracing::error!("built-in watcher stopped: {e:#}"),
                        Err(e) => tracing::error!("built-in watcher crashed: {e}"),
                    }
                    return;
                }
                _ = state::watcher_reload_requested() => match Config::load(&home) {
                    Ok(cfg) => match cfg.local_watcher(&home) {
                        Some(new) => {
                            w = new;
                            break;
                        }
                        None => tracing::warn!("[watcher] removed from the config: keeping the running watcher"),
                    },
                    // Keep watching with the old settings rather than not at all.
                    Err(e) => tracing::error!("config not reloaded: {e:#}"),
                },
            }
        }
        task.abort();
        let _ = task.await;
        tracing::info!("built-in watcher restarted with new settings");
    }
}

#[cfg(not(windows))]
fn run(_home: &std::path::Path, _restarted: bool) -> Result<()> {
    bail!("`run` is only available on Windows")
}
