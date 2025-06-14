#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

mod archiver;
mod cloudinary;
mod state;

use crate::state::{
    AppState, ArticleResponse, CloudinaryConfig, DownloadedGameInfo, LaunchConfig,
    cleanup_active_downloads, save_active_downloads_to_file,
    save_state_to_file,
};
use ico::IconDir;
use image::DynamicImage;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, File};
use std::path::{Path};
use std::process::Command as StdCommand;
use std::sync::{Mutex, RwLock};
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_notification::NotificationExt;
use tauri_plugin_shell::ShellExt;
use tauri_plugin_shell::process::CommandEvent;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[derive(Serialize, Deserialize, Default, Debug)]
pub struct ActiveDownloads {
    #[serde(default)]
    pub downloads: HashMap<String, DownloadInfo>,
    #[serde(skip)]
    pub tokens: HashMap<String, CancellationToken>,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct DownloadInfo {
    id: String,
    filename: String,
    url: String,
    progress: f32,
    status: String,
    path: Option<String>,
    error: Option<String>,
    provider: Option<String>,
    downloaded_at: Option<String>,
    extracted: bool,
    extracted_path: Option<String>,
    extraction_status: Option<String>, // เพิ่ม: idle, extracting, completed, failed
    extraction_progress: Option<f32>,  // เพิ่ม: ความคืบหน้า (0.0 - 100.0)
}

#[tauri::command]
fn is_directory(path: String) -> Result<bool, String> {
    let path_obj = std::path::Path::new(&path);
    if !path_obj.exists() {
        return Err("Path does not exist".to_string());
    }
    Ok(path_obj.is_dir())
}

#[tauri::command]
async fn unarchive_file(
    file_path: String,
    output_dir: String,
    download_id: String, // เพิ่มเพื่อระบุไฟล์ที่กำลังแตก
    app: AppHandle,
) -> Result<(), String> {
    // ส่งสถานะเริ่มต้น
    app.emit(
        "extraction-progress",
        &serde_json::json!({
            "downloadId": download_id,
            "status": "extracting",
            "progress": 0.0
        }),
    )
    .map_err(|e| format!("Failed to emit extraction progress: {}", e))?;

    // อัปเดตสถานะใน active downloads
    {
        let active_downloads = app.state::<RwLock<ActiveDownloads>>();
        let mut downloads = active_downloads
            .write()
            .map_err(|e| format!("Failed to lock active downloads: {}", e))?;
        if let Some(download) = downloads.downloads.get_mut(&download_id) {
            download.extraction_status = Some("extracting".to_string());
            download.extraction_progress = Some(0.0);
        }
        save_active_downloads_to_file(&app, &downloads)?;
    }

    // เรียกฟังก์ชันแตกไฟล์
    let result = archiver::unarchive_file_with_progress(&file_path, &output_dir, |progress| {
        // ส่งความคืบหน้า (ถ้า library รองรับ)
        app.emit(
            "extraction-progress",
            &serde_json::json!({
                "downloadId": download_id,
                "status": "extracting",
                "progress": progress
            }),
        )
        .ok();
    });

    match result {
        Ok(_) => {
            // อัปเดตสถานะเมื่อสำเร็จ
            app.emit(
                "extraction-progress",
                &serde_json::json!({
                    "downloadId": download_id,
                    "status": "completed",
                    "progress": 100.0
                }),
            )
            .map_err(|e| format!("Failed to emit extraction complete: {}", e))?;

            {
                let active_downloads = app.state::<RwLock<ActiveDownloads>>();
                let mut downloads = active_downloads
                    .write()
                    .map_err(|e| format!("Failed to lock active downloads: {}", e))?;
                if let Some(download) = downloads.downloads.get_mut(&download_id) {
                    download.extraction_status = Some("completed".to_string());
                    download.extraction_progress = Some(100.0);
                    download.extracted = true;
                    download.extracted_path = Some(output_dir.clone());
                }
                save_active_downloads_to_file(&app, &downloads)?;
            }

            app.notification()
                .builder()
                .title("Extraction Complete")
                .body(format!("File extracted to {}", output_dir))
                .show()
                .map_err(|e| format!("Failed to show notification: {}", e))?;

            Ok(())
        }
        Err(e) => {
            // อัปเดตสถานะเมื่อล้มเหลว
            app.emit(
                "extraction-progress",
                &serde_json::json!({
                    "downloadId": download_id,
                    "status": "failed",
                    "progress": 0.0,
                    "error": e.to_string()
                }),
            )
            .map_err(|e| format!("Failed to emit extraction error: {}", e))?;

            {
                let active_downloads = app.state::<RwLock<ActiveDownloads>>();
                let mut downloads = active_downloads
                    .write()
                    .map_err(|e| format!("Failed to lock active downloads: {}", e))?;
                if let Some(download) = downloads.downloads.get_mut(&download_id) {
                    download.extraction_status = Some("failed".to_string());
                    download.extraction_progress = Some(0.0);
                }
                save_active_downloads_to_file(&app, &downloads)?;
            }

            Err(e.to_string())
        }
    }
}

#[tauri::command]
async fn check_path_exists(path: String) -> Result<bool, String> {
    Ok(std::path::Path::new(&path).exists())
}

#[tauri::command]
async fn select_game_executable(app: AppHandle, _game_id: String) -> Result<String, String> {
    let dialog = app
        .dialog()
        .file()
        .add_filter("Executable Files", &["exe", "py", "sh"]);
    let result = dialog.blocking_pick_file();

    match result {
        Some(file_path) => {
            // Convert the FilePath to a String
            let path_str = file_path.to_string();
            Ok(path_str)
        }
        None => Err("No file selected".to_string()),
    }
}

#[tauri::command]
async fn launch_game(
    _app: AppHandle,
    game_id: String,
    launch_config: Option<LaunchConfig>, // เปลี่ยนเป็น Option
    state: State<'_, Mutex<AppState>>,
) -> Result<(), String> {
    let app_state = state
        .lock()
        .map_err(|e| format!("Failed to lock state: {}", e))?;

    // ดึง launch_config จาก AppState หากมี
    let stored_launch_config = app_state.games.as_ref().and_then(|games| {
        games
            .iter()
            .find(|g| g.id == game_id)
            .and_then(|game| game.launch_config.clone())
    });

    // ใช้ launch_config จากพารามิเตอร์ถ้าไม่มีใน AppState
    let launch_config = stored_launch_config
        .or(launch_config)
        .ok_or("No launch configuration provided or found")?;

    let executable_path = &launch_config.executable_path;
    let path_obj = Path::new(executable_path);

    if !path_obj.exists() {
        return Err("Executable does not exist".to_string());
    }

    let launch_method = &launch_config.launch_method;
    match launch_method.as_str() {
        "direct" => {
            #[cfg(target_os = "windows")]
            {
                StdCommand::new(executable_path)
                    .spawn()
                    .map_err(|e| format!("Failed to launch: {}", e))?;
            }
            #[cfg(not(target_os = "windows"))]
            {
                return Err("Direct launch only supported on Windows".to_string());
            }
        }
        "python" => {
            let python_check = StdCommand::new("python3").arg("--version").output();
            if python_check.is_err() {
                return Err("Python3 is not installed".to_string());
            }
            StdCommand::new("python3")
                .arg(executable_path)
                .spawn()
                .map_err(|e| format!("Failed to launch Python script: {}", e))?;
        }
        "wine" => {
            #[cfg(not(target_os = "windows"))]
            {
                let wine_check = StdCommand::new("wine").arg("--version").output();
                if wine_check.is_err() {
                    return Err("Wine is not installed".to_string());
                }
                StdCommand::new("wine")
                    .arg(executable_path)
                    .spawn()
                    .map_err(|e| format!("Failed to launch with Wine: {}", e))?;
            }
            #[cfg(target_os = "windows")]
            {
                return Err("Wine not needed on Windows".to_string());
            }
        }
        "custom" => {
            if let Some(cmd) = &launch_config.custom_command {
                StdCommand::new("sh")
                    .arg("-c")
                    .arg(cmd)
                    .spawn()
                    .map_err(|e| format!("Failed to launch custom command: {}", e))?;
            } else {
                return Err("Custom command not provided".to_string());
            }
        }
        _ => return Err("Invalid launch method".to_string()),
    }

    Ok(())
}

#[tauri::command]
async fn extract_icon(app: AppHandle, executable_path: String) -> Result<String, String> {
    let path_obj = Path::new(&executable_path);
    if !path_obj.exists() {
        return Err("Executable does not exist".to_string());
    }

    let icon_path = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("Failed to get app data dir: {}", e))?
        .join("icons")
        .join(format!("{}.png", Uuid::new_v4()));

