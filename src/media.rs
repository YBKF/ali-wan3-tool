//! 参考图预处理：本地文件校验 / 压缩 / 转 Base64 data URI，以及公网 URL 的简单校验。

use anyhow::{bail, Context as _, Result};
use base64::Engine as _;
use std::path::{Path, PathBuf};

/// 参考图上限（服务端限制：reference_image 最多 10 张）
pub const MAX_IMAGES: usize = 10;
/// 服务端限制：单张图片不超过 20MB
const MAX_RAW_BYTES: u64 = 20 * 1024 * 1024;
/// 超过该体积就先压缩，给 Base64 膨胀（约 +33%）留出余量
const COMPRESS_THRESHOLD: u64 = 14 * 1024 * 1024;
/// 服务端限制：单边 [240, 8000] 像素
const MIN_SIDE: u32 = 240;
const MAX_SIDE: u32 = 8000;
/// 服务端限制：长宽比不超过 8:1
const MAX_ASPECT: f64 = 8.0;
/// 压缩后的长边像素
const COMPRESS_LONG_SIDE: u32 = 2560;

#[derive(Clone, Debug)]
pub struct RefImage {
    /// 列表里显示的名字
    pub label: String,
    /// 第二行说明（尺寸 / 体积 / 来源）
    pub info: String,
    /// 额外提示，例如压缩说明
    pub note: Option<String>,
    /// 本地预览图路径；远程 URL 为 None
    pub preview: Option<PathBuf>,
    /// 最终写入 `media[].url` 的值：data URI 或公网 URL
    pub media_url: String,
}

/// 读取并校验本地图片，必要时压缩，返回可直接提交的 data URI。
pub fn prepare_local(path: &Path) -> Result<RefImage> {
    let extension = path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let mime = mime_for(&extension)?;

    let metadata = std::fs::metadata(path)
        .with_context(|| format!("无法读取文件 {}", path.display()))?;
    let raw_len = metadata.len();
    if raw_len == 0 {
        bail!("文件为空");
    }

    let (width, height) =
        image::image_dimensions(path).with_context(|| "无法解析图片尺寸，可能不是有效图片")?;
    check_geometry(width, height)?;

    let (bytes, mime, note) = if raw_len > COMPRESS_THRESHOLD {
        let compressed = compress(path)?;
        let note = format!(
            "体积超限已自动压缩 {} → {}",
            human_size(raw_len),
            human_size(compressed.len() as u64)
        );
        (compressed, "image/jpeg", Some(note))
    } else {
        (std::fs::read(path)?, mime, None)
    };

    if bytes.len() as u64 > MAX_RAW_BYTES {
        bail!(
            "图片处理后仍为 {}，超过 20MB 上限，请先手动压缩",
            human_size(bytes.len() as u64)
        );
    }

    let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
    let media_url = format!("data:{mime};base64,{encoded}");

    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("image")
        .to_string();

    Ok(RefImage {
        label: name,
        info: format!("{width}x{height} · {}", human_size(bytes.len() as u64)),
        note,
        preview: Some(path.to_path_buf()),
        media_url,
    })
}

/// 校验公网图片地址。服务端支持 http(s) 与 oss 临时地址，这里不做下载。
pub fn prepare_remote(url: &str) -> Result<RefImage> {
    let url = url.trim();
    if url.is_empty() {
        bail!("请先填写图片地址");
    }
    if !(url.starts_with("http://") || url.starts_with("https://") || url.starts_with("oss://")) {
        bail!("只支持 http:// https:// oss:// 开头的地址");
    }
    let label = url
        .rsplit('/')
        .find(|segment| !segment.is_empty())
        .map(|segment| {
            if segment.len() > 42 {
                format!("…{}", &segment[segment.len() - 42..])
            } else {
                segment.to_string()
            }
        })
        .unwrap_or_else(|| "远程图片".to_string());

    Ok(RefImage {
        label,
        info: "远程 URL · 由服务端拉取".to_string(),
        note: None,
        preview: None,
        media_url: url.to_string(),
    })
}

fn mime_for(extension: &str) -> Result<&'static str> {
    Ok(match extension {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "bmp" => "image/bmp",
        "webp" => "image/webp",
        other => bail!("不支持的图片格式 .{other}，仅支持 jpg / jpeg / png / bmp / webp"),
    })
}

