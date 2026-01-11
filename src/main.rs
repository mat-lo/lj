use clap::{Parser, Subcommand};
use console::{style, Term};
use dialoguer::{theme::ColorfulTheme, Input, MultiSelect};
use futures_util::StreamExt;
use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const RD_BASE_URL: &str = "https://api.real-debrid.com/rest/1.0";

#[derive(Parser)]
#[command(name = "lj")]
#[command(about = "Download magnets via Real-Debrid", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Magnet link to download
    #[arg(value_name = "MAGNET")]
    magnet: Option<String>,
}

#[derive(Subcommand)]
enum Commands {
    /// Show downloads in progress
    Dl,
    /// Set or update API key
    SetKey,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct Download {
    id: String,
    filename: String,
    url: String,
    target_dir: String,
    total_bytes: u64,
    downloaded_bytes: u64,
    speed: f64,
    status: DownloadStatus,
    started_at: u64,
    pid: Option<u32>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
enum DownloadStatus {
    Pending,
    Downloading,
    Completed,
    Failed(String),
    Cancelled,
}

#[derive(Debug, Deserialize)]
struct AddMagnetResponse {
    id: String,
    #[allow(dead_code)]
    uri: String,
}

#[derive(Debug, Deserialize)]
struct TorrentInfo {
    #[allow(dead_code)]
    id: String,
    status: String,
    files: Option<Vec<TorrentFile>>,
    links: Option<Vec<String>>,
    progress: Option<f64>,
    speed: Option<u64>,
    seeders: Option<u32>,
}

#[derive(Debug, Deserialize, Clone)]
struct TorrentFile {
    id: u32,
    path: String,
    bytes: u64,
    #[allow(dead_code)]
    selected: u8,
}

#[derive(Debug, Deserialize)]
struct UnrestrictResponse {
    filename: String,
    download: String,
    #[allow(dead_code)]
    filesize: Option<u64>,
}

fn get_config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("lj")
}

fn get_downloads_dir() -> PathBuf {
    get_config_dir().join("downloads")
}

fn get_download_file(id: &str) -> PathBuf {
    get_downloads_dir().join(format!("{}.json", id))
}

fn get_api_key_file() -> PathBuf {
    get_config_dir().join("api_key")
}

fn load_api_key() -> Option<String> {
    if let Ok(key) = env::var("RD_API_TOKEN") {
        if !key.is_empty() {
            return Some(key);
        }
    }

    let key_file = get_api_key_file();
    if key_file.exists() {
        if let Ok(key) = fs::read_to_string(&key_file) {
            let key = key.trim().to_string();
            if !key.is_empty() {
                return Some(key);
            }
        }
    }
    None
}

fn save_api_key(key: &str) -> io::Result<()> {
    let config_dir = get_config_dir();
    fs::create_dir_all(&config_dir)?;
    fs::write(get_api_key_file(), key)?;
    Ok(())
}

fn save_download(download: &Download) -> io::Result<()> {
    let downloads_dir = get_downloads_dir();
    fs::create_dir_all(&downloads_dir)?;
    let data = serde_json::to_string_pretty(download)?;
    fs::write(get_download_file(&download.id), data)?;
    Ok(())
}

fn load_download(id: &str) -> Option<Download> {
    let path = get_download_file(id);
    if path.exists() {
        if let Ok(data) = fs::read_to_string(&path) {
            return serde_json::from_str(&data).ok();
        }
    }
    None
}

fn load_all_downloads() -> Vec<Download> {
    let downloads_dir = get_downloads_dir();
    let mut downloads = Vec::new();

    if let Ok(entries) = fs::read_dir(&downloads_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().map(|e| e == "json").unwrap_or(false) {
                if let Ok(data) = fs::read_to_string(&path) {
                    if let Ok(dl) = serde_json::from_str::<Download>(&data) {
                        downloads.push(dl);
                    }
                }
            }
        }
    }

    downloads.sort_by_key(|dl| dl.started_at);
    downloads
}

fn delete_download(id: &str) {
    let path = get_download_file(id);
    let _ = fs::remove_file(path);
}

fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;

    if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.2} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.2} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

fn format_speed(bytes_per_sec: f64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;

    if bytes_per_sec >= MB {
        format!("{:.2} MB/s", bytes_per_sec / MB)
    } else if bytes_per_sec >= KB {
        format!("{:.2} KB/s", bytes_per_sec / KB)
    } else {
        format!("{:.0} B/s", bytes_per_sec)
    }
}

