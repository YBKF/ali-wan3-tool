use semver::Version;
use serde::Deserialize;

// GitHub 与 Gitee 的发布版本 Json 类似，可以使用同一个结构
// GET https://gitee.com/api/v5/repos/{owner}/{repo}/releases/latest
// GET https://api.github.com/repos/{owner}/{repo}/releases/latest
#[derive(Debug, Deserialize)]
pub struct RepoVersion {
    tag_name: String,
}

impl TryFrom<RepoVersion> for Version {
    type Error = semver::Error;

    fn try_from(value: RepoVersion) -> Result<Self, Self::Error> {
        Version::parse(value.tag_name.trim_matches('v'))
    }
}
