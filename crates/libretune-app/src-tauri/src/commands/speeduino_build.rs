//! Speeduino firmware build-from-source proof-of-concept.
//!
//! Unlike `firmware_update.rs` (STM32/rusEFI DFU/OpenBLT flashing of an
//! already-built binary), Speeduino ships no pre-built `.hex` for its AVR
//! boards -- this module downloads the firmware source, compiles it via an
//! auto-fetched `arduino-cli`, and flashes the result. Scoped to the
//! Arduino Mega 2560 (the standard Speeduino board) for this PoC; ESP32/
//! Teensy/STM32 "Black" variants are not covered.
//!
//! Design ported from a proven, already-working reference: the user's own
//! Speeduino Studio project's `ArduinoCliService`/`AvrdudeService`/
//! `FirmwareReleaseService` (C#/.NET). Command shapes, GitHub API endpoints,
//! and avrdude arguments mirror that implementation directly.
//!
//! Each `#[tauri::command]` is a thin wrapper around an `*_impl` function
//! that takes a plain `base_dir: &Path` (instead of resolving it from an
//! `AppHandle` internally) and `app: Option<&AppHandle>` (only used to emit
//! progress/log events -- `None` skips emission). This keeps the real
//! download/extract/compile logic callable from a plain integration test
//! without needing a running Tauri app, which this project doesn't have a
//! test harness for yet.

use crate::commands::metrics::stop_metrics_task;
use crate::paths::get_firmware_source_dir;
use crate::state::AppState;
use serde::Serialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use tauri::{AppHandle, Emitter};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const AVR_MEGA_FQBN: &str = "arduino:avr:mega";
const SPEEDUINO_REPO: &str = "noisymime/speeduino";
const ARDUINO_CLI_REPO: &str = "arduino/arduino-cli";