    fs::create_dir_all(icon_path.parent().unwrap())
        .map_err(|e| format!("Failed to create icons dir: {}", e))?;

    #[cfg(target_os = "windows")]
    {
        if executable_path.to_lowercase().ends_with(".exe") {
            let file =
                File::open(&executable_path).map_err(|e| format!("Failed to open file: {}", e))?;
            let icon_dir_result = IconDir::read(file);
            let icon_image = match icon_dir_result {
                Ok(icon_dir) => {
                    let entry = icon_dir
                        .entries()
                        .first()
                        .ok_or("No icons found in executable")?;
                    entry
                        .decode()
                        .map_err(|e| format!("Failed to decode icon: {}", e))?
                }
                Err(e) => {
                    println!("Icon extraction failed: {}. Using default icon.", e);
                    // Use a default icon
                    let default_icon = app
                        .path()
                        .resource_dir()
                        .map_err(|e| format!("Failed to get resource dir: {}", e))?
                        .join("default_icon.png");
                    if default_icon.exists() {
                        fs::copy(&default_icon, &icon_path)
                            .map_err(|e| format!("Failed to copy default icon: {}", e))?;
                        return Ok(icon_path
                            .to_str()
                            .ok_or("Failed to convert path to string")?
                            .to_string());
                    } else {
                        return Err("Default icon not found and icon extraction failed".to_string());
                    }
                }
            };

            let rgba = icon_image.rgba_data();
            let img =
                image::RgbaImage::from_raw(icon_image.width(), icon_image.height(), rgba.to_vec())
                    .ok_or("Failed to create RGBA image")?;
            let dynamic_img = DynamicImage::ImageRgba8(img);

            dynamic_img
                .save_with_format(&icon_path, image::ImageFormat::Png)
                .map_err(|e| format!("Failed to save icon: {}", e))?;
        } else {
            return Err("Only .exe files supported for icon extraction on Windows".to_string());
        }
    }

