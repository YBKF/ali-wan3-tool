//! 本地配置持久化。配置写在 `%APPDATA%\ali-wan3-tool\config.json`。

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// 地域列表：(显示名, region 标识)。model / endpoint / API Key 必须同地域。
pub const REGIONS: &[(&str, &str)] = &[
    ("北京", "cn-beijing"),
    ("新加坡", "ap-southeast-1"),
    ("东京", "ap-northeast-1"),
    ("法兰克福", "eu-central-1"),
    ("弗吉尼亚", "us-east-1"),
    ("中国香港", "cn-hongkong"),
];

/// 模型列表：(显示名, model 标识)。
pub const MODELS: &[(&str, &str)] = &[
    ("标准版", "wan3.0-video"),
    ("高速版", "wan3.0-video-prime"),
];

/// `parameters.resolution`
pub const RESOLUTIONS: &[&str] = &["480P", "720P", "1080P"];

/// `parameters.ratio`
pub const RATIOS: &[&str] = &["adaptive", "21:9", "16:9", "4:3", "1:1", "3:4", "9:16"];

/// 无视频输入时允许的时长范围（秒）
pub const MIN_DURATION: i32 = 2;
pub const MAX_DURATION: i32 = 30;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub api_key: String,
    pub workspace_id: String,
    pub region: String,
    pub output_dir: String,
    pub model: String,
    pub resolution: String,
    pub ratio: String,
    /// 秒；`-1` 表示智能时长
    pub duration: i32,
    pub audio: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            workspace_id: String::new(),
            region: "cn-beijing".to_string(),
            output_dir: default_output_dir(),
            model: "wan3.0-video".to_string(),
            resolution: "1080P".to_string(),
            ratio: "adaptive".to_string(),
            duration: 5,
            audio: false,
        }
    }
}

fn default_output_dir() -> String {
    let home = std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join("Downloads")
        .join("wan3-videos")
        .to_string_lossy()
        .to_string()
}

impl Config {
    pub fn file_path() -> PathBuf {
        let base = std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        base.join("ali-wan3-tool").join("config.json")
    }

    pub fn load() -> Self {
        std::fs::read_to_string(Self::file_path())
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    /// 提示词单独存一个文件，方便直接编辑 / 备份。
    pub fn prompt_file_path() -> PathBuf {
        Self::file_path()
            .parent()
            .map(|dir| dir.join("prompt.txt"))
            .unwrap_or_else(|| PathBuf::from("prompt.txt"))
    }

    pub fn load_prompt() -> String {
        Self::load_prompt_from(&Self::prompt_file_path())
    }

    pub fn save_prompt(text: &str) -> Result<()> {
        Self::save_prompt_to(&Self::prompt_file_path(), text)
    }

    pub fn load_prompt_from(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap_or_default()
    }

    pub fn save_prompt_to(path: &Path, text: &str) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, text)?;
        Ok(())
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::file_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    /// 服务端基地址。填写了业务空间 ID 则使用 MaaS 域名，否则回退经典 DashScope 域名。
    pub fn endpoint_base(&self) -> String {
        let workspace = self.workspace_id.trim();
        if workspace.is_empty() {
            "https://dashscope.aliyuncs.com".to_string()
        } else {
            format!("https://{workspace}.{}.maas.aliyuncs.com", self.region)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_falls_back_to_dashscope_without_workspace() {
        let mut config = Config::default();
        assert_eq!(config.endpoint_base(), "https://dashscope.aliyuncs.com");

        config.workspace_id = "  ws-123  ".to_string();
        config.region = "cn-beijing".to_string();
        assert_eq!(
            config.endpoint_base(),
            "https://ws-123.cn-beijing.maas.aliyuncs.com"
        );
    }

    /// 配置文件缺字段时也要能读出来（向后兼容）。
    #[test]
    fn deserializes_partial_config() {
        let config: Config = serde_json::from_str(r#"{"api_key":"sk-1","duration":-1}"#).unwrap();
        assert_eq!(config.api_key, "sk-1");
        assert_eq!(config.duration, -1);
        assert_eq!(config.region, "cn-beijing");
        assert!(!config.audio);
    }

    /// 提示词单独落盘，能原样读回。
    #[test]
    fn prompt_round_trips_through_its_own_file() {
        let path = std::env::temp_dir()
            .join("ali-wan3-tool-tests")
            .join("prompt.txt");

        Config::save_prompt_to(&path, "让图1里的人物转身，镜头缓慢推近").unwrap();
        assert_eq!(
            Config::load_prompt_from(&path),
            "让图1里的人物转身，镜头缓慢推近"
        );

        // 文件不存在时返回空串，不应 panic
        let missing = path.with_file_name("does-not-exist.txt");
        assert_eq!(Config::load_prompt_from(&missing), "");
    }

    /// 提示词文件与配置文件在同一目录，但不是同一个文件。
    #[test]
    fn prompt_file_lives_next_to_config() {
        let config = Config::file_path();
        let prompt = Config::prompt_file_path();
        assert_eq!(config.parent(), prompt.parent());
        assert_ne!(config, prompt);
        assert_eq!(prompt.file_name().unwrap(), "prompt.txt");
    }
}