#[derive(Debug, Clone, Serialize)]
pub struct SpeeduinoToolchainInfo {
    pub arduino_cli_path: Option<String>,
    pub avr_core_installed: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SpeeduinoRelease {
    pub version: String,
    pub published_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SpeeduinoBuildResult {
    pub success: bool,
    pub hex_path: Option<String>,
    pub log: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct SpeeduinoBuildLogEvent {
    line: String,
}

#[derive(Debug, Clone, Serialize)]
struct SpeeduinoDownloadProgressEvent {
    received_bytes: u64,
    total_bytes: u64,
}

fn push_log(app: Option<&AppHandle>, log: &mut Vec<String>, line: impl Into<String>) {
    let line = line.into();
    if let Some(app) = app {
        let _ = app.emit(
            "speeduino-build:log",
            SpeeduinoBuildLogEvent { line: line.clone() },
        );
    }
    log.push(line);
}

// ── Tool discovery ──────────────────────────────────────────────────────

fn tools_dir(base_dir: &Path) -> PathBuf {
    base_dir.join("tools")
}

fn arduino_cli_exe_name() -> &'static str {
    if cfg!(windows) {
        "arduino-cli.exe"
    } else {
        "arduino-cli"
    }
}

fn avrdude_exe_name() -> &'static str {
    if cfg!(windows) {
        "avrdude.exe"
    } else {
        "avrdude"
    }
}

/// Our own auto-downloaded copy (known-good) takes priority over PATH,
/// matching the reference implementation's own precedence.
fn find_in_tools_dir_or_path(base_dir: &Path, exe_name: &str) -> Option<PathBuf> {
    let cached = tools_dir(base_dir).join(exe_name);
    if cached.is_file() {
        return Some(cached);
    }
    if let Ok(path_var) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let candidate = dir.join(exe_name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

fn find_arduino_cli(base_dir: &Path) -> Option<PathBuf> {
    find_in_tools_dir_or_path(base_dir, arduino_cli_exe_name())
}

fn find_avrdude(base_dir: &Path) -> Option<PathBuf> {
    find_in_tools_dir_or_path(base_dir, avrdude_exe_name())
}

async fn check_avr_core_installed(cli: &Path) -> bool {
    let output = tokio::process::Command::new(cli)
        .args(["core", "list"])
        .output()
        .await;
    match output {
        Ok(out) => String::from_utf8_lossy(&out.stdout).contains("arduino:avr"),
        Err(_) => false,
    }
}

// ── Streamed subprocess execution ───────────────────────────────────────

/// Run a subprocess, emitting each stdout/stderr line as a
/// `speeduino-build:log` event as it arrives (not after the process exits).
async fn run_streamed(
    app: Option<&AppHandle>,
    exe: &Path,
    args: &[&str],
    cwd: Option<&Path>,
    log: &mut Vec<String>,
) -> Result<i32, String> {
    let mut cmd = tokio::process::Command::new(exe);
    cmd.args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("Failed to start {}: {e}", exe.display()))?;

    let stdout = child.stdout.take().ok_or("Failed to capture stdout")?;
    let stderr = child.stderr.take().ok_or("Failed to capture stderr")?;
    let mut out_lines = BufReader::new(stdout).lines();
    let mut err_lines = BufReader::new(stderr).lines();

    let mut out_done = false;
    let mut err_done = false;
    loop {
        tokio::select! {
            line = out_lines.next_line(), if !out_done => {
                match line {
                    Ok(Some(l)) => push_log(app, log, l),
                    _ => out_done = true,
                }
            }
            line = err_lines.next_line(), if !err_done => {
                match line {
                    Ok(Some(l)) => push_log(app, log, l),
                    _ => err_done = true,
                }
            }
            status = child.wait(), if out_done && err_done => {
                let status = status.map_err(|e| e.to_string())?;
                return Ok(status.code().unwrap_or(-1));
            }
        }
    }
}

// ── Downloads ────────────────────────────────────────────────────────────

fn http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent("LibreTune")
        .build()
        .map_err(|e| e.to_string())
}

async fn fetch_latest_release_json(
    client: &reqwest::Client,
    repo: &str,
) -> Result<serde_json::Value, String> {
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let resp = client
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| format!("Failed to fetch release info: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("GitHub API returned {}", resp.status()));
    }
    resp.json::<serde_json::Value>()
        .await
        .map_err(|e| format!("Failed to parse release JSON: {e}"))
}

async fn download_with_progress(
    app: Option<&AppHandle>,
    client: &reqwest::Client,
    url: &str,
    dest_path: &Path,
) -> Result<(), String> {
    use futures_util::StreamExt;

    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("Download request failed: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("Download failed: HTTP {}", response.status()));
    }
    let total_bytes = response.content_length().unwrap_or(0);
    let mut received_bytes: u64 = 0;

    if let Some(parent) = dest_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("Failed to create directory: {e}"))?;
    }
    let mut file = tokio::fs::File::create(dest_path)
        .await
        .map_err(|e| format!("Failed to create file: {e}"))?;

    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("Download error: {e}"))?;
        file.write_all(&chunk)
            .await
            .map_err(|e| format!("Write error: {e}"))?;
        received_bytes += chunk.len() as u64;
        if let Some(app) = app {
            let _ = app.emit(
                "speeduino-build:download-progress",
                SpeeduinoDownloadProgressEvent {
                    received_bytes,
                    total_bytes,
                },
            );
        }
    }
    Ok(())
}

// ── Archive extraction ──────────────────────────────────────────────────

/// Extract a single named binary from a zip archive (case-insensitive,
/// ignoring any directory prefix) into `dest_dir`.
fn extract_zip_binary(
    archive_path: &Path,
    binary_name: &str,
    dest_dir: &Path,
) -> Result<(), String> {
    let file = std::fs::File::open(archive_path).map_err(|e| e.to_string())?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| e.to_string())?;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).map_err(|e| e.to_string())?;
        let entry_name = entry
            .name()
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or("")
            .to_string();
        if entry_name.eq_ignore_ascii_case(binary_name) {
            let dest = dest_dir.join(binary_name);
            let mut out = std::fs::File::create(&dest).map_err(|e| e.to_string())?;
            std::io::copy(&mut entry, &mut out).map_err(|e| e.to_string())?;
            return Ok(());
        }
    }
    Err(format!("{binary_name} not found in archive"))
}