    #[cfg(not(target_os = "windows"))]
    {
        let default_icon = app
            .path()
            .resource_dir()
            .map_err(|e| format!("Failed to get resource dir: {}", e))?
            .join("default_icon.png");
        if default_icon.exists() {
            fs::copy(&default_icon, &icon_path)
                .map_err(|e| format!("Failed to copy default icon: {}", e))?;
        } else {
            return Err("Default icon not found".to_string());
        }
    }

    Ok(icon_path
        .to_str()
        .ok_or("Failed to convert path to string")?
        .to_string())
}

#[tauri::command]
async fn save_launch_config(
    game_id: String,
    launch_config: LaunchConfig,
    icon_path: Option<String>,
    state: State<'_, Mutex<AppState>>,
    app: AppHandle,
) -> Result<(), String> {
    let mut app_state = state
        .lock()
        .map_err(|e| format!("Failed to lock state: {}", e))?;
    if app_state.games.is_none() {
        app_state.games = Some(Vec::new());
        println!("Initialized empty games list");
    }
    if let Some(games) = app_state.games.as_mut() {
        if let Some(game) = games.iter_mut().find(|g| g.id == game_id) {
            game.launch_config = Some(launch_config.clone());
            game.icon_path = icon_path.clone();
            println!("Updated launch config for game_id: {}", game_id);
        } else {
            println!("Game with id {} not found", game_id);
            return Err(format!("Game with id {} not found", game_id));
        }
    }
    save_state_to_file(&app, &app_state)?;
    println!("Launch config saved to file for game_id: {}", game_id);
    Ok(())
}

#[tauri::command]
fn echo_test(message: String) -> String {
    println!("Echo test received: {}", message);
    format!("Echo reply: {}", message)
}

#[tauri::command]
fn verify_config_exists(app: AppHandle) -> Result<String, String> {
    state::verify_config_file(&app).map(|_| "Config file verified successfully".to_string())
}

#[tauri::command]
fn show_download_notification(
    app: AppHandle,
    title: String,
    message: String,
) -> Result<(), String> {
    app.notification()
        .builder()
        .title(title)
        .body(message)
        .show()
        .map_err(|e| format!("Failed to show notification: {}", e))?;
    Ok(())
}

#[tauri::command]
fn set_token(
    token: String,
    state: State<'_, Mutex<AppState>>,
    app: AppHandle,
) -> Result<(), String> {
    let mut app_state = state
        .lock()
        .map_err(|e| format!("Failed to lock state: {}", e))?;
    app_state.token = Some(token);
    save_state_to_file(&app, &app_state)?;
    Ok(())
}

#[tauri::command]
fn get_token(state: State<'_, Mutex<AppState>>) -> Result<Option<String>, String> {
    let app_state = state
        .lock()
        .map_err(|e| format!("Failed to lock state: {}", e))?;
    Ok(app_state.token.clone())
}

#[tauri::command]
fn set_cloudinary_config(
    cloud_name: String,
    api_key: String,
    api_secret: String,
    state: State<'_, Mutex<AppState>>,
    app: AppHandle,
) -> Result<(), String> {
    let mut app_state = state
        .lock()
        .map_err(|e| format!("Failed to lock state: {}", e))?;
    let config = CloudinaryConfig {
        cloud_name,
        api_key,
        api_secret,
    };
    app_state.cloudinary = Some(config);
    save_state_to_file(&app, &app_state)?;
    Ok(())
}

#[tauri::command]
fn get_cloudinary_config(
    state: State<'_, Mutex<AppState>>,
) -> Result<Option<CloudinaryConfig>, String> {
    let app_state = state
        .lock()
        .map_err(|e| format!("Failed to lock state: {}", e))?;
    Ok(app_state.cloudinary.clone())
}

#[tauri::command]
fn save_all_settings(
    token: String,
    cloudinary_config: state::CloudinaryConfig,
    state: State<'_, Mutex<AppState>>,
    app: AppHandle,
) -> Result<(), String> {
    let mut app_state = state
        .lock()
        .map_err(|e| format!("Failed to lock state: {}", e))?;
    app_state.token = Some(token);
    app_state.cloudinary = Some(CloudinaryConfig {
        cloud_name: cloudinary_config.cloud_name,
        api_key: cloudinary_config.api_key,
        api_secret: cloudinary_config.api_secret,
    });
    save_state_to_file(&app, &app_state)?;
    Ok(())
}

#[tauri::command]
async fn upload_to_cloudinary(
    file_path: String,
    public_id: Option<String>,
    state: State<'_, Mutex<AppState>>,
) -> Result<String, String> {
    let cloudinary_config = {
        let app_state = state
            .lock()
            .map_err(|e| format!("Failed to lock state: {}", e))?;
        app_state
            .cloudinary
            .as_ref()
            .ok_or("Cloudinary config not set")?
            .clone()
    };
    cloudinary::upload_to_cloudinary(file_path, public_id, &cloudinary_config).await
}

#[tauri::command]
fn open_directory(path: String, _app: AppHandle) -> Result<(), String> {
    let path_obj = std::path::Path::new(&path);
    if !path_obj.exists() {
        return Err("Directory does not exist".to_string());
    }

    #[cfg(target_os = "windows")]
    {
        StdCommand::new("explorer")
            .arg(path)
            .spawn()
            .map_err(|e| format!("Failed to open directory: {}", e))?;
    }

    #[cfg(target_os = "macos")]
    {
        StdCommand::new("open")
            .arg(path)
            .spawn()
            .map_err(|e| format!("Failed to open directory: {}", e))?;
    }

    #[cfg(target_os = "linux")]
    {
        StdCommand::new("xdg-open")
            .arg(path)
            .spawn()
            .map_err(|e| format!("Failed to open directory: {}", e))?;
    }

    Ok(())
}