async fn prompt_api_key() -> Option<String> {
    println!("{}", style("Real-Debrid API key not found.").yellow());
    println!("Get your API key from: https://real-debrid.com/apitoken\n");

    let key: String = Input::with_theme(&ColorfulTheme::default())
        .with_prompt("Enter your Real-Debrid API key")
        .interact_text()
        .ok()?;

    if key.is_empty() {
        return None;
    }

    if let Err(e) = save_api_key(&key) {
        eprintln!("{} Failed to save API key: {}", style("Error:").red(), e);
    } else {
        println!("{}", style("API key saved!").green());
    }

    Some(key)
}

async fn add_magnet(client: &Client, api_key: &str, magnet: &str) -> Result<String, String> {
    let resp = client
        .post(format!("{}/torrents/addMagnet", RD_BASE_URL))
        .bearer_auth(api_key)
        .form(&[("magnet", magnet)])
        .send()
        .await
        .map_err(|e| format!("Failed to add magnet: {}", e))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("Failed to add magnet: {} - {}", status, text));
    }

    let data: AddMagnetResponse = resp
        .json()
        .await
        .map_err(|e| format!("Failed to parse response: {}", e))?;

    Ok(data.id)
}

async fn get_torrent_info(
    client: &Client,
    api_key: &str,
    torrent_id: &str,
) -> Result<TorrentInfo, String> {
    let resp = client
        .get(format!("{}/torrents/info/{}", RD_BASE_URL, torrent_id))
        .bearer_auth(api_key)
        .send()
        .await
        .map_err(|e| format!("Failed to get torrent info: {}", e))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("Failed to get torrent info: {} - {}", status, text));
    }

    resp.json()
        .await
        .map_err(|e| format!("Failed to parse torrent info: {}", e))
}

async fn select_files(
    client: &Client,
    api_key: &str,
    torrent_id: &str,
    file_ids: &[u32],
) -> Result<(), String> {
    let ids = file_ids
        .iter()
        .map(|id| id.to_string())
        .collect::<Vec<_>>()
        .join(",");

    let resp = client
        .post(format!("{}/torrents/selectFiles/{}", RD_BASE_URL, torrent_id))
        .bearer_auth(api_key)
        .form(&[("files", ids)])
        .send()
        .await
        .map_err(|e| format!("Failed to select files: {}", e))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("Failed to select files: {} - {}", status, text));
    }

    Ok(())
}

async fn unrestrict_link(
    client: &Client,
    api_key: &str,
    link: &str,
) -> Result<UnrestrictResponse, String> {
    let resp = client
        .post(format!("{}/unrestrict/link", RD_BASE_URL))
        .bearer_auth(api_key)
        .form(&[("link", link)])
        .send()
        .await
        .map_err(|e| format!("Failed to unrestrict link: {}", e))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("Failed to unrestrict link: {} - {}", status, text));
    }

    resp.json()
        .await
        .map_err(|e| format!("Failed to parse unrestrict response: {}", e))
}

async fn delete_torrent(client: &Client, api_key: &str, torrent_id: &str) -> Result<(), String> {
    let resp = client
        .delete(format!("{}/torrents/delete/{}", RD_BASE_URL, torrent_id))
        .bearer_auth(api_key)
        .send()
        .await
        .map_err(|e| format!("Failed to delete torrent: {}", e))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("Failed to delete torrent: {} - {}", status, text));
    }

    Ok(())
}