/// Extract a single named binary from a .tar.gz archive into `dest_dir`.
fn extract_targz_binary(
    archive_path: &Path,
    binary_name: &str,
    dest_dir: &Path,
) -> Result<(), String> {
    let file = std::fs::File::open(archive_path).map_err(|e| e.to_string())?;
    let decoder = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(decoder);
    for entry in archive.entries().map_err(|e| e.to_string())? {
        let mut entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path().map_err(|e| e.to_string())?.into_owned();
        let entry_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if entry_name == binary_name {
            let dest = dest_dir.join(binary_name);
            let mut out = std::fs::File::create(&dest).map_err(|e| e.to_string())?;
            std::io::copy(&mut entry, &mut out).map_err(|e| e.to_string())?;
            return Ok(());
        }
    }
    Err(format!("{binary_name} not found in archive"))
}

fn extract_zip_all(zip_path: &Path, dest_dir: &Path) -> Result<(), String> {
    let file = std::fs::File::open(zip_path).map_err(|e| e.to_string())?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| e.to_string())?;
    zip.extract(dest_dir).map_err(|e| e.to_string())
}

#[cfg(unix)]
fn mark_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path) {
        let mut perms = meta.permissions();
        perms.set_mode(0o755);
        let _ = std::fs::set_permissions(path, perms);
    }
}

#[cfg(not(unix))]
fn mark_executable(_path: &Path) {}

// ── Locating the sketch inside downloaded source ────────────────────────

/// Given an extracted Speeduino source tree, locate the sketch folder (the
/// one containing `speeduino.ino`) that arduino-cli should compile. GitHub's
/// release zip wraps everything in a single top-level "speeduino-<version>/"
/// folder, inside which the .ino lives at "speeduino/speeduino.ino".
fn find_sketch_root(extract_dir: &Path) -> Option<PathBuf> {
    if let Some(top_level) = first_subdir(extract_dir) {
        if let Some(found) = find_dir_containing_ino(&top_level) {
            return Some(found);
        }
    }
    find_dir_containing_ino(extract_dir)
}

fn first_subdir(dir: &Path) -> Option<PathBuf> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    if entries.len() == 1 {
        entries.pop()
    } else {
        None
    }
}

fn find_dir_containing_ino(root: &Path) -> Option<PathBuf> {
    if dir_has_ino(root) {
        return Some(root.to_path_buf());
    }
    let entries = std::fs::read_dir(root).ok()?;
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.is_dir() && dir_has_ino(&path) {
            return Some(path);
        }
    }
    None
}

fn dir_has_ino(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .any(|e| e.path().extension().and_then(|s| s.to_str()) == Some("ino"))
        })
        .unwrap_or(false)
}

/// Extract Arduino library names from arduino-cli's "fatal error: X.h: No
/// such file or directory" compile output, so they can be auto-installed via
/// `arduino-cli lib install`.
fn parse_missing_libraries(output: &str) -> Vec<String> {
    let re = regex::Regex::new(r"fatal error: (\S+)\.h: No such file or directory").unwrap();
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    for cap in re.captures_iter(output) {
        let header = &cap[1];
        let lib_name = header
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or(header)
            .to_string();
        if seen.insert(lib_name.clone()) {
            result.push(lib_name);
        }
    }
    result
}

// ── Testable inner implementations ──────────────────────────────────────

async fn get_speeduino_toolchain_info_impl(base_dir: &Path) -> SpeeduinoToolchainInfo {
    let cli_path = find_arduino_cli(base_dir);
    let avr_core_installed = match &cli_path {
        Some(cli) => check_avr_core_installed(cli).await,
        None => false,
    };
    SpeeduinoToolchainInfo {
        arduino_cli_path: cli_path.map(|p| p.display().to_string()),
        avr_core_installed,
    }
}