#[tauri::command]
async fn fetch_article_by_slug(
    slug: String,
    token: Option<String>,
) -> Result<ArticleResponse, String> {
    state::fetch_article_by_slug(slug, token).await
}

#[tauri::command]
fn get_download_dir(app: AppHandle) -> Result<String, String> {
    let state = app.state::<Mutex<AppState>>();
    let app_state = state
        .lock()
        .map_err(|e| format!("Failed to lock state: {}", e))?;
    app_state
        .download_dir
        .clone()
        .ok_or_else(|| "Download directory not set".to_string())
}

#[tauri::command]
fn set_download_dir(
    dir: String,
    state: State<'_, Mutex<AppState>>,
    app: AppHandle,
) -> Result<(), String> {
    let mut app_state = state
        .lock()
        .map_err(|e| format!("Failed to lock state: {}", e))?;
    app_state.download_dir = Some(dir.clone());
    save_state_to_file(&app, &app_state)?;
    println!("Download directory set to: {}", dir);
    Ok(())
}

fn ensure_webview2_runtime(app: &tauri::AppHandle) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        // ตรวจสอบว่า WebView2 runtime ติดตั้งอยู่หรือไม่
        let output = StdCommand::new("reg")
            .args(&["query", "HKLM\\SOFTWARE\\Microsoft\\EdgeUpdate\\Clients"])
            .output()
            .map_err(|e| format!("Failed to check WebView2 runtime: {}", e))?;

        if output.status.success() {
            println!("WebView2 runtime is already installed");
            return Ok(());
        }

        // กำหนดพาธที่คาดว่า bootstrapper จะอยู่
        let paths_to_check = vec![
            app.path()
                .resource_dir()
                .map_err(|e| format!("Failed to get resource dir: {}", e))?
                .join("binaries")
                .join("Release")
                .join("WebView2-x86_64-pc-windows-msvc.exe"),
        ];

        for path in paths_to_check {
            println!("Checking bootstrapper at: {:?}", path);
            if path.exists() {
                println!("Found bootstrapper at: {:?}", path);
                let path_str = path.to_str().ok_or("Failed to convert path to string")?;

                app.shell()
                    .command(path_str)
                    .args(&["/silent", "/install"])
                    .spawn()
                    .map_err(|e| format!("Failed to install WebView2 runtime: {}", e))?;

                return Ok(());
            }
        }

        return Err("WebView2 bootstrapper not found in expected location".to_string());
    }

    #[cfg(not(target_os = "windows"))]
    {
        Ok(())
    }
}

