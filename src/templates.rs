//! 提示词模版：`video_gen_prompt` 目录下的 `*.prompt.txt` 文本文件。
//!
//! 一个模版就是一个文件，文件名（去掉 `.prompt.txt`）即标题，文件内容就是固定提示词。
//! 目录与 `prompt.txt` 同级，放在 `%APPDATA%\ali-wan3-tool\video_gen_prompt`，
//! 这样用户可以自己拿编辑器或网盘同步这些模版。
//!
//! 这一层只做文件读写与标题校验，不碰界面。

use anyhow::{Context as _, Result, anyhow};
use std::path::{Path, PathBuf};

use crate::config::Config;

/// 模版文件后缀，连同 `Config::prompt_file_path` 的命名风格保持一致。
pub const TEMPLATE_SUFFIX: &str = ".prompt.txt";

/// 默认标题的前缀，编号从 1 开始递增。
const DEFAULT_PREFIX: &str = "prompt_template_";

/// 标题长度上限（字符数），留足 Windows 文件名空间。
pub const MAX_TITLE_CHARS: usize = 60;

/// Windows 文件名里不允许出现的字符。
const ILLEGAL_CHARS: &[char] = &['<', '>', ':', '"', '/', '\\', '|', '?', '*'];

/// 一个提示词模版。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromptTemplate {
    /// 标题，即去掉 `.prompt.txt` 之后的文件名。
    pub title: String,
    pub path: PathBuf,
}

impl PromptTemplate {
    /// 读出模版正文；空文件返回空串。
    pub fn read(&self) -> Result<String> {
        std::fs::read_to_string(&self.path)
            .with_context(|| format!("读取 {} 失败", self.path.display()))
    }
}

/// 模版目录。与 `config.json`、`prompt.txt` 放在同一个用户目录下。
pub fn dir() -> PathBuf {
    Config::file_path()
        .parent()
        .map(|base| base.join("video_gen_prompt"))
        .unwrap_or_else(|| PathBuf::from("video_gen_prompt"))
}

/// 建好模版目录（打开文件夹、新建模版之前调用）。
pub fn ensure_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("创建目录 {} 失败", dir.display()))
}

/// 列出目录里的全部模版，按文件名排序。
///
/// 目录不存在、没有权限等情况一律当作「没有模版」，不在界面上报错。
pub fn list_in(dir: &Path) -> Vec<PromptTemplate> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };

    let mut templates: Vec<PromptTemplate> = entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().is_file())
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            let title = name.strip_suffix(TEMPLATE_SUFFIX)?.to_string();
            (!title.is_empty()).then(|| PromptTemplate {
                title,
                path: entry.path(),
            })
        })
        .collect();

    // 文件系统不保证顺序，这里按标题（即文件名）排序，大小写不敏感
    templates.sort_by_cached_key(|template| template.title.to_lowercase());
    templates
}

/// 标题对应的文件路径。
pub fn path_for(dir: &Path, title: &str) -> PathBuf {
    dir.join(format!("{title}{TEMPLATE_SUFFIX}"))
}

/// 下一个默认标题：`prompt_template_{n}`，取第一个还没被占用的编号。
pub fn next_title_in(dir: &Path) -> String {
    (1..)
        .map(|n| format!("{DEFAULT_PREFIX}{n}"))
        .find(|title| !path_for(dir, title).exists())
        .expect("无穷序列里一定有可用编号")
}

/// 校验标题：返回去掉首尾空白后的标题，或一句可以直接展示给用户的错误。
pub fn validate_title(title: &str) -> Result<String, String> {
    let title = title.trim();

    if title.is_empty() {
        return Err("请输入模版标题（标题就是文件名）".to_string());
    }
    if title.chars().count() > MAX_TITLE_CHARS {
        return Err(format!("标题最长 {MAX_TITLE_CHARS} 个字符"));
    }
    if let Some(ch) = title.chars().find(|ch| ILLEGAL_CHARS.contains(ch)) {
        return Err(format!("标题不能包含字符 {ch}"));
    }
    if title.chars().any(char::is_control) {
        return Err("标题不能包含不可见字符".to_string());
    }
    if title.starts_with('.') && title.chars().all(|ch| ch == '.') {
        return Err("标题不能只用「.」".to_string());
    }
    if title.ends_with('.') || title.ends_with(' ') {
        return Err("标题不能以空格或句点结尾".to_string());
    }

    Ok(title.to_string())
}

/// 在指定目录新建模版文件（内容为空，随后由系统编辑器填充）。
pub fn create_in(dir: &Path, title: &str) -> Result<PathBuf> {
    let title = validate_title(title).map_err(|message| anyhow!("{message}"))?;
    ensure_dir(dir)?;

    let path = path_for(dir, &title);
    if path.exists() {
        return Err(anyhow!("模版「{title}」已存在，请换一个标题"));
    }

    std::fs::write(&path, "").with_context(|| format!("创建模版 {} 失败", path.display()))?;
    Ok(path)
}