fn check_geometry(width: u32, height: u32) -> Result<()> {
    if width.min(height) < MIN_SIDE || width.max(height) > MAX_SIDE {
        bail!("图片单边需在 {MIN_SIDE}~{MAX_SIDE} 像素之间，当前 {width}x{height}");
    }
    let aspect = width.max(height) as f64 / width.min(height) as f64;
    if aspect > MAX_ASPECT {
        bail!("图片长宽比不能超过 8:1，当前 {aspect:.1}:1");
    }
    Ok(())
}

/// 解码 → 等比缩放到长边 2560 → 去透明通道 → 编码为 JPEG。
fn compress(path: &Path) -> Result<Vec<u8>> {
    let source = image::open(path).with_context(|| "图片解码失败")?;
    let (width, height) = (source.width(), source.height());

    let scale = (COMPRESS_LONG_SIDE as f64 / width.max(height) as f64).min(1.0);
    let source = if scale < 1.0 {
        let target_width = ((width as f64 * scale).round() as u32).max(1);
        let target_height = ((height as f64 * scale).round() as u32).max(1);
        source.resize(target_width, target_height, image::imageops::FilterType::Lanczos3)
    } else {
        source
    };

    let rgb = source.to_rgb8();
    let mut buffer = Vec::new();
    let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buffer, 88);
    encoder
        .encode(rgb.as_raw(), rgb.width(), rgb.height(), image::ExtendedColorType::Rgb8)
        .with_context(|| "JPEG 编码失败")?;
    Ok(buffer)
}

pub fn human_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    let value = bytes as f64;
    if value >= MB {
        format!("{:.1}MB", value / MB)
    } else if value >= KB {
        format!("{:.0}KB", value / KB)
    } else {
        format!("{bytes}B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_dir() -> PathBuf {
        let dir = std::env::temp_dir().join("ali-wan3-tool-tests");
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }

    fn write_sample(path: &Path, width: u32, height: u32) {
        let buffer = image::RgbImage::from_fn(width, height, |x, y| {
            image::Rgb([
                ((x * 7) % 256) as u8,
                ((y * 13) % 256) as u8,
                ((x + y) % 256) as u8,
            ])
        });
        image::DynamicImage::ImageRgb8(buffer)
            .save(path)
            .expect("save sample image");
    }

    #[test]
    fn geometry_limits_match_documentation() {
        assert!(check_geometry(1024, 1024).is_ok());
        // 单边下限 + 正好 8:1
        assert!(check_geometry(240, 1920).is_ok());
        // 单边上限 + 正好 8:1
        assert!(check_geometry(1000, 8000).is_ok());

        // 单边小于 240
        assert!(check_geometry(200, 1024).is_err());
        // 单边大于 8000
        assert!(check_geometry(9000, 1000).is_err());
        // 长宽比超过 8:1（240x8000 边长都合法但比例为 33:1）
        assert!(check_geometry(240, 8000).is_err());
        assert!(check_geometry(1024, 100).is_err());
    }

    #[test]
    fn unsupported_extension_is_rejected() {
        assert!(mime_for("png").is_ok());
        assert!(mime_for("jpeg").is_ok());
        assert!(mime_for("gif").is_err());
        assert!(mime_for("heic").is_err());
    }

    #[test]
    fn remote_url_validation() {
        assert!(prepare_remote("").is_err());
        assert!(prepare_remote("ftp://a/b.png").is_err());

        let prepared = prepare_remote("https://example.com/a/b/pic.png").unwrap();
        assert_eq!(prepared.media_url, "https://example.com/a/b/pic.png");
        assert_eq!(prepared.label, "pic.png");
        assert!(prepared.preview.is_none());
    }

    /// 本地图片 → data URI 的完整链路。
    #[test]
    fn local_image_becomes_data_uri() {
        let path = scratch_dir().join("sample.png");
        write_sample(&path, 640, 480);

        let prepared = prepare_local(&path).unwrap();
        assert!(prepared.media_url.starts_with("data:image/png;base64,"));
        assert!(prepared.media_url.len() > 1000);
        assert!(prepared.info.starts_with("640x480 · "));
        assert!(prepared.preview.is_some());
        assert!(prepared.note.is_none());
    }

    /// 解码后可用的 JPEG，长边压到 2560。
    #[test]
    fn compress_downsizes_to_long_side_limit() {
        let path = scratch_dir().join("big.png");
        write_sample(&path, 3000, 2000);

        let bytes = compress(&path).unwrap();
        let original = std::fs::metadata(&path).unwrap().len();
        assert!((bytes.len() as u64) < original);

        let decoded = image::load_from_memory(&bytes).unwrap();
        assert_eq!(decoded.width(), COMPRESS_LONG_SIDE);
        assert!(decoded.height() < 2000);
    }
}