#[tauri::command]
async fn webview2_response(
    response: serde_json::Value,
    app: AppHandle,
    active_downloads: State<'_, RwLock<ActiveDownloads>>,
) -> Result<(), String> {
    println!("Received WebView2 response: {:?}", response);

    let download_id = match response.get("downloadId").and_then(|id| id.as_str()) {
        Some(id) => id,
        None => {
            println!("Warning: Response missing downloadId: {:?}", response);
            let _ = show_download_notification(
                app.clone(),
                "Download Error".to_string(),
                "A download failed: missing download identifier".to_string(),
            );
            return Err("Missing downloadId in WebView2 response".to_string());
        }
    };

    let status = response
        .get("status")
        .and_then(|s| s.as_str())
        .unwrap_or("unknown");

    let mut downloads = active_downloads
        .write()
        .map_err(|e| format!("Failed to lock active downloads: {}", e))?;

    let download_started = response
        .get("downloadStarted")
        .and_then(|s| s.as_bool())
        .unwrap_or(false);

    if download_started && !downloads.downloads.contains_key(download_id) {
        let filename = response
            .get("filename")
            .and_then(|f| f.as_str())
            .unwrap_or("Unknown file")
            .to_string();

        println!(
            "Registering new download from start notification: id={}, filename={}",
            download_id, filename
        );

        let download_info = DownloadInfo {
            id: download_id.to_string(),
            filename: filename.clone(),
            url: "".to_string(),
            progress: 0.1,
            status: "downloading".to_string(),
            path: None,
            error: None,
            provider: Some("webview2".to_string()),
            downloaded_at: None,
            extracted: false,
            extracted_path: None,
            extraction_status: Some("idle".to_string()), // Default to "idle"
            extraction_progress: Some(0.0),              // Default to 0.0
        };

        downloads
            .downloads
            .insert(download_id.to_string(), download_info);

        let _ = app.emit(
            "download-progress",
            &serde_json::json!({
                "id": download_id,
                "progress": 0.1,
                "filename": filename
            }),
        );
    }

    if let Some(download) = downloads.downloads.get_mut(download_id) {
        match status {
            "success" => {
                if let Some(path) = response.get("path").and_then(|p| p.as_str()) {
                    download.status = "completed".to_string();
                    download.progress = 100.0;
                    download.path = Some(path.to_string());
                    download.downloaded_at = Some(chrono::Utc::now().to_rfc3339());

                    if let Some(filename) = response.get("filename").and_then(|f| f.as_str()) {
                        download.filename = filename.to_string();
                    }

                    println!("Download completed: id={}, path={}", download_id, path);
                    let _ = app.emit(
                        "download-complete",
                        &serde_json::json!({
                            "id": download_id,
                            "filename": download.filename,
                            "path": path
                        }),
                    );
                    let _ = show_download_notification(
                        app.clone(),
                        "Download Complete".to_string(),
                        format!("Downloaded: {}", download.filename),
                    );
                } else {
                    download.status = "downloading".to_string();

                    if download.progress < 10.0 {
                        download.progress = 10.0;
                    }

                    println!("Download started: id={}", download_id);
                    let _ = app.emit(
                        "download-progress",
                        &serde_json::json!({
                            "id": download_id,
                            "progress": download.progress
                        }),
                    );
                }
            }
            "error" => {
                download.status = "failed".to_string();
                download.error = response
                    .get("message")
                    .and_then(|m| m.as_str())
                    .map(|s| s.to_string());
                println!(
                    "Download error: id={}, error={:?}",
                    download_id, download.error
                );
                let _ = app.emit(
                    "download-error",
                    &serde_json::json!({
                        "id": download_id,
                        "error": download.error
                    }),
                );
                let _ = show_download_notification(
                    app.clone(),
                    "Download Failed".to_string(),
                    format!("Failed to download: {}", download.filename),
                );
            }
            "progress" => {
                if let Some(progress) = response.get("progress").and_then(|p| p.as_f64()) {
                    if progress as f32 > download.progress || download_started {
                        download.progress = progress as f32;
                        download.status = "downloading".to_string();
                        println!(
                            "Download progress: id={}, progress={}",
                            download_id, progress
                        );
                        let _ = app.emit(
                            "download-progress",
                            &serde_json::json!({
                                "id": download_id,
                                "progress": progress
                            }),
                        );
                    }
                }
            }
            _ => {
                println!("Unknown status received: {}", status);
                download.status = "unknown".to_string();
                download.error = Some(format!("Unknown status: {}", status));

                let _ = app.emit(
                    "download-error",
                    &serde_json::json!({
                        "id": download_id,
                        "error": download.error
                    }),
                );
            }
        }
    } else {
        println!("No download found for id: {}", download_id);
        if download_id.len() > 0
            && (status == "success" || status == "error" || status == "progress")
        {
            let filename = response
                .get("filename")
                .and_then(|f| f.as_str())
                .unwrap_or("Unknown file");

            let progress = if status == "progress" {
                response
                    .get("progress")
                    .and_then(|p| p.as_f64())
                    .unwrap_or(0.0) as f32
            } else {
                0.0
            };

            let download_info = DownloadInfo {
                id: download_id.to_string(),
                filename: filename.to_string(),
                url: "".to_string(),
                progress,
                status: match status {
                    "success" => {
                        if response.get("path").is_some() {
                            "completed".to_string()
                        } else {
                            "downloading".to_string()
                        }
                    }
                    "progress" => "downloading".to_string(),
                    "error" => "failed".to_string(),
                    _ => "unknown".to_string(),
                },
                path: response
                    .get("path")
                    .and_then(|p| p.as_str())
                    .map(|s| s.to_string()),
                error: if status == "error" {
                    response
                        .get("message")
                        .and_then(|m| m.as_str())
                        .map(|s| s.to_string())
                } else {
                    None
                },
                provider: Some("webview2".to_string()),
                downloaded_at: None,
                extracted: false,
                extracted_path: None,
                extraction_status: Some("idle".to_string()), // Default to "idle"
                extraction_progress: Some(0.0),              // Default to 0.0
            };

            downloads
                .downloads
                .insert(download_id.to_string(), download_info);
            println!("Registered new download with id: {}", download_id);

            match status {
                "progress" => {
                    let _ = app.emit(
                        "download-progress",
                        &serde_json::json!({
                            "id": download_id,
                            "progress": progress
                        }),
                    );
                }
                "success" => {
                    if let Some(path) = response.get("path").and_then(|p| p.as_str()) {
                        let _ = app.emit(
                            "download-complete",
                            &serde_json::json!({
                                "id": download_id,
                                "filename": filename,
                                "path": path
                            }),
                        );
                    }
                }
                _ => {}
            }
        }
    }

    if matches!(status, "success" | "error") {
        downloads.tokens.remove(download_id);
    }

    save_active_downloads_to_file(&app, &downloads)?;
    Ok(())
}

#[tauri::command]
fn get_active_downloads(
    active_downloads: State<'_, RwLock<ActiveDownloads>>,
) -> Result<Vec<DownloadInfo>, String> {
    let downloads = active_downloads
        .read()
        .map_err(|e| format!("Failed to read active downloads: {}", e))?;
    Ok(downloads.downloads.values().cloned().collect())
}

#[tauri::command]
fn open_file(path: String, _app: AppHandle) -> Result<(), String> {
    let path_obj = std::path::Path::new(&path);
    if !path_obj.exists() {
        return Err("File does not exist".to_string());
    }

    #[cfg(target_os = "windows")]
    {
        StdCommand::new("cmd")
            .args(["/c", "start", "", path_obj.to_str().unwrap()])
            .spawn()
            .map_err(|e| format!("Failed to open file: {}", e))?;
    }

    #[cfg(target_os = "macos")]
    {
        StdCommand::new("open")
            .arg(path)
            .spawn()
            .map_err(|e| format!("Failed to open file: {}", e))?;
    }

    #[cfg(target_os = "linux")]
    {
        StdCommand::new("xdg-open")
            .arg(path)
            .spawn()
            .map_err(|e| format!("Failed to open file: {}", e))?;
    }

    Ok(())
}

