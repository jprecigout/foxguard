use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub server: ServerConfig,
    pub camera: CameraConfig,
    pub detection: DetectionConfig,
    pub email: EmailConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    pub api_token: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CameraConfig {
    #[serde(default)]
    pub device_index: usize,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DetectionConfig {
    #[serde(default)]
    pub enabled: bool,
    pub model_path: String,
    pub input_size: u32,
    pub confidence_threshold: f32,
    pub email_cooldown_secs: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EmailConfig {
    #[serde(default)]
    pub enabled: bool,
    pub smtp_server: String,
    pub smtp_user: String,
    pub smtp_password: String,
    pub from_address: String,
    pub to_address: String,
}

impl Config {
    pub fn load(path: &str) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let config: Config = toml::from_str(&content)?;
        Ok(config)
    }
}
