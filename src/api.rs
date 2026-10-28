//! 阿里百炼 万相 3.0 视频生成 API：创建任务 + 轮询查询 + 下载结果。
//!
//! 文档要点：
//! - 必须携带 `X-DashScope-Async: enable`，只支持异步
//! - `reference_image` 与 `first_frame/last_frame` 互斥，本工具只用全能参考模式
//! - 结果链接与 task_id 均只有 24 小时有效，故生成后立即下载

use crate::config::Config;
use anyhow::{Context as _, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::io::{Read as _, Write as _};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

const CREATE_PATH: &str = "/api/v1/services/aigc/video-generation/video-synthesis";
const TASK_PATH: &str = "/api/v1/tasks/";

const REQUEST_TIMEOUT: Duration = Duration::from_secs(180);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30 * 60);

fn agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        // 自行读取响应体以拿到服务端返回的错误码与信息
        .http_status_as_error(false)
        .build()
        .into()
}

#[derive(Debug, Clone, Serialize)]
pub struct MediaItem {
    #[serde(rename = "type")]
    pub kind: String,
    pub url: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CreateRequest {
    pub model: String,
    pub input: RequestInput,
    pub parameters: Parameters,
}

#[derive(Debug, Clone, Serialize)]
pub struct RequestInput {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    pub media: Vec<MediaItem>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Parameters {
    pub resolution: String,
    pub ratio: String,
    /// `-1` 表示智能时长
    pub duration: i32,
    pub audio: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub output_video_duration: Option<f64>,
    #[serde(default)]
    pub fps: Option<i64>,
    #[serde(default, rename = "SR")]
    pub sr: Option<i64>,
    #[serde(default)]
    pub ratio: Option<String>,
}

impl Usage {
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if let Some(duration) = self.output_video_duration {
            parts.push(format!("时长 {duration}s"));
        }
        if let Some(ratio) = self.ratio.as_ref() {
            parts.push(format!("比例 {ratio}"));
        }
        if let Some(sr) = self.sr {
            parts.push(format!("{sr}P"));
        }
        if let Some(fps) = self.fps {
            parts.push(format!("{fps}fps"));
        }
        parts.join(" · ")
    }
}

#[derive(Debug, Clone)]
pub enum TaskOutcome {
    Pending,
    Running,
    Succeeded {
        video_url: String,
        usage: Option<Usage>,
    },
    Failed {
        code: String,
        message: String,
    },
}

#[derive(Debug, Deserialize)]
struct CreateEnvelope {
    #[serde(default)]
    output: Option<CreateOutput>,
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CreateOutput {
    #[serde(default)]
    task_id: Option<String>,
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct QueryEnvelope {
    #[serde(default)]
    output: Option<QueryOutput>,
    #[serde(default)]
    usage: Option<Usage>,
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct QueryOutput {
    #[serde(default)]
    task_status: Option<String>,
    #[serde(default)]
    video_url: Option<String>,
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

/// 创建视频生成任务，返回 `task_id`。
pub fn create_task(config: &Config, request: &CreateRequest) -> Result<String> {
    let api_key = config.api_key.trim();
    if api_key.is_empty() {
        bail!("请先填写 API Key");
    }

    let url = format!("{}{CREATE_PATH}", config.endpoint_base());
    let response = agent(REQUEST_TIMEOUT)
        .post(&url)
        .header("Content-Type", "application/json")
        .header("Authorization", format!("Bearer {api_key}"))
        .header("X-DashScope-Async", "enable")
        .send_json(request);

    let mut response = match response {
        Ok(response) => response,
        Err(error) => bail!("请求创建任务失败（{url}）：{error}"),
    };
    let status = response.status().as_u16();
    let body = response.body_mut().read_to_string().unwrap_or_default();

    let envelope: CreateEnvelope = serde_json::from_str(&body).map_err(|_| {
        anyhow!(
            "服务端返回的不是 JSON（HTTP {status}）：{}",
            truncate(&body)
        )
    })?;

    if let Some(task_id) = envelope.output.as_ref().and_then(|o| o.task_id.clone()) {
        return Ok(task_id);
    }

    if let Some(output) = envelope.output.as_ref()
        && let Some(message) = output.message.as_ref()
    {
        bail!(
            "创建任务失败：{} {message}",
            output.code.clone().unwrap_or_default()
        );
    }
    if let Some(message) = envelope.message.as_ref() {
        bail!(
            "创建任务失败：{} {message}",
            envelope.code.clone().unwrap_or_default()
        );
    }
    bail!("创建任务失败（HTTP {status}）：{}", truncate(&body));
}

/// 查询一次任务状态。
pub fn query_task(config: &Config, task_id: &str) -> Result<TaskOutcome> {
    let api_key = config.api_key.trim();
    let url = format!("{}{TASK_PATH}{task_id}", config.endpoint_base());

    let mut response = agent(REQUEST_TIMEOUT)
        .get(&url)
        .header("Authorization", format!("Bearer {api_key}"))
        .call()
        .with_context(|| format!("查询任务失败（{url}）"))?;
    let status = response.status().as_u16();
    let body = response.body_mut().read_to_string().unwrap_or_default();

    let envelope: QueryEnvelope = serde_json::from_str(&body)
        .map_err(|_| anyhow!("查询返回的不是 JSON（HTTP {status}）：{}", truncate(&body)))?;

    if let Some(message) = envelope.message.as_ref() {
        bail!(
            "查询任务失败：{} {message}",
            envelope.code.clone().unwrap_or_default()
        );
    }

    let output = envelope.output.as_ref().ok_or_else(|| {
        anyhow!(
            "查询返回缺少 output 字段（HTTP {status}）：{}",
            truncate(&body)
        )
    })?;

    match output.task_status.as_deref().unwrap_or("UNKNOWN") {
        "PENDING" => Ok(TaskOutcome::Pending),
        "RUNNING" => Ok(TaskOutcome::Running),
        "SUCCEEDED" => {
            let video_url = output
                .video_url
                .clone()
                .ok_or_else(|| anyhow!("任务成功但未返回 video_url"))?;
            Ok(TaskOutcome::Succeeded {
                video_url,
                usage: envelope.usage.clone(),
            })
        }
        "FAILED" | "CANCELED" => Ok(TaskOutcome::Failed {
            code: output.code.clone().unwrap_or_else(|| "Unknown".to_string()),
            message: output
                .message
                .clone()
                .unwrap_or_else(|| "任务执行失败".to_string()),
        }),
        "UNKNOWN" => bail!("任务不存在或已过期（task_id 有效期 24 小时）"),
        other => bail!("未知任务状态：{other}"),
    }
}

/// 下载生成的视频到本地，边下边通过原子计数上报进度。
pub fn download_video(
    url: &str,
    destination: &Path,
    received: &AtomicU64,
    total: &AtomicU64,
) -> Result<u64> {
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("无法创建输出目录 {}", parent.display()))?;
    }

    let response = agent(DOWNLOAD_TIMEOUT).get(url).call()?;
    let status = response.status().as_u16();
    if status >= 400 {
        bail!("下载视频失败，HTTP {status}");
    }

    if let Some(length) = response
        .headers()
        .get("content-length")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
    {
        total.store(length, Ordering::Relaxed);
    }

    let mut response = response;
    let mut reader = response.body_mut().as_reader();
    let mut file = std::fs::File::create(destination)
        .with_context(|| format!("无法写入 {}", destination.display()))?;

    let mut buffer = vec![0u8; 128 * 1024];
    let mut written: u64 = 0;
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        file.write_all(&buffer[..read])?;
        written += read as u64;
        received.store(written, Ordering::Relaxed);
    }
    file.flush()?;
    Ok(written)
}

fn truncate(text: &str) -> String {
    const LIMIT: usize = 400;
    if text.chars().count() <= LIMIT {
        text.to_string()
    } else {
        format!("{}…", text.chars().take(LIMIT).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_request(prompt: Option<&str>) -> CreateRequest {
        CreateRequest {
            model: "wan3.0-video".to_string(),
            input: RequestInput {
                prompt: prompt.map(|value| value.to_string()),
                media: vec![MediaItem {
                    kind: "reference_image".to_string(),
                    url: "data:image/png;base64,AAAA".to_string(),
                }],
            },
            parameters: Parameters {
                resolution: "720P".to_string(),
                ratio: "16:9".to_string(),
                duration: 5,
                audio: true,
            },
        }
    }

    /// 请求体字段名与枚举值必须和官方文档一致。
    #[test]
    fn request_body_matches_documented_shape() {
        let value = serde_json::to_value(sample_request(Some("让图1里的人物转身"))).unwrap();

        assert_eq!(value["model"], "wan3.0-video");
        assert_eq!(value["input"]["prompt"], "让图1里的人物转身");
        assert_eq!(value["input"]["media"][0]["type"], "reference_image");
        assert_eq!(
            value["input"]["media"][0]["url"],
            "data:image/png;base64,AAAA"
        );
        assert_eq!(value["parameters"]["resolution"], "720P");
        assert_eq!(value["parameters"]["ratio"], "16:9");
        assert_eq!(value["parameters"]["duration"], 5);
        assert_eq!(value["parameters"]["audio"], true);
    }

    /// 只传参考图时不发送 prompt 字段；智能时长用 -1。
    #[test]
    fn omits_empty_prompt_and_supports_smart_duration() {
        let mut request = sample_request(None);
        request.parameters.duration = -1;

        let value = serde_json::to_value(request).unwrap();
        assert!(value["input"].get("prompt").is_none());
        assert_eq!(value["parameters"]["duration"], -1);
        assert_eq!(value["input"]["media"][0]["type"], "reference_image");
    }

    #[test]
    fn parses_succeeded_envelope() {
        let body = r#"{
            "request_id": "78c9b768",
            "output": {
                "task_id": "17ed7e50",
                "task_status": "SUCCEEDED",
                "video_url": "https://dashscope-result-bj.oss-cn-beijing.aliyuncs.com/xxx/video.mp4"
            },
            "usage": {
                "video_count": 1,
                "output_video_duration": 5.0,
                "fps": 30,
                "SR": 720,
                "ratio": "16:9"
            }
        }"#;

        let envelope: QueryEnvelope = serde_json::from_str(body).unwrap();
        let output = envelope.output.as_ref().unwrap();
        assert_eq!(output.task_status.as_deref(), Some("SUCCEEDED"));
        assert!(output.video_url.as_ref().unwrap().ends_with("video.mp4"));

        let usage = envelope.usage.unwrap();
        assert_eq!(usage.sr, Some(720));
        assert_eq!(usage.fps, Some(30));
        assert_eq!(usage.describe(), "时长 5s · 比例 16:9 · 720P · 30fps");
    }

    #[test]
    fn parses_failed_envelope() {
        let body = r#"{
            "request_id": "e5e57877",
            "output": {
                "task_id": "eff1443c",
                "task_status": "FAILED",
                "code": "InvalidParameter",
                "message": "The two modes are mutually exclusive."
            }
        }"#;

        let envelope: QueryEnvelope = serde_json::from_str(body).unwrap();
        let output = envelope.output.unwrap();
        assert_eq!(output.task_status.as_deref(), Some("FAILED"));
        assert_eq!(output.code.as_deref(), Some("InvalidParameter"));
    }
}