#[tauri::command]
async fn cancel_active_download(download_id: String, app: AppHandle) -> Result<(), String> {
    println!("Cancellation requested for download: {}", download_id);
    let active_downloads = app.state::<RwLock<ActiveDownloads>>();
    let mut downloads = active_downloads
        .write()
        .map_err(|e| format!("Failed to lock active downloads: {}", e))?;

    if let Some(token) = downloads.tokens.remove(&download_id) {
        token.cancel();
        if let Some(download) = downloads.downloads.get_mut(&download_id) {
            download.status = "cancelled".to_string();
            download.progress = 0.0;
            download.error = Some("Download cancelled by user".to_string());
        }

        let binary_path = app
            .path()
            .resource_dir()
            .map_err(|e| format!("Failed to get resource dir: {}", e))?
            .join("binaries")
            .join("Release")
            .join("ConsoleApp2.exe-x86_64-pc-windows-msvc.exe");

        if !binary_path.exists() {
            return Err("WebView2 binary not found".to_string());
        }

        let message = serde_json::json!({
            "action": "cancelDownload",
            "downloadId": download_id
        });
        let message_str = message.to_string();

        app.shell()
            .command(binary_path.to_str().ok_or("Invalid binary path")?)
            .arg(&message_str)
            .spawn()
            .map_err(|e| format!("Failed to send cancel command: {}", e))?;

        app.emit(
            "cancel-download",
            &serde_json::json!({ "download_id": download_id }),
        )
        .map_err(|e| format!("Failed to emit cancel-download event: {}", e))?;

        show_download_notification(
            app.clone(),
            "Download Cancelled".to_string(),
            format!("Download {} was cancelled", download_id),
        )?;

        save_active_downloads_to_file(&app, &downloads)?;
        println!("Download {} cancelled successfully", download_id);
        Ok(())
    } else {
        Err(format!("No active download found for id: {}", download_id))
    }
}

#[tauri::command]
async fn remove_file(path: String) -> Result<(), String> {
    fs::remove_file(&path).map_err(|e| format!("Failed to remove file: {}", e))?;
    Ok(())
}

#[tauri::command]
fn register_manual_download(
    download_id: String,
    filename: String,
    path: String,
    active_downloads: State<'_, RwLock<ActiveDownloads>>,
    app: AppHandle,
) -> Result<(), String> {
    println!("Manually registered download: {} at {}", download_id, path);
    let mut downloads = active_downloads
        .write()
        .map_err(|e| format!("Failed to write active downloads: {}", e))?;

    // Check if extracted path exists
    let extracted_path = format!("{}_extracted", path);
    let extracted = std::path::Path::new(&extracted_path).exists();

    downloads.downloads.insert(
        download_id.clone(),
        DownloadInfo {
            id: download_id,
            filename,
            url: "".to_string(),
            progress: 100.0,
            status: "completed".to_string(),
            path: Some(path.clone()),
            error: None,
            provider: None,
            downloaded_at: Some(chrono::Utc::now().to_rfc3339()),
            extracted,
            extracted_path: if extracted {
                Some(extracted_path)
            } else {
                None
            },
            extraction_status: Some(if extracted {
                "completed".to_string()
            } else {
                "idle".to_string()
            }), // Reflect extraction status
            extraction_progress: Some(if extracted { 100.0 } else { 0.0 }), // Reflect extraction progress
        },
    );

    save_active_downloads_to_file(&app, &downloads)?;
    Ok(())
}

#[tauri::command]
fn save_games(
    games: Vec<DownloadInfo>,
    state: State<'_, Mutex<AppState>>,
    app: AppHandle,
) -> Result<(), String> {
    let mut app_state = state
        .lock()
        .map_err(|e| format!("Failed to lock state: {}", e))?;

    // ดึง games เดิมจาก app_state เพื่อรักษา launch_config และ icon_path
    let existing_games = app_state.games.clone().unwrap_or_default();

    let converted_games: Vec<DownloadedGameInfo> = games
        .into_iter()
        .map(|game| {
            // ค้นหา game เดิมที่มี id เดียวกัน
            let existing_game = existing_games.iter().find(|g| g.id == game.id);

            DownloadedGameInfo {
                id: game.id,
                filename: game.filename,
                path: game.path.unwrap_or_default(),
                extracted: game.extracted,
                extracted_path: game.extracted_path,
                downloaded_at: game.downloaded_at,
                // รักษา launch_config และ icon_path เดิมถ้ามี
                launch_config: existing_game.and_then(|g| g.launch_config.clone()),
                icon_path: existing_game.and_then(|g| g.icon_path.clone()),
            }
        })
        .collect();

    app_state.games = Some(converted_games);
    save_state_to_file(&app, &app_state)?;
    println!("Games saved successfully to config");
    Ok(())
}