async fn download_arduino_cli_impl(
    base_dir: &Path,
    app: Option<&AppHandle>,
) -> Result<String, String> {
    let client = http_client()?;
    let release = fetch_latest_release_json(&client, ARDUINO_CLI_REPO).await?;
    let assets = release["assets"]
        .as_array()
        .ok_or("No assets in arduino-cli release")?;

    let (os_marker, is_zip): (&str, bool) = if cfg!(target_os = "windows") {
        ("Windows_64bit", true)
    } else if cfg!(target_os = "macos") {
        ("macOS_64bit", false)
    } else {
        ("Linux_64bit", false)
    };

    let asset = assets
        .iter()
        .find(|a| {
            a["name"]
                .as_str()
                .map(|n| n.contains(os_marker) && (n.ends_with(".zip") || n.ends_with(".tar.gz")))
                .unwrap_or(false)
        })
        .ok_or_else(|| format!("Could not find a {os_marker} arduino-cli release asset"))?;

    let download_url = asset["browser_download_url"]
        .as_str()
        .ok_or("Asset missing download URL")?;
    let asset_name = asset["name"].as_str().unwrap_or("arduino-cli-download");

    let dest_dir = tools_dir(base_dir);
    std::fs::create_dir_all(&dest_dir).map_err(|e| e.to_string())?;
    let archive_path = dest_dir.join(asset_name);

    download_with_progress(app, &client, download_url, &archive_path).await?;

    let final_name = arduino_cli_exe_name();
    if is_zip {
        extract_zip_binary(&archive_path, final_name, &dest_dir)?;
    } else {
        extract_targz_binary(&archive_path, final_name, &dest_dir)?;
    }
    let _ = std::fs::remove_file(&archive_path);

    let final_path = dest_dir.join(final_name);
    mark_executable(&final_path);
    if !final_path.is_file() {
        return Err("arduino-cli binary not found after extraction".to_string());
    }
    Ok(final_path.display().to_string())
}

async fn ensure_avr_core_impl(
    base_dir: &Path,
    app: Option<&AppHandle>,
) -> Result<SpeeduinoBuildResult, String> {
    let cli = find_arduino_cli(base_dir).ok_or("arduino-cli not found — download it first")?;
    let mut log = Vec::new();
    if check_avr_core_installed(&cli).await {
        push_log(app, &mut log, "arduino:avr core already installed.");
        return Ok(SpeeduinoBuildResult {
            success: true,
            hex_path: None,
            log,
        });
    }
    push_log(
        app,
        &mut log,
        "Installing arduino:avr core (this may take a minute)...",
    );
    let exit_code = run_streamed(
        app,
        &cli,
        &["core", "install", "arduino:avr"],
        None,
        &mut log,
    )
    .await?;
    Ok(SpeeduinoBuildResult {
        success: exit_code == 0,
        hex_path: None,
        log,
    })
}

async fn download_speeduino_source_impl(
    base_dir: &Path,
    app: Option<&AppHandle>,
    version: &str,
) -> Result<String, String> {
    let client = http_client()?;
    let url = format!("https://github.com/{SPEEDUINO_REPO}/archive/refs/tags/{version}.zip");
    std::fs::create_dir_all(base_dir).map_err(|e| e.to_string())?;
    let zip_path = base_dir.join(format!("speeduino_{version}.zip"));

    download_with_progress(app, &client, &url, &zip_path).await?;

    let extract_dir = base_dir.join(format!("speeduino_{version}"));
    if extract_dir.exists() {
        std::fs::remove_dir_all(&extract_dir).map_err(|e| e.to_string())?;
    }
    extract_zip_all(&zip_path, &extract_dir)?;
    let _ = std::fs::remove_file(&zip_path);

    find_sketch_root(&extract_dir)
        .map(|p| p.display().to_string())
        .ok_or_else(|| {
            "Could not locate the speeduino sketch folder in the downloaded source".to_string()
        })
}

