//! merkwerk.app: macOS keys the microphone, system audio and accessibility grants to an app bundle.

use anyhow::{Context, Result};
use std::env;

/// Bundle id; macOS keys the mic, system audio and accessibility grants to it.
pub const BUNDLE_ID: &str = "io.github.arzelaascoii.merkwerk";

/// Writes ~/Applications/merkwerk.app around a copy of this binary.
/// macOS asks for system audio on behalf of the app that launched a process, and terminals like
/// Ghostty or Terminal don't declare it (NSAudioCaptureUsageDescription), so from there the other
/// side of a call records as silence. As its own app, merkwerk gets asked for it directly.
pub fn install() -> Result<()> {
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
    // Signed with a local certificate, macOS ties the permissions to "this identifier, this
    // certificate" and keeps them across reinstalls; an ad-hoc signature changes with every build
    // and macOS drops Accessibility, system audio and calendar access each time.
    let identity = signing_identity().unwrap_or_else(|e| {
        eprintln!("no local signing certificate ({e:#}); signing ad hoc, permissions reset on every install");
        "-"
    });
    let signed = std::process::Command::new("codesign")
        .args(["--force", "--sign", identity, "--identifier", BUNDLE_ID])
        .arg(&app)
        .status()?;
    anyhow::ensure!(signed.success(), "codesign failed");
    println!("installed {}", app.display());
    Ok(())
}

pub fn in_app_bundle() -> bool {
    env::current_exe().is_ok_and(|p| p.to_string_lossy().contains(".app/Contents/MacOS/"))
}

/// `merkwerk` in a terminal: installs merkwerk.app (or updates it after `brew upgrade`), then opens
/// it. A running app on an older version is left alone; quitting it would cut off a recording.
pub fn start_app() -> Result<()> {
    use std::process::Command;
    let app = std::path::PathBuf::from(env::var("HOME").context("HOME not set")?).join("Applications/merkwerk.app");
    let bin = app.join("Contents/MacOS/merkwerk");
    if std::fs::read(&bin).ok() != Some(std::fs::read(env::current_exe()?)?) {
        let running = Command::new("pgrep").arg("-f").arg(&bin).output()?.status.success();
        anyhow::ensure!(
            !running,
            "merkwerk.app runs an older version. Quit it (menu bar → Quit), then run `merkwerk` again."
        );
        install()?;
    }
    anyhow::ensure!(
        Command::new("open").arg(&app).status()?.success(),
        "could not open {}",
        app.display()
    );
    println!("merkwerk is running: the window shows what is left to set up. Look for mw in the menu bar.");
    Ok(())
}

const SIGNING_NAME: &str = "merkwerk local signing";

/// A self-signed code signing certificate in the login keychain, made on first install. It never
/// leaves this Mac and only has to stay the same; macOS doesn't need to trust it.
// ponytail: codesign may ask once for the login keychain password; "Always Allow" ends that.
fn signing_identity() -> Result<&'static str> {
    use std::process::Command;
    let exists = Command::new("security")
        .args(["find-certificate", "-c", SIGNING_NAME])
        .output()?
        .status
        .success();
    if exists {
        return Ok(SIGNING_NAME);
    }
    let dir = std::env::temp_dir().join(format!("merkwerk-signing-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let run = |cmd: &mut Command| -> Result<()> {
        let out = cmd.current_dir(&dir).output()?;
        anyhow::ensure!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr).trim());
        Ok(())
    };
    let made = (|| {
        std::fs::write(
            dir.join("cfg"),
            format!(
                "[req]\ndistinguished_name = dn\nx509_extensions = ext\nprompt = no\n[dn]\nCN = {SIGNING_NAME}\n\
                 [ext]\nbasicConstraints = critical,CA:false\nkeyUsage = critical,digitalSignature\n\
                 extendedKeyUsage = critical,codeSigning\n"
            ),
        )?;
        // macOS's own LibreSSL writes a PKCS#12 that `security import` reads (OpenSSL 3 needs -legacy).
        run(Command::new("/usr/bin/openssl").args([
            "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "3650", "-keyout", "key.pem", "-out", "cert.pem",
            "-config", "cfg",
        ]))?;
        run(Command::new("/usr/bin/openssl").args([
            "pkcs12",
            "-export",
            "-inkey",
            "key.pem",
            "-in",
            "cert.pem",
            "-out",
            "id.p12",
            "-passout",
            "pass:merkwerk",
            "-name",
            SIGNING_NAME,
        ]))?;
        run(Command::new("security").args(["import", "id.p12", "-P", "merkwerk", "-T", "/usr/bin/codesign"]))
    })();
    // The private key only lives on in the keychain.
    let _ = std::fs::remove_dir_all(&dir);
    made.map(|()| SIGNING_NAME)
}