#[tauri::command]
fn get_saved_games(
    state: State<'_, Mutex<AppState>>,
    app: AppHandle,
) -> Result<Vec<DownloadedGameInfo>, String> {
    let mut app_state = state
        .lock()
        .map_err(|e| format!("Failed to lock state: {}", e))?;

    // Get the current games list or initialize an empty one
    let games = app_state.games.clone().unwrap_or_default();

    // Filter out games whose files no longer exist
    let valid_games: Vec<DownloadedGameInfo> = games
        .into_iter()
        .filter(|game| {
            let path_exists = game.path.is_empty() || std::path::Path::new(&game.path).exists();
            let extracted_path_exists = game.extracted_path.as_ref().map_or(true, |path| {
                path.is_empty() || std::path::Path::new(path).exists()
            });

            // Keep the game if either its path or extracted path exists
            let keep = path_exists || extracted_path_exists;
            if !keep {
                println!(
                    "Removing game {} from state as its files no longer exist",
                    game.id
                );
            }
            keep
        })
        .collect();

    // Update the state if any games were removed
    if valid_games.len() != app_state.games.as_ref().map_or(0, |g| g.len()) {
        app_state.games = Some(valid_games.clone());
        save_state_to_file(&app, &app_state)?;
        println!("Updated state with valid games");
    }

    Ok(valid_games)
}