async fn compile_speeduino_firmware_impl(
    base_dir: &Path,
    app: Option<&AppHandle>,
    sketch_path: &str,
) -> Result<SpeeduinoBuildResult, String> {
    let cli = find_arduino_cli(base_dir).ok_or("arduino-cli not found — download it first")?;
    let build_dir = base_dir.join("build");
    std::fs::create_dir_all(&build_dir).map_err(|e| e.to_string())?;
    let build_dir_str = build_dir.display().to_string();

    let mut log = Vec::new();
    let mut exit_code = -1;

    for attempt in 1..=10 {
        push_log(
            app,
            &mut log,
            if attempt == 1 {
                "Compiling for Arduino Mega 2560...".to_string()
            } else {
                format!("Retrying compile (attempt {attempt})...")
            },
        );

        let mut attempt_log = Vec::new();
        exit_code = run_streamed(
            app,
            &cli,
            &[
                "compile",
                "--fqbn",
                AVR_MEGA_FQBN,
                "--output-dir",
                build_dir_str.as_str(),
                sketch_path,
            ],
            None,
            &mut attempt_log,
        )
        .await?;
        log.extend(attempt_log.iter().cloned());

        if exit_code == 0 {
            break;
        }

        let missing = parse_missing_libraries(&attempt_log.join("\n"));
        if missing.is_empty() {
            push_log(
                app,
                &mut log,
                "Compile failed — no missing libraries detected. See output above.",
            );
            break;
        }

        for lib_name in missing {
            push_log(
                app,
                &mut log,
                format!("Missing library detected: {lib_name}.h -> installing '{lib_name}'..."),
            );
            let mut lib_log = Vec::new();
            let lib_exit = run_streamed(
                app,
                &cli,
                &["lib", "install", lib_name.as_str()],
                None,
                &mut lib_log,
            )
            .await?;
            log.extend(lib_log);
            if lib_exit != 0 {
                push_log(app, &mut log, format!("  Warning: could not auto-install '{lib_name}' — you may need to install it manually."));
            }
        }
    }

    let sketch_name = Path::new(sketch_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("speeduino");
    let mut hex_path = build_dir.join(format!("{sketch_name}.ino.hex"));
    if !hex_path.is_file() {
        if let Some(found) = std::fs::read_dir(&build_dir).ok().and_then(|entries| {
            entries
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .find(|p| p.extension().and_then(|s| s.to_str()) == Some("hex"))
        }) {
            hex_path = found;
        }
    }

    let success = exit_code == 0 && hex_path.is_file();
    if success {
        push_log(app, &mut log, format!("Compiled: {}", hex_path.display()));
    }

    Ok(SpeeduinoBuildResult {
        success,
        hex_path: success.then(|| hex_path.display().to_string()),
        log,
    })
}

async fn upload_speeduino_firmware_impl(
    base_dir: &Path,
    app: Option<&AppHandle>,
    sketch_path: &str,
    port: &str,
) -> Result<SpeeduinoBuildResult, String> {
    let build_dir = base_dir.join("build");
    let mut log = Vec::new();

    if let Some(cli) = find_arduino_cli(base_dir) {
        push_log(
            app,
            &mut log,
            format!("Uploading to {port} via arduino-cli..."),
        );
        let build_dir_str = build_dir.display().to_string();
        let exit_code = run_streamed(
            app,
            &cli,
            &[
                "upload",
                "--fqbn",
                AVR_MEGA_FQBN,
                "-p",
                port,
                "--input-dir",
                build_dir_str.as_str(),
                sketch_path,
            ],
            None,
            &mut log,
        )
        .await?;
        if exit_code == 0 {
            push_log(app, &mut log, "Upload successful.");
            return Ok(SpeeduinoBuildResult {
                success: true,
                hex_path: None,
                log,
            });
        }
        push_log(
            app,
            &mut log,
            format!("arduino-cli upload failed (exit {exit_code}) — trying avrdude directly."),
        );
    } else {
        push_log(
            app,
            &mut log,
            "arduino-cli not found — trying avrdude directly.",
        );
    }

    let avrdude = find_avrdude(base_dir)
        .ok_or("Neither arduino-cli nor avrdude is available. Download arduino-cli first.")?;
    let hex_path = std::fs::read_dir(&build_dir)
        .map_err(|e| e.to_string())?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().and_then(|s| s.to_str()) == Some("hex"))
        .ok_or("No compiled .hex found in the build directory")?;

    let avrdude_dir = avrdude.parent().map(|p| p.to_path_buf());
    let conf_path = avrdude_dir.as_ref().map(|d| d.join("avrdude.conf"));
    let hex_str = hex_path.display().to_string();
    let flash_arg = format!("flash:w:{hex_str}:i");
    let conf_str = conf_path
        .as_ref()
        .filter(|c| c.is_file())
        .map(|c| c.display().to_string());

    let mut args: Vec<&str> = Vec::new();
    if let Some(ref conf) = conf_str {
        args.push("-C");
        args.push(conf.as_str());
    }
    args.extend([
        "-p",
        "atmega2560",
        "-c",
        "wiring",
        "-P",
        port,
        "-b",
        "115200",
        "-D",
        "-U",
        flash_arg.as_str(),
        "-v",
    ]);

    push_log(
        app,
        &mut log,
        format!("Running: avrdude {}", args.join(" ")),
    );
    let exit_code = run_streamed(app, &avrdude, &args, avrdude_dir.as_deref(), &mut log).await?;
    let success = exit_code == 0;
    push_log(
        app,
        &mut log,
        if success {
            "Flash successful.".to_string()
        } else {
            format!("Flash failed (exit {exit_code}).")
        },
    );

    Ok(SpeeduinoBuildResult {
        success,
        hex_path: Some(hex_str),
        log,
    })
}

