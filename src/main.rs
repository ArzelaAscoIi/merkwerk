mod activity;
mod ask;
mod audio;
mod calendar;
mod config;
mod detect;
mod mcp;
mod notes;
mod screen;
mod session;
mod store;
mod summarize;
mod transcribe;
mod ui;

use anyhow::{Context, Result};
use config::Config;
use std::{
    env, io,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use store::Store;

const USAGE: &str = "merkwerk: local meeting recorder

  merkwerk [watch]          menu bar + auto-record calls (default)
  merkwerk record [title]   record now, Enter to stop
  merkwerk list             list meetings
  merkwerk show <id>        print summary and transcript
  merkwerk notes            open the notes window
  merkwerk activity [date]  screen activity of a day (default today)
  merkwerk day [date]       write the work log for a day
  merkwerk ask <id> <q>     ask about a meeting (or <date>_log for a day)
  merkwerk transcribe <id>  re-run whisper on the saved audio
  merkwerk summarize <id>   (re)write transcript.md and summary.md
  merkwerk mcp              MCP server on stdio
  merkwerk model            download the whisper + VAD models
  merkwerk install          install ~/Applications/merkwerk.app (needed to hear the other side of calls)

env: MERKWERK_DIR MERKWERK_MODEL MERKWERK_LANG MERKWERK_LLM_URL
     MERKWERK_LLM_MODEL MERKWERK_LLM_KEY MERKWERK_IGNORE MERKWERK_ME
     MERKWERK_SCREEN MERKWERK_SCREEN_IGNORE";

/// A call is over once nobody else has used the mic for this long.
const CALL_IDLE: Duration = Duration::from_secs(15);

fn main() -> Result<()> {
    let cfg = Config::from_env();
    let store = Store { root: cfg.root.clone() };
    // Finder may pass a process serial number (-psn_...) to apps it launches.
    let args: Vec<String> = env::args().skip(1).filter(|a| !a.starts_with("-psn")).collect();
    let arg = |i: usize| args.get(i).map(String::as_str);
    let id = || arg(1).context("missing <id>");
    match arg(0) {
        None | Some("watch") => watch(&cfg, &store),
        Some("record") => record(&cfg, &store, arg(1).unwrap_or(store::UNTITLED)),
        Some("list") => {
            for m in store.list() {
                println!("{}  {:>5}  {}", m.id, duration(&m), m.title);
            }
            Ok(())
        }
        Some("show") => {
            let id = id()?;
            store.load(id)?;
            println!(
                "{}\n{}",
                store.read(id, "summary.md").unwrap_or_default(),
                store.transcript(id)
            );
            Ok(())
        }
        Some("transcribe") => session::retranscribe(&cfg, &store, id()?),
        Some("summarize") => session::finalize(&cfg, &store, id()?),
        Some("mcp") => mcp::serve(&cfg, &store),
        Some("notes") => ui::run_notes(cfg.clone(), store),
        Some("activity") => {
            print!("{}", activity::digest(&cfg, day(arg(1))?, 200));
            Ok(())
        }
        Some("ask") => {
            let question = args
                .get(2..)
                .map(|q| q.join(" "))
                .filter(|q| !q.is_empty())
                .context("usage: merkwerk ask <id> <question>")?;
            println!("{}", ask::answer(&cfg, &store, id()?, &[], &question)?);
            Ok(())
        }
        Some("day") => {
            println!("{}", activity::write_work_log(&cfg, &store, day(arg(1))?)?);
            Ok(())
        }
        Some("model") => transcribe::ensure_models(&cfg),
        Some("install") => install(),
        _ => {
            eprintln!("{USAGE}");
            Ok(())
        }
    }
}

fn watch(cfg: &Config, store: &Store) -> Result<()> {
    log_to_file(cfg);
    transcribe::ensure_models(cfg)?;
    eprintln!("watching for calls, meetings go to {}", cfg.root.display());
    ui::AUTO.store(config::auto_record(&cfg.root), Ordering::Relaxed);
    ui::SCREEN.store(cfg.screen_enabled(), Ordering::Relaxed);
    let (c, s) = (cfg.clone(), store.clone());
    thread::spawn(move || activity::run(&c, &s));
    let (c, s) = (cfg.clone(), store.clone());
    thread::spawn(move || {
        if let Err(e) = watch_loop(&c, &s) {
            eprintln!("{e:#}");
            std::process::exit(1);
        }
    });
    ui::run(cfg.clone(), store.clone())
}

fn watch_loop(cfg: &Config, store: &Store) -> Result<()> {
    // A call the user stopped or ignored stays ignored until its app releases the mic.
    let mut snoozed: Option<String> = None;
    let mut announced: Option<String> = None;
    loop {
        let users = detect::mic_users(&cfg.ignore);
        if snoozed.as_ref().is_some_and(|s| !users.contains(s)) {
            snoozed = None;
        }
        let mut call = users.into_iter().find(|u| snoozed.as_ref() != Some(u));
        if ui::IGNORE.swap(false, Ordering::Relaxed) {
            snoozed = call.take();
        }
        let start = ui::START.swap(false, Ordering::Relaxed);
        let auto = ui::AUTO.load(Ordering::Relaxed);
        // Manual mode (default): a detected call only asks; recording needs a click.
        let pending = if auto || start { None } else { call.clone() };
        if pending.is_some() && pending != announced {
            eprintln!(
                "call detected: {} (manual mode: click Record to record it)",
                call.as_deref().unwrap_or("")
            );
        }
        announced = pending.clone();
        *ui::PENDING.lock().unwrap() = pending;
        if start || (auto && call.is_some()) {
            // Recording a detected call (clicked or auto) ends with the call; otherwise it runs until stopped.
            let app = call;
            let label = app.clone().unwrap_or_else(|| "manual".into());
            eprintln!("recording: {label}");
            ui::STOP.store(false, Ordering::Relaxed);
            *ui::RECORDING.lock().unwrap() = Some((label, Instant::now()));
            let mut idle_since: Option<Instant> = None;
            let rec = session::record(cfg, store, store::UNTITLED, app.clone(), || {
                if ui::STOP.swap(false, Ordering::Relaxed) {
                    snoozed = app.clone();
                    return true;
                }
                // Manual recordings run until stopped; calls end once their app lets go of the mic.
                let Some(app) = &app else { return false };
                if detect::mic_users(&cfg.ignore).contains(app) {
                    idle_since = None;
                    return false;
                }
                idle_since.get_or_insert_with(Instant::now).elapsed() >= CALL_IDLE
            });
            *ui::RECORDING.lock().unwrap() = None;
            // A failed recording must not take the menu bar app down with it.
            let rec = match rec {
                Ok(rec) => rec,
                Err(e) => {
                    eprintln!("recording failed: {e:#}");
                    continue;
                }
            };
            eprintln!("recording ended: {}", rec.meta.id);
            // Finish in the background so a back-to-back call is still caught.
            let (cfg, store) = (cfg.clone(), store.clone());
            ui::FINISHING.fetch_add(1, Ordering::Relaxed);
            thread::spawn(move || {
                if let Err(e) = rec.finish(&cfg, &store) {
                    eprintln!("finish: {e:#}");
                }
                ui::FINISHING.fetch_sub(1, Ordering::Relaxed);
            });
        }
        // Detecting a call 2 s later is fine and halves the CoreAudio queries.
        thread::sleep(Duration::from_secs(2));
    }
}

/// Bundle id; macOS keys the mic, system audio and accessibility grants to it.
const BUNDLE_ID: &str = "io.github.arzelaascoii.merkwerk";

/// Writes ~/Applications/merkwerk.app around a copy of this binary.
/// macOS asks for system audio on behalf of the app that launched a process, and terminals like
/// Ghostty or Terminal don't declare it (NSAudioCaptureUsageDescription), so from there the other
/// side of a call records as silence. As its own app, merkwerk gets asked for it directly.
fn install() -> Result<()> {
    let home = env::var("HOME").context("HOME not set")?;
    let app = std::path::PathBuf::from(home).join("Applications/merkwerk.app");
    let bin = app.join("Contents/MacOS");
    std::fs::create_dir_all(&bin)?;
    std::fs::copy(env::current_exe()?, bin.join("merkwerk"))?;
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleName</key><string>merkwerk</string>
  <key>CFBundleIdentifier</key><string>{BUNDLE_ID}</string>
  <key>CFBundleExecutable</key><string>merkwerk</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>{}</string>
  <key>LSMinimumSystemVersion</key><string>14.6</string>
  <key>NSMicrophoneUsageDescription</key><string>merkwerk transcribes your side of calls locally.</string>
  <key>NSAudioCaptureUsageDescription</key><string>merkwerk transcribes the other side of calls locally.</string>
  <key>NSCalendarsFullAccessUsageDescription</key><string>merkwerk names meetings and their participants from your calendar.</string>
</dict></plist>
"#,
        env!("CARGO_PKG_VERSION")
    );
    std::fs::write(app.join("Contents/Info.plist"), plist)?;
    // Ad-hoc signature over the whole bundle, so macOS ties the permissions to merkwerk.app.
    let signed = std::process::Command::new("codesign")
        .args(["--force", "--sign", "-", "--identifier", BUNDLE_ID])
        .arg(&app)
        .status()?;
    anyhow::ensure!(signed.success(), "codesign failed");
    println!(
        "installed {}
start it with: open -a merkwerk (quit any merkwerk running in a terminal first)",
        app.display()
    );
    Ok(())
}