#[tauri::command]
async fn start_webview2_download(
    url: String,
    filename: String,
    download_id: String,
    app: AppHandle,
    active_downloads: State<'_, RwLock<ActiveDownloads>>,
) -> Result<(), String> {
    println!(
        "Starting WebView2 download: id={}, url={}, filename={}",
        download_id, url, filename
    );

    // ตรวจสอบ WebView2 runtime ก่อน
    #[cfg(target_os = "windows")]
    {
        if let Err(e) = ensure_webview2_runtime(&app) {
            // แจ้งเตือนผู้ใช้หาก WebView2 runtime หรือ bootstrapper ไม่พบ
            app.notification()
                .builder()
                .title("WebView2 Required")
                .body("Please install Microsoft WebView2 Runtime to use this feature.")
                .show()
                .map_err(|e| format!("Failed to show notification: {}", e))?;
            return Err(format!("WebView2 runtime not available: {}", e));
        }
    }

    let save_folder = get_download_dir(app.clone())?;
    println!("Save folder: {}", save_folder);

    if !std::path::Path::new(&save_folder).exists() {
        if let Err(e) = std::fs::create_dir_all(&save_folder) {
            println!("Failed to create save folder: {}", e);
            return Err(format!("Failed to create save folder: {}", e));
        }
    }

    let token = CancellationToken::new();
    {
        let mut downloads = active_downloads
            .write()
            .map_err(|e| format!("Failed to lock active downloads: {}", e))?;
        downloads.downloads.insert(
            download_id.clone(),
            DownloadInfo {
                id: download_id.clone(),
                filename: filename.clone(),
                url: url.clone(),
                progress: 0.0,
                status: "starting".to_string(),
                path: None,
                error: None,
                provider: Some("webview2".to_string()),
                downloaded_at: None,
                extracted: false,
                extracted_path: None,
                extraction_status: Some("idle".to_string()), // Default to "idle"
                extraction_progress: Some(0.0),              // Default to 0.0
            },
        );
        downloads.tokens.insert(download_id.clone(), token.clone());

        save_active_downloads_to_file(&app, &downloads)?;
    }

    let message = serde_json::json!({
        "action": "setDownload",
        "url": url,
        "saveFolder": save_folder,
        "downloadId": download_id,
        "filename": filename
    });
    let message_str = message.to_string();
    println!("Sending message to WebView2: {}", message_str);

    let mut binary_path = app
        .path()
        .resource_dir()
        .map_err(|e| format!("Failed to get resource dir: {}", e))?
        .join("binaries")
        .join("Release")
        .join("WebView2-x86_64-pc-windows-msvc.exe");

    if !binary_path.exists() {
        println!("Binary not found at: {:?}", binary_path);

        let paths_to_check = vec![std::path::PathBuf::from(
            "WebView2-x86_64-pc-windows-msvc.exe",
        )];

        let mut found = false;
        for path in paths_to_check {
            if path.exists() {
                println!("Found binary at alternate location: {:?}", path);
                binary_path = path;
                found = true;
                break;
            }
        }

        if !found {
            return Err(format!(
                "WebView2 binary not found in any expected location"
            ));
        }
    }

    println!("Starting WebView2 binary at: {:?}", binary_path);

    let (mut rx, _child) = app
        .shell()
        .command(binary_path.to_str().ok_or("Invalid binary path")?)
        .arg(&message_str)
        .spawn()
        .map_err(|e| format!("Failed to spawn WebView2 process: {}", e))?;

    let app_clone = app.clone();
    let download_id_clone = download_id.clone();

    tauri::async_runtime::spawn(async move {
        while let Some(event) = rx.recv().await {
            match event {
                CommandEvent::Stdout(line) => {
                    let output = String::from_utf8_lossy(&line).to_string();
                    println!("WebView2 stdout: {}", output);
                    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&output) {
                        println!("Parsed WebView2 response: {:?}", json);
                        let active_downloads = app_clone.state::<RwLock<ActiveDownloads>>();
                        if let Err(e) =
                            webview2_response(json, app_clone.clone(), active_downloads).await
                        {
                            println!("Error processing WebView2 response: {}", e);
                        }
                    }
                }
                CommandEvent::Stderr(line) => {
                    let output = String::from_utf8_lossy(&line).to_string();
                    println!("WebView2 stderr: {}", output);
                    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&output) {
                        println!("Parsed WebView2 stderr response: {:?}", json);
                        let active_downloads = app_clone.state::<RwLock<ActiveDownloads>>();
                        if let Err(e) =
                            webview2_response(json, app_clone.clone(), active_downloads).await
                        {
                            println!("Error processing WebView2 stderr response: {}", e);
                        }
                    }
                }
                CommandEvent::Error(e) => {
                    println!("WebView2 process error: {}", e);
                    let active_downloads = app_clone.state::<RwLock<ActiveDownloads>>();
                    let error_json = serde_json::json!({
                        "status": "error",
                        "message": format!("WebView2 process error: {}", e),
                        "downloadId": download_id_clone
                    });
                    let _ =
                        webview2_response(error_json, app_clone.clone(), active_downloads).await;
                }
                CommandEvent::Terminated(code) => {
                    println!("WebView2 process terminated with code: {:?}", code);
                    if code.code != Some(0) {
                        let should_report_error = {
                            let active_downloads = app_clone.state::<RwLock<ActiveDownloads>>();
                            match active_downloads.read() {
                                Ok(downloads) => {
                                    if let Some(download) =
                                        downloads.downloads.get(&download_id_clone)
                                    {
                                        download.status != "completed"
                                            && download.status != "failed"
                                    } else {
                                        false
                                    }
                                }
                                Err(e) => {
                                    println!("Failed to lock active downloads: {}", e);
                                    false
                                }
                            }
                        };

                        if should_report_error {
                            let error_json = serde_json::json!({
                                "status": "error",
                                "message": format!("WebView2 process terminated unexpectedly with code: {:?}", code),
                                "downloadId": download_id_clone
                            });

                            let active_downloads = app_clone.state::<RwLock<ActiveDownloads>>();
                            if let Err(e) =
                                webview2_response(error_json, app_clone.clone(), active_downloads)
                                    .await
                            {
                                println!("Error reporting WebView2 termination: {}", e);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    });

    app.emit(
        "start-webview2-download",
        &serde_json::json!({
            "url": url,
            "filename": filename,
            "downloadId": download_id
        }),
    )
    .map_err(|e| format!("Failed to emit start-webview2-download event: {}", e))?;

    println!("WebView2 download initiated for id: {}", download_id);
    Ok(())
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_fs::init())
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_opener::init())
        .setup(move |app| {
            let app_handle = app.handle().clone();



            let initial_state = match state::load_state_from_file(&app_handle) {
                Ok(loaded_state) => {
                    println!("Loaded state successfully: {:?}", loaded_state);
                    loaded_state
                }
                Err(e) => {
                    println!("Failed to load state: {}. Using default state.", e);
                    let default_state = AppState::default();
                    if let Err(save_err) = state::save_state_to_file(&app_handle, &default_state) {
                        println!("Failed to save default state: {}", save_err);
                    }
                    default_state
                }
            };

            let mut initial_downloads = match state::load_active_downloads_from_file(&app_handle) {
                Ok(loaded_downloads) => {
                    println!(
                        "Loaded active downloads successfully: {:?}",
                        loaded_downloads
                    );
                    loaded_downloads
                }
                Err(e) => {
                    println!(
                        "Failed to load active downloads: {}. Using default downloads.",
                        e
                    );
                    ActiveDownloads::default()
                }
            };
            cleanup_active_downloads(&mut initial_downloads);

            app.manage(Mutex::new(initial_state));
            app.manage(RwLock::new(initial_downloads));

            if let Ok(mut app_state) = app.state::<Mutex<AppState>>().lock() {
                if app_state.download_dir.is_none() {
                    app_state.download_dir = state::get_default_download_dir(&app_handle);
                    if let Err(e) = state::save_state_to_file(&app_handle, &app_state) {
                        println!("Failed to save default download directory: {}", e);
                    }
                }
            }

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            echo_test,
            set_token,
            get_token,
            set_cloudinary_config,
            get_cloudinary_config,
            save_all_settings,
            upload_to_cloudinary,
            fetch_article_by_slug,
            get_download_dir,
            set_download_dir,
            verify_config_exists,
            show_download_notification,
            open_directory,
            cancel_active_download,
            get_active_downloads,
            open_file,
            remove_file,
            unarchive_file,
            check_path_exists,
            save_games,
            get_saved_games,
            register_manual_download,
            start_webview2_download,
            webview2_response,
            is_directory,
            select_game_executable,
            launch_game,
            extract_icon,
            save_launch_config
        ])
        .on_window_event(|app, event| {
            if let tauri::WindowEvent::CloseRequested { .. } = event {
                let app_handle = app.app_handle().clone();
                if let Ok(app_state) = app.state::<Mutex<AppState>>().lock() {
                    if let Err(e) = state::save_state_to_file(&app_handle, &app_state) {
                        println!("Failed to save state on close: {}", e);
                    } else {
                        println!("State saved successfully on close");
                    }
                } else {
                    println!("Failed to lock state on close");
                }
                if let Ok(active_downloads) = app.state::<RwLock<ActiveDownloads>>().read() {
                    if let Err(e) =
                        state::save_active_downloads_to_file(&app_handle, &active_downloads)
                    {
                        println!("Failed to save active downloads on close: {}", e);
                    } else {
                        println!("Active downloads saved successfully on close");
                    }
                } else {
                    println!("Failed to lock active downloads on close");
                }
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