/// 改标题：只改文件名，文件内容不动。
///
/// 新标题同样要走 [`validate_title`] 的规则；和其他模版重名（大小写不敏感，
/// 因为 Windows 文件名不区分大小写）时直接报错，绝不覆盖已有文件。
pub fn rename(path: &Path, new_title: &str) -> Result<PathBuf> {
    let title = validate_title(new_title).map_err(|message| anyhow!("{message}"))?;
    let dir = path
        .parent()
        .ok_or_else(|| anyhow!("找不到 {} 所在的目录", path.display()))?;
    let target = path_for(dir, &title);

    // 标题没变（或者只改了首尾空白）就什么都不用做
    if target == path {
        return Ok(target);
    }

    let lowered = title.to_lowercase();
    let taken = list_in(dir)
        .into_iter()
        .any(|other| other.path != path && other.title.to_lowercase() == lowered);
    // 只改大小写时目标路径「存在」是同一个文件，不算重名
    let same_file =
        target.to_string_lossy().to_lowercase() == path.to_string_lossy().to_lowercase();
    if taken || (target.exists() && !same_file) {
        return Err(anyhow!("模版「{title}」已存在，请换一个标题"));
    }

    std::fs::rename(path, &target)
        .with_context(|| format!("把 {} 改名为 {} 失败", path.display(), target.display()))?;
    Ok(target)
}

/// 删除模版文件。
pub fn delete(path: &Path) -> Result<()> {
    std::fs::remove_file(path).with_context(|| format!("删除 {} 失败", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join("ali-wan3-tool-tests")
            .join("templates")
            .join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 只认 `.prompt.txt`，且结果按文件名排序。
    #[test]
    fn lists_only_templates_in_file_name_order() {
        let dir = temp_dir("list");
        for name in ["b.prompt.txt", "a.prompt.txt", "notes.txt", "c.md"] {
            std::fs::write(dir.join(name), name).unwrap();
        }
        std::fs::create_dir_all(dir.join("nested.prompt.txt")).unwrap();

        let titles: Vec<String> = list_in(&dir)
            .into_iter()
            .map(|template| template.title)
            .collect();
        assert_eq!(titles, ["a", "b"]);
    }

    /// 默认标题从 1 开始，并跳过已被占用的编号。
    #[test]
    fn default_title_skips_taken_numbers() {
        let dir = temp_dir("next-title");
        assert_eq!(next_title_in(&dir), "prompt_template_1");

        std::fs::write(dir.join("prompt_template_1.prompt.txt"), "").unwrap();
        assert_eq!(next_title_in(&dir), "prompt_template_2");

        // 自定义标题不参与编号，删掉 1 号之后编号会补回来
        std::fs::write(dir.join("人物转身.prompt.txt"), "").unwrap();
        assert_eq!(next_title_in(&dir), "prompt_template_2");
        std::fs::remove_file(dir.join("prompt_template_1.prompt.txt")).unwrap();
        assert_eq!(next_title_in(&dir), "prompt_template_1");
    }

    #[test]
    fn rejects_illegal_titles() {
        for title in ["", "   ", "a/b", "a:b", "a*b", "a\"b", "结尾."] {
            assert!(validate_title(title).is_err(), "{title:?} 应当被拒绝");
        }
        assert_eq!(validate_title("  人物转身  ").unwrap(), "人物转身");
    }

    /// 同名文件不会被覆盖。
    #[test]
    fn create_refuses_to_overwrite() {
        let dir = temp_dir("create");

        let path = create_in(&dir, "人物转身").unwrap();
        assert!(path.ends_with("人物转身.prompt.txt"));
        std::fs::write(&path, "镜头缓慢推近").unwrap();

        assert!(create_in(&dir, "人物转身").is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "镜头缓慢推近");
    }

    /// 改标题：内容跟着走，标题变非法或和别人重名时不动文件。
    #[test]
    fn rename_moves_content_and_refuses_duplicates() {
        let dir = temp_dir("rename");
        let path = create_in(&dir, "人物转身").unwrap();
        std::fs::write(&path, "镜头缓慢推近").unwrap();
        create_in(&dir, "其他").unwrap();

        let renamed = rename(&path, "  人物转身特写  ").unwrap();
        assert!(renamed.ends_with("人物转身特写.prompt.txt"));
        assert!(!path.exists());
        assert_eq!(std::fs::read_to_string(&renamed).unwrap(), "镜头缓慢推近");

        // 名字没变：什么也不做，不报错
        assert_eq!(rename(&renamed, "人物转身特写").unwrap(), renamed);

        // 重名（含大小写变体）与非法标题都拒绝
        create_in(&dir, "abc").unwrap();
        assert!(rename(&renamed, "其他").is_err());
        assert!(rename(&renamed, "ABC").is_err());
        assert!(rename(&renamed, "a/b").is_err());
        assert!(rename(&renamed, "   ").is_err());
        assert_eq!(std::fs::read_to_string(&renamed).unwrap(), "镜头缓慢推近");
    }

    /// 新建的空模版读回来是空串，删除后列表里也没有了。
    #[test]
    fn create_read_and_delete() {
        let dir = temp_dir("round-trip");
        let path = create_in(&dir, "prompt_template_1").unwrap();

        let template = list_in(&dir).remove(0);
        assert_eq!(template.read().unwrap(), "");

        delete(&path).unwrap();
        assert!(list_in(&dir).is_empty());
    }
}