/// Started as merkwerk.app, stderr goes nowhere; keep it in `$MERKWERK_DIR/merkwerk.log` instead.
fn log_to_file(cfg: &Config) {
    use std::io::IsTerminal;
    use std::os::fd::AsRawFd;
    if io::stderr().is_terminal() {
        return;
    }
    unsafe extern "C" {
        fn dup2(from: i32, to: i32) -> i32;
    }
    let _ = std::fs::create_dir_all(&cfg.root);
    if let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(cfg.root.join("merkwerk.log"))
    {
        unsafe { dup2(file.as_raw_fd(), 2) };
    }
}

fn record(cfg: &Config, store: &Store, title: &str) -> Result<()> {
    transcribe::ensure_models(cfg)?;
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    thread::spawn(move || {
        let _ = io::stdin().read_line(&mut String::new());
        flag.store(true, Ordering::Relaxed);
    });
    eprintln!("press Enter to stop");
    let rec = session::record(cfg, store, title, None, || stop.load(Ordering::Relaxed))?;
    eprintln!("stopped, finishing transcription ...");
    rec.finish(cfg, store)
}

fn duration(m: &store::Meta) -> String {
    let parse = |s: &str| chrono::DateTime::parse_from_rfc3339(s).ok();
    match (parse(&m.started_at), m.ended_at.as_deref().and_then(parse)) {
        (Some(a), Some(b)) => format!("{}m", (b - a).num_minutes()),
        (Some(_), None) => "live".into(),
        _ => "?".into(),
    }
}

/// `YYYY-MM-DD`, default today.
fn day(arg: Option<&str>) -> Result<chrono::NaiveDate> {
    match arg {
        Some(d) => Ok(chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d").context("date must be YYYY-MM-DD")?),
        None => Ok(chrono::Local::now().date_naive()),
    }
}
