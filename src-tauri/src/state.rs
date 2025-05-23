use crate::ActiveDownloads;
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::Write;
use std::path::PathBuf;
use tauri::{AppHandle, Manager};

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CloudinaryConfig {
    pub cloud_name: String,
    pub api_key: String,
    pub api_secret: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AppState {
    pub token: Option<String>,
    pub cloudinary: Option<CloudinaryConfig>,
    pub download_dir: Option<String>,
    pub games: Option<Vec<DownloadedGameInfo>>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct LaunchConfig {
    #[serde(rename = "executablePath")]
    pub executable_path: String,

    #[serde(rename = "launchMethod")]
    pub launch_method: String,

    #[serde(rename = "customCommand")]
    pub custom_command: Option<String>,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct DownloadedGameInfo {
    pub id: String,
    pub filename: String,
    pub path: String,
    pub extracted: bool,
    pub extracted_path: Option<String>,
    pub downloaded_at: Option<String>,
    pub launch_config: Option<LaunchConfig>, // New field
    pub icon_path: Option<String>,           // New field
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            token: None,
            cloudinary: None,
            download_dir: None,
            games: None,
        }
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub struct ArticleResponse {
    pub slug: String,
    pub title: String,
    pub content: String,
}

pub async fn fetch_article_by_slug(
    slug: String,
    token: Option<String>,
) -> Result<ArticleResponse, String> {
    let api_url = format!("https://api.chanomhub.online/articles/{}", slug);

    let client = reqwest::Client::new();
    let mut request = client.get(&api_url);

    if let Some(token) = token {
        request = request.header("Authorization", format!("Bearer {}", token));
    }

    let response = request
        .send()
        .await
        .map_err(|e| format!("Failed to send request: {}", e))?;

    if response.status().is_success() {
        let article: ArticleResponse = response
            .json()
            .await
            .map_err(|e| format!("Failed to parse response: {}", e))?;
        Ok(article)
    } else {
        Err(format!("API request failed: {}", response.status()))
    }
}

pub fn verify_config_file(app: &AppHandle) -> Result<(), String> {
    let config_dir = get_config_dir(app).ok_or("Could not get config directory")?;
    let config_path = config_dir.join("config.json");

    println!("Verifying config file at: {:?}", config_path);

    if config_path.exists() {
        println!("✅ Config file exists");

        match fs::read_to_string(&config_path) {
            Ok(contents) => {
                println!("✅ Successfully read file contents");
                println!("File size: {} bytes", contents.len());

                match serde_json::from_str::<AppState>(&contents) {
                    Ok(state) => {
                        println!("✅ Successfully parsed JSON");
                        println!("State: {:?}", state);

                        if state.token.is_some() {
                            println!("✅ Token is set: {}", state.token.unwrap());
                        } else {
                            println!("⚠️ Token is not set");
                        }

                        if state.cloudinary.is_some() {
                            println!("✅ Cloudinary config is set");
                        } else {
                            println!("⚠️ Cloudinary config is not set");
                        }
                    }
                    Err(e) => {
                        println!("❌ Failed to parse JSON: {}", e);
                        println!("File contents: {}", contents);
                        return Err(format!("Failed to parse config JSON: {}", e));
                    }
                }
            }
            Err(e) => {
                println!("❌ Failed to read file: {}", e);
                return Err(format!("Failed to read config file: {}", e));
            }
        }
    } else {
        println!("❌ Config file does not exist");
        return Err("Config file does not exist".to_string());
    }

    Ok(())
}

pub fn get_config_dir(app: &AppHandle) -> Option<PathBuf> {
    app.path().app_config_dir().ok()
}

pub fn get_default_download_dir(app: &AppHandle) -> Option<String> {
    let resource_dir = app.path().resource_dir().ok()?;
    let download_dir = resource_dir.join("downloads");
    if !download_dir.exists() {
        if let Err(e) = fs::create_dir_all(&download_dir) {
            println!("Failed to create downloads directory: {}", e);
            return None;
        }
    }
    download_dir.to_str().map(|s| s.to_string())
}

pub fn load_state_from_file(app: &AppHandle) -> Result<AppState, String> {
    let config_dir = get_config_dir(app).ok_or("Could not get config directory")?;
    let config_path = config_dir.join("config.json");

    let mut state = if config_path.exists() {
        let contents = fs::read_to_string(&config_path)
            .map_err(|e| format!("Failed to read config file: {}", e))?;
        let state: AppState = serde_json::from_str(&contents)
            .map_err(|e| format!("Failed to parse config file: {}", e))?;
        println!("Loaded state from file: {:?}", state);
        state
    } else {
        AppState::default()
    };

    if state.games.is_none() {
        state.games = Some(Vec::new());
        println!("Initialized empty games list in loaded state");
    }

    state.download_dir = state.download_dir.or_else(|| get_default_download_dir(app));
    save_state_to_file(app, &state)?; // บันทึกเพื่อให้แน่ใจว่ามีไฟล์ config.json
    Ok(state)
}

pub fn save_state_to_file(app: &AppHandle, state: &AppState) -> Result<(), String> {
    let config_dir = get_config_dir(app).ok_or("Could not get config directory")?;
    fs::create_dir_all(&config_dir).map_err(|e| format!("Failed to create config dir: {}", e))?;

    let config_path = config_dir.join("config.json");
    println!("Saving state to: {:?}", config_path);

    let mut file =
        File::create(&config_path).map_err(|e| format!("Failed to create config file: {}", e))?;
    let json = serde_json::to_string_pretty(state)
        .map_err(|e| format!("Failed to serialize state: {}", e))?;
    file.write_all(json.as_bytes())
        .map_err(|e| format!("Failed to write config file: {}", e))?;

    println!("State saved successfully");
    Ok(())
}

pub fn save_active_downloads_to_file(
    app: &AppHandle,
    active_downloads: &ActiveDownloads,
) -> Result<(), String> {
    let config_dir = get_config_dir(app).ok_or("Could not get config directory")?;
    fs::create_dir_all(&config_dir).map_err(|e| format!("Failed to create config dir: {}", e))?;

    let downloads_path = config_dir.join("active_downloads.json");
    println!("Saving active downloads to: {:?}", downloads_path);

    let mut file = File::create(&downloads_path)
        .map_err(|e| format!("Failed to create active downloads file: {}", e))?;
    let json = serde_json::to_string_pretty(active_downloads)
        .map_err(|e| format!("Failed to serialize active downloads: {}", e))?;
    file.write_all(json.as_bytes())
        .map_err(|e| format!("Failed to write active downloads file: {}", e))?;

    println!("Active downloads saved successfully");
    Ok(())
}

pub fn load_active_downloads_from_file(app: &AppHandle) -> Result<ActiveDownloads, String> {
    let config_dir = get_config_dir(app).ok_or("Could not get config directory")?;
    let downloads_path = config_dir.join("active_downloads.json");

    if downloads_path.exists() {
        let contents = fs::read_to_string(&downloads_path)
            .map_err(|e| format!("Failed to read active downloads file: {}", e))?;
        let active_downloads: ActiveDownloads = serde_json::from_str(&contents)
            .map_err(|e| format!("Failed to parse active downloads file: {}", e))?;
        Ok(active_downloads)
    } else {
        Ok(ActiveDownloads::default())
    }
}

pub fn cleanup_active_downloads(active_downloads: &mut ActiveDownloads) {
    for download in active_downloads.downloads.values_mut() {
        if download.status == "starting" || download.status == "downloading" {
            download.status = "failed".to_string();
            download.error = Some("Download interrupted due to application restart".to_string());
        }
    }
}