async fn wait_for_files(
    client: &Client,
    api_key: &str,
    torrent_id: &str,
) -> Result<Vec<TorrentFile>, String> {
    let start = Instant::now();
    let timeout = Duration::from_secs(60);

    loop {
        if start.elapsed() > timeout {
            return Err("Timeout waiting for file list".to_string());
        }

        let info = get_torrent_info(client, api_key, torrent_id).await?;

        match info.status.as_str() {
            "waiting_files_selection" => {
                if let Some(files) = info.files {
                    return Ok(files);
                }
            }
            "magnet_error" | "dead" | "error" => {
                return Err(format!("Torrent error: {}", info.status));
            }
            _ => {}
        }

        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

async fn wait_for_download(
    client: &Client,
    api_key: &str,
    torrent_id: &str,
) -> Result<Vec<String>, String> {
    let start = Instant::now();
    let timeout = Duration::from_secs(600);

    loop {
        if start.elapsed() > timeout {
            return Err("Timeout waiting for Real-Debrid to process".to_string());
        }

        let info = get_torrent_info(client, api_key, torrent_id).await?;

        match info.status.as_str() {
            "downloaded" => {
                if let Some(links) = info.links {
                    return Ok(links);
                }
                return Err("No links available".to_string());
            }
            "magnet_error" | "dead" | "error" => {
                return Err(format!("Torrent error: {}", info.status));
            }
            "downloading" | "queued" | "compressing" | "uploading" => {
                let progress = info.progress.unwrap_or(0.0);
                let speed = info.speed.unwrap_or(0) as f64 / 1_000_000.0;
                let seeders = info.seeders.unwrap_or(0);
                print!(
                    "\r{} {:.1}% @ {:.2} MB/s ({} seeders)    ",
                    style("RD Processing:").cyan(),
                    progress,
                    speed,
                    seeders
                );
                io::stdout().flush().ok();
            }
            _ => {}
        }

        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn process_magnet(api_key: &str, magnet: &str) -> Result<Vec<(String, String, u64)>, String> {
    let client = Client::new();

    println!("{} Adding magnet to Real-Debrid...", style("[1/4]").dim());
    let torrent_id = add_magnet(&client, api_key, magnet).await?;

    println!("{} Waiting for file list...", style("[2/4]").dim());
    let files = wait_for_files(&client, api_key, &torrent_id).await?;

    let valid_files: Vec<_> = files
        .iter()
        .filter(|f| {
            let path_lower = f.path.to_lowercase();
            !path_lower.contains("sample") && f.bytes > 1_000_000
        })
        .cloned()
        .collect();

    let selected_ids: Vec<u32> = if valid_files.len() == 1 {
        println!(
            "  {} {}",
            style("Single file:").green(),
            valid_files[0].path.split('/').last().unwrap_or(&valid_files[0].path)
        );
        vec![valid_files[0].id]
    } else if valid_files.is_empty() {
        if files.is_empty() {
            return Err("No files in torrent".to_string());
        }
        println!("  {}", style("Auto-selecting all files").yellow());
        files.iter().map(|f| f.id).collect()
    } else {
        println!("\n{}", style("Select files to download:").cyan());

        let items: Vec<String> = valid_files
            .iter()
            .map(|f| {
                let name = f.path.split('/').last().unwrap_or(&f.path);
                format!("{} ({})", name, format_bytes(f.bytes))
            })
            .collect();

        let selections = MultiSelect::with_theme(&ColorfulTheme::default())
            .items(&items)
            .defaults(&vec![true; items.len()])
            .interact()
            .map_err(|e| format!("Selection cancelled: {}", e))?;

        if selections.is_empty() {
            let _ = delete_torrent(&client, api_key, &torrent_id).await;
            return Err("No files selected".to_string());
        }

        selections.iter().map(|&i| valid_files[i].id).collect()
    };

    println!("{} Selecting files...", style("[3/4]").dim());
    select_files(&client, api_key, &torrent_id, &selected_ids).await?;

    println!("{} Waiting for Real-Debrid to process...", style("[4/4]").dim());
    let links = wait_for_download(&client, api_key, &torrent_id).await?;
    println!();

    let mut download_links = Vec::new();
    for link in links {
        match unrestrict_link(&client, api_key, &link).await {
            Ok(unrestricted) => {
                let size = if let Ok(resp) = client.head(&unrestricted.download).send().await {
                    resp.headers()
                        .get("content-length")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(0)
                } else {
                    0
                };
                download_links.push((unrestricted.filename, unrestricted.download, size));
            }
            Err(e) => {
                eprintln!("{} {}", style("Warning:").yellow(), e);
            }
        }
    }

    let _ = delete_torrent(&client, api_key, &torrent_id).await;

    if download_links.is_empty() {
        return Err("No download links obtained".to_string());
    }

    Ok(download_links)
}

fn spawn_background_download(download: &Download) {
    let exe = env::current_exe().expect("Failed to get current executable path");

    let child = Command::new(&exe)
        .arg("--bg-download")
        .arg(&download.id)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();

    match child {
        Ok(child) => {
            let mut dl = download.clone();
            dl.pid = Some(child.id());
            dl.status = DownloadStatus::Downloading;
            let _ = save_download(&dl);
        }
        Err(e) => {
            eprintln!("Failed to spawn download process: {}", e);
        }
    }
}

async fn run_background_download(download_id: &str) {
    let mut download = match load_download(download_id) {
        Some(dl) => dl,
        None => {
            eprintln!("Download not found: {}", download_id);
            return;
        }
    };

    download.status = DownloadStatus::Downloading;
    download.pid = Some(std::process::id());
    let _ = save_download(&download);

    let client = Client::new();
    let target_path = PathBuf::from(&download.target_dir).join(&download.filename);

    let result = async {
        let resp = client
            .get(&download.url)
            .send()
            .await
            .map_err(|e| format!("Request failed: {}", e))?;

        if !resp.status().is_success() {
            return Err(format!("HTTP error: {}", resp.status()));
        }

        let total_size = resp.content_length().unwrap_or(download.total_bytes);

        let mut file = tokio::fs::File::create(&target_path)
            .await
            .map_err(|e| format!("Failed to create file: {}", e))?;

        let mut stream = resp.bytes_stream();
        let mut downloaded: u64 = 0;
        let mut last_update = Instant::now();
        let mut last_bytes: u64 = 0;

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| format!("Download error: {}", e))?;

            tokio::io::AsyncWriteExt::write_all(&mut file, &chunk)
                .await
                .map_err(|e| format!("Write error: {}", e))?;

            downloaded += chunk.len() as u64;

            if last_update.elapsed() >= Duration::from_millis(500) {
                let elapsed = last_update.elapsed().as_secs_f64();
                let speed = (downloaded - last_bytes) as f64 / elapsed;

                // Reload to check for cancellation
                if let Some(dl) = load_download(download_id) {
                    if dl.status == DownloadStatus::Cancelled {
                        return Err("Cancelled".to_string());
                    }
                }

                // Update progress
                download.downloaded_bytes = downloaded;
                download.total_bytes = total_size;
                download.speed = speed;
                let _ = save_download(&download);

                last_update = Instant::now();
                last_bytes = downloaded;
            }
        }

        Ok(())
    }
    .await;

    match result {
        Ok(()) => {
            download.status = DownloadStatus::Completed;
            download.downloaded_bytes = download.total_bytes;
            download.speed = 0.0;
            download.pid = None;
        }
        Err(e) => {
            if e == "Cancelled" {
                download.status = DownloadStatus::Cancelled;
                let _ = std::fs::remove_file(&target_path);
            } else {
                download.status = DownloadStatus::Failed(e);
            }
            download.speed = 0.0;
            download.pid = None;
        }
    }
    let _ = save_download(&download);
}

fn show_downloads() {
    let term = Term::stdout();
    let mut downloads = load_all_downloads();

    // Clean up dead processes
    for dl in &mut downloads {
        if dl.status == DownloadStatus::Downloading {
            if let Some(pid) = dl.pid {
                if signal::kill(Pid::from_raw(pid as i32), None).is_err() {
                    if dl.downloaded_bytes >= dl.total_bytes && dl.total_bytes > 0 {
                        dl.status = DownloadStatus::Completed;
                    } else {
                        dl.status = DownloadStatus::Failed("Process died".to_string());
                    }
                    dl.pid = None;
                    let _ = save_download(dl);
                }
            }
        }
    }

    // Reload after cleanup
    let downloads = load_all_downloads();

    if downloads.is_empty() {
        println!("{}", style("No downloads").dim());
        return;
    }

    println!("{}", style("Downloads:").bold());
    println!();

    for (i, dl) in downloads.iter().enumerate() {
        let status_str = match &dl.status {
            DownloadStatus::Pending => style("PENDING").yellow().to_string(),
            DownloadStatus::Downloading => {
                let pct = if dl.total_bytes > 0 {
                    (dl.downloaded_bytes as f64 / dl.total_bytes as f64 * 100.0) as u8
                } else {
                    0
                };
                format!(
                    "{} {}% @ {}",
                    style("DOWNLOADING").cyan(),
                    pct,
                    format_speed(dl.speed)
                )
            }
            DownloadStatus::Completed => style("COMPLETED").green().to_string(),
            DownloadStatus::Failed(e) => format!("{} {}", style("FAILED").red(), e),
            DownloadStatus::Cancelled => style("CANCELLED").dim().to_string(),
        };

        println!(
            "{} {} {}",
            style(format!("[{}]", i + 1)).dim(),
            &dl.filename,
            style(format!("({})", format_bytes(dl.total_bytes))).dim()
        );
        println!("    {} {}", status_str, style(format!("-> {}", dl.target_dir)).dim());

        if dl.status == DownloadStatus::Downloading && dl.total_bytes > 0 {
            let pct = dl.downloaded_bytes as f64 / dl.total_bytes as f64;
            let width = 40;
            let filled = (pct * width as f64) as usize;
            let empty = width - filled;
            println!(
                "    [{}{}]",
                style("=".repeat(filled)).green(),
                " ".repeat(empty)
            );
        }
        println!();
    }

    println!("{}", style("Actions:").bold());
    println!("  [c]ancel <n>  - Cancel download #n");
    println!("  [r]emove <n>  - Remove completed/failed #n");
    println!("  [C]lear       - Clear all completed/failed/cancelled");
    println!("  [q]uit        - Exit");
    println!();

    let download_ids: Vec<String> = downloads.iter().map(|dl| dl.id.clone()).collect();

    loop {
        print!("> ");
        io::stdout().flush().ok();

        let mut input = String::new();
        if io::stdin().read_line(&mut input).is_err() {
            break;
        }

        let input = input.trim();
        if input.is_empty() {
            continue;
        }

        match input.chars().next() {
            Some('q') | Some('Q') => break,
            Some('C') => {
                for dl in &downloads {
                    if matches!(
                        dl.status,
                        DownloadStatus::Completed | DownloadStatus::Failed(_) | DownloadStatus::Cancelled
                    ) {
                        delete_download(&dl.id);
                    }
                }
                let _ = term.clear_screen();
                show_downloads();
                return;
            }
            Some('c') | Some('r') => {
                let is_cancel = input.starts_with('c');
                let num_str = input[1..].trim();
                if let Ok(n) = num_str.parse::<usize>() {
                    if n > 0 && n <= download_ids.len() {
                        let id = &download_ids[n - 1];

                        if is_cancel {
                            if let Some(mut dl) = load_download(id) {
                                if dl.status == DownloadStatus::Downloading {
                                    dl.status = DownloadStatus::Cancelled;
                                    if let Some(pid) = dl.pid {
                                        let _ = signal::kill(
                                            Pid::from_raw(pid as i32),
                                            Signal::SIGTERM,
                                        );
                                    }
                                    dl.pid = None;
                                    let _ = save_download(&dl);
                                    println!("{}", style("Cancelled").yellow());
                                }
                            }
                        } else {
                            delete_download(id);
                            println!("{}", style("Removed").green());
                        }
                    }
                }
            }
            _ => {
                println!("{}", style("Unknown command").red());
            }
        }
    }
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() >= 3 && args[1] == "--bg-download" {
        run_background_download(&args[2]).await;
        return;
    }

    let cli = Cli::parse();

    match cli.command {
        Some(Commands::Dl) => {
            show_downloads();
            return;
        }
        Some(Commands::SetKey) => {
            let key: String = Input::with_theme(&ColorfulTheme::default())
                .with_prompt("Enter your Real-Debrid API key")
                .interact_text()
                .expect("Failed to read input");

            if let Err(e) = save_api_key(&key) {
                eprintln!("{} Failed to save API key: {}", style("Error:").red(), e);
            } else {
                println!("{}", style("API key saved!").green());
            }
            return;
        }
        None => {}
    }

    let magnet = match cli.magnet {
        Some(m) => m,
        None => {
            println!("Usage: lj <magnet>    - Download from magnet link");
            println!("       lj dl          - Show downloads in progress");
            println!("       lj set-key     - Set Real-Debrid API key");
            return;
        }
    };

    if !magnet.starts_with("magnet:") {
        eprintln!("{} Not a valid magnet link", style("Error:").red());
        return;
    }

    let api_key = match load_api_key() {
        Some(key) => key,
        None => match prompt_api_key().await {
            Some(key) => key,
            None => {
                eprintln!("{} API key is required", style("Error:").red());
                return;
            }
        },
    };

    println!();
    match process_magnet(&api_key, &magnet).await {
        Ok(links) => {
            let current_dir = env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .to_string_lossy()
                .to_string();

            println!();
            println!(
                "{} Starting {} download(s) in background...",
                style("Success!").green(),
                links.len()
            );

            for (filename, url, size) in links {
                let id = format!(
                    "{}-{}",
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap()
                        .as_millis(),
                    &filename[..filename.len().min(10)]
                );

                let download = Download {
                    id: id.clone(),
                    filename: filename.clone(),
                    url,
                    target_dir: current_dir.clone(),
                    total_bytes: size,
                    downloaded_bytes: 0,
                    speed: 0.0,
                    status: DownloadStatus::Pending,
                    started_at: SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap()
                        .as_secs(),
                    pid: None,
                };

                // Save download first, then spawn
                let _ = save_download(&download);
                spawn_background_download(&download);

                println!("  {} {}", style("->").green(), filename);
            }

            println!();
            println!(
                "{}",
                style("Downloads running in background. Use 'lj dl' to check progress.").dim()
            );
        }
        Err(e) => {
            eprintln!("{} {}", style("Error:").red(), e);
        }
    }
}