// ── Tauri commands (thin wrappers) ──────────────────────────────────────

#[tauri::command]
pub async fn get_speeduino_toolchain_info(
    app: AppHandle,
) -> Result<SpeeduinoToolchainInfo, String> {
    Ok(get_speeduino_toolchain_info_impl(&get_firmware_source_dir(&app)).await)
}

#[tauri::command]
pub async fn download_arduino_cli(app: AppHandle) -> Result<String, String> {
    let base_dir = get_firmware_source_dir(&app);
    download_arduino_cli_impl(&base_dir, Some(&app)).await
}

#[tauri::command]
pub async fn ensure_avr_core(app: AppHandle) -> Result<SpeeduinoBuildResult, String> {
    let base_dir = get_firmware_source_dir(&app);
    ensure_avr_core_impl(&base_dir, Some(&app)).await
}

#[tauri::command]
pub async fn check_latest_speeduino_release() -> Result<SpeeduinoRelease, String> {
    let client = http_client()?;
    let release = fetch_latest_release_json(&client, SPEEDUINO_REPO).await?;
    Ok(SpeeduinoRelease {
        version: release["tag_name"]
            .as_str()
            .unwrap_or("unknown")
            .to_string(),
        published_at: release["published_at"].as_str().unwrap_or("").to_string(),
    })
}

#[tauri::command]
pub async fn download_speeduino_source(app: AppHandle, version: String) -> Result<String, String> {
    let base_dir = get_firmware_source_dir(&app);
    download_speeduino_source_impl(&base_dir, Some(&app), &version).await
}

#[tauri::command]
pub async fn compile_speeduino_firmware(
    app: AppHandle,
    sketch_path: String,
) -> Result<SpeeduinoBuildResult, String> {
    let base_dir = get_firmware_source_dir(&app);
    compile_speeduino_firmware_impl(&base_dir, Some(&app), &sketch_path).await
}

#[tauri::command]
pub async fn upload_speeduino_firmware(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    sketch_path: String,
    port: String,
) -> Result<SpeeduinoBuildResult, String> {
    // Release LibreTune's own tuning connection first -- if it's still
    // holding the serial port open, avrdude/arduino-cli will fail to open
    // it for flashing. Mirrors firmware_update.rs's update_ecu_firmware.
    let mut log = Vec::new();
    if state.connection.lock().await.is_some() {
        push_log(
            Some(&app),
            &mut log,
            "Releasing the active ECU connection before flashing…",
        );
        stop_metrics_task(state.clone()).await;
        {
            let mut task_guard = state.streaming_task.lock().await;
            if let Some(handle) = task_guard.take() {
                handle.abort();
            }
        }
        *state.connection.lock().await = None;
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }

    let base_dir = get_firmware_source_dir(&app);
    let mut result =
        upload_speeduino_firmware_impl(&base_dir, Some(&app), &sketch_path, &port).await?;
    log.append(&mut result.log);
    result.log = log;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_missing_libraries_extracts_names_without_path_or_extension() {
        let output = "\
src/speeduino.ino:12:10: fatal error: PID_v1.h: No such file or directory
 #include <PID_v1.h>
          ^~~~~~~~~~
compilation terminated.
";
        let missing = parse_missing_libraries(output);
        assert_eq!(missing, vec!["PID_v1".to_string()]);
    }

    #[test]
    fn test_parse_missing_libraries_dedupes_and_strips_directory_prefix() {
        let output = "\
fatal error: some/nested/Wire.h: No such file or directory
fatal error: Wire.h: No such file or directory
";
        let missing = parse_missing_libraries(output);
        assert_eq!(missing, vec!["Wire".to_string()]);
    }

    #[test]
    fn test_parse_missing_libraries_returns_empty_when_no_match() {
        let output = "Sketch uses 45000 bytes of program storage space.\n";
        assert!(parse_missing_libraries(output).is_empty());
    }

    #[test]
    fn test_find_sketch_root_locates_ino_inside_github_release_layout() {
        let tmp = tempfile::tempdir().unwrap();
        let extract_dir = tmp.path().join("extracted");
        let top_level = extract_dir.join("speeduino-202501.7");
        let sketch_dir = top_level.join("speeduino");
        std::fs::create_dir_all(&sketch_dir).unwrap();
        std::fs::write(sketch_dir.join("speeduino.ino"), "// sketch").unwrap();
        // A sibling non-sketch top-level file/dir to make sure the walk isn't
        // fooled by the first directory entry alone.
        std::fs::create_dir_all(top_level.join("reference")).unwrap();

        let found = find_sketch_root(&extract_dir).expect("should find the sketch dir");
        assert_eq!(found, sketch_dir);
    }

    #[test]
    fn test_find_sketch_root_returns_none_when_no_ino_anywhere() {
        let tmp = tempfile::tempdir().unwrap();
        let extract_dir = tmp.path().join("extracted");
        std::fs::create_dir_all(extract_dir.join("speeduino-202501.7").join("docs")).unwrap();

        assert!(find_sketch_root(&extract_dir).is_none());
    }

    // ── Real, network-dependent integration test ────────────────────────
    // Ignored by default (slow: real downloads + a real ~1-2 min AVR
    // compile). Run explicitly with:
    //   cargo test -p libretune-app --lib speeduino_build::tests::test_real_download_and_compile -- --ignored --nocapture
    // Proves the actual shipped download/extract/compile logic works
    // end-to-end against the real arduino-cli and Speeduino GitHub releases,
    // not just that the code compiles.
    #[test]
    #[ignore]
    fn test_real_download_and_compile_produces_a_hex() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let tmp = tempfile::tempdir().unwrap();
            let base_dir = tmp.path();

            println!("== Downloading arduino-cli ==");
            let cli_path = download_arduino_cli_impl(base_dir, None)
                .await
                .expect("arduino-cli download should succeed");
            println!("arduino-cli at: {cli_path}");
            assert!(Path::new(&cli_path).is_file());

            println!("== Ensuring arduino:avr core ==");
            let core_result = ensure_avr_core_impl(base_dir, None)
                .await
                .expect("core install should not error");
            for line in &core_result.log {
                println!("{line}");
            }
            assert!(core_result.success, "avr core install should succeed");

            println!("== Checking latest Speeduino release ==");
            let client = http_client().unwrap();
            let release = fetch_latest_release_json(&client, SPEEDUINO_REPO)
                .await
                .expect("should fetch latest release");
            let version = release["tag_name"].as_str().unwrap().to_string();
            println!("Latest Speeduino release: {version}");

            println!("== Downloading Speeduino source {version} ==");
            let sketch_path = download_speeduino_source_impl(base_dir, None, &version)
                .await
                .expect("source download should succeed");
            println!("Sketch at: {sketch_path}");
            assert!(Path::new(&sketch_path).is_dir());

            println!("== Compiling for Arduino Mega 2560 ==");
            let build_result = compile_speeduino_firmware_impl(base_dir, None, &sketch_path)
                .await
                .expect("compile should not error");
            for line in &build_result.log {
                println!("{line}");
            }
            assert!(build_result.success, "compile should succeed");
            let hex_path = build_result
                .hex_path
                .expect("should have a hex path on success");
            println!("Compiled hex: {hex_path}");
            assert!(
                Path::new(&hex_path).is_file(),
                "hex file should actually exist"
            );
            let hex_size = std::fs::metadata(&hex_path).unwrap().len();
            println!("Hex file size: {hex_size} bytes");
            assert!(
                hex_size > 1000,
                "hex file should be a real, non-trivial firmware image"
            );
        });
    }
}
