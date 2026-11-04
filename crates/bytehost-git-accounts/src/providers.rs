//! 三家 provider 的 endpoint、鉴权头与响应解析——只有这一份实现。
//!
//! 纯函数,不发起请求;真实 HTTP 由 [`crate::transport`] 的可注入实现执行。

use crate::model::{ConnectedAccount, GitAccountError, GitProvider, RemoteRepo};

/// 一次 HTTP GET 的纯描述。
///
/// 自定义 Debug 会打码 `Authorization` / `PRIVATE-TOKEN` 头与
/// `access_token` 等查询参数,避免诊断日志泄露 PAT。
#[derive(Clone, PartialEq, Eq)]
pub struct HttpRequest {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub query: Vec<(String, String)>,
}

fn sensitive_header(name: &str) -> bool {
    name.eq_ignore_ascii_case("authorization") || name.eq_ignore_ascii_case("private-token")
}

fn sensitive_query(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "access_token" | "token" | "private_token" | "oauth_token"
    )
}

impl std::fmt::Debug for HttpRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let headers: Vec<(&str, &str)> = self
            .headers
            .iter()
            .map(|(name, value)| {
                if sensitive_header(name) {
                    (name.as_str(), "***")
                } else {
                    (name.as_str(), value.as_str())
                }
            })
            .collect();
        let query: Vec<(&str, &str)> = self
            .query
            .iter()
            .map(|(name, value)| {
                if sensitive_query(name) {
                    (name.as_str(), "***")
                } else {
                    (name.as_str(), value.as_str())
                }
            })
            .collect();
        f.debug_struct("HttpRequest")
            .field("url", &self.url)
            .field("headers", &headers)
            .field("query", &query)
            .finish()
    }
}

/// 把底层错误文本里出现的敏感头 / 查询值替换成 `***`。
pub fn redact_secrets(mut text: String, request: &HttpRequest) -> String {
    for (name, value) in &request.headers {
        if sensitive_header(name) && !value.is_empty() {
            text = text.replace(value.as_str(), "***");
        }
    }
    for (name, value) in &request.query {
        if sensitive_query(name) && !value.is_empty() {
            text = text.replace(value.as_str(), "***");
        }
    }
    text
}

/// 一次 HTTP 响应(只要状态码与正文,足够解析三类 API)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub body: String,
}

impl HttpResponse {
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

impl GitProvider {
    /// whoami 用户信息接口。
    pub fn whoami_url(self) -> &'static str {
        match self {
            GitProvider::GitHub => "https://api.github.com/user",
            GitProvider::GitLab => "https://gitlab.com/api/v4/user",
            GitProvider::Gitee => "https://gitee.com/api/v5/user",
        }
    }

    /// PAT 创建页面(由产品用系统浏览器打开)。
    pub fn token_creation_url(self) -> &'static str {
        match self {
            GitProvider::GitHub => "https://github.com/settings/tokens/new",
            GitProvider::GitLab => "https://gitlab.com/-/user_settings/personal_access_tokens",
            GitProvider::Gitee => "https://gitee.com/profile/personal_access_tokens/new",
        }
    }

    pub fn list_repos_url(self) -> &'static str {
        match self {
            GitProvider::GitHub => {
                "https://api.github.com/user/repos?per_page=100&sort=updated&affiliation=owner"
            }
            GitProvider::GitLab => {
                "https://gitlab.com/api/v4/projects?membership=true&owned=true&per_page=100&order_by=last_activity_at"
            }
            GitProvider::Gitee => "https://gitee.com/api/v5/user/repos?per_page=100&sort=updated",
        }
    }

    /// 构造 whoami 请求。三家鉴权方式不同:GitHub `Bearer`,GitLab
    /// `PRIVATE-TOKEN`,Gitee `access_token` query。
    pub fn whoami_request(self, token: &str) -> HttpRequest {
        let mut request = HttpRequest {
            url: self.whoami_url().to_string(),
            headers: vec![("User-Agent".into(), "bytehost".into())],
            query: Vec::new(),
        };
        match self {
            GitProvider::GitHub => request
                .headers
                .push(("Authorization".into(), format!("Bearer {token}"))),
            GitProvider::GitLab => request
                .headers
                .push(("PRIVATE-TOKEN".into(), token.to_string())),
            GitProvider::Gitee => request
                .query
                .push(("access_token".into(), token.to_string())),
        }
        request
    }

    /// 构造仓库列表请求,鉴权方式与 whoami 一致。
    pub fn list_repos_request(self, token: &str) -> HttpRequest {
        let mut request = self.whoami_request(token);
        request.url = self.list_repos_url().to_string();
        request
    }

    /// 三家 whoami 的用户名字段都叫 `login`。
    pub fn extract_username(self, json: &str) -> Result<ConnectedAccount, GitAccountError> {
        let value: serde_json::Value =
            serde_json::from_str(json).map_err(|e| GitAccountError::ParseResponse {
                detail: e.to_string(),
            })?;
        let username = value
            .get("login")
            .and_then(|v| v.as_str())
            .ok_or(GitAccountError::MissingLoginField)?;
        Ok(ConnectedAccount {
            username: username.to_string(),
        })
    }

    /// 仓库列表字段名按 provider 不同;单条脏数据跳过,不让整个列表失败。
    pub fn parse_repo_list(self, json: &str) -> Result<Vec<RemoteRepo>, GitAccountError> {
        let value: serde_json::Value =
            serde_json::from_str(json).map_err(|e| GitAccountError::ParseResponse {
                detail: e.to_string(),
            })?;
        let items = value.as_array().ok_or(GitAccountError::NotArray)?;
        let (name_field, url_field) = match self {
            GitProvider::GitHub => ("full_name", "clone_url"),
            GitProvider::GitLab => ("path_with_namespace", "http_url_to_repo"),
            GitProvider::Gitee => ("full_name", "html_url"),
        };
        Ok(items
            .iter()
            .filter_map(|item| {
                let full_name = item.get(name_field)?.as_str()?.to_string();
                let clone_url = item.get(url_field)?.as_str()?.to_string();
                Some(RemoteRepo {
                    full_name,
                    clone_url,
                })
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_and_error_redaction_never_expose_tokens() {
        let github = GitProvider::GitHub.whoami_request("ghp_marker");
        let debug = format!("{github:?}");
        assert!(!debug.contains("ghp_marker"));
        assert!(debug.contains("***"));

        let gitee = GitProvider::Gitee.whoami_request("gitee_marker");
        let debug = format!("{gitee:?}");
        assert!(!debug.contains("gitee_marker"));
        let error = redact_secrets(
            "request failed for https://gitee.com/api/v5/user?access_token=gitee_marker".into(),
            &gitee,
        );
        assert!(!error.contains("gitee_marker"));
        assert!(error.contains("***"));
    }

    #[test]
    fn github_uses_bearer_header() {
        let request = GitProvider::GitHub.whoami_request("tok");
        assert_eq!(request.url, "https://api.github.com/user");
        assert!(
            request
                .headers
                .contains(&("Authorization".to_string(), "Bearer tok".to_string()))
        );
        assert!(request.query.is_empty());
    }

    #[test]
    fn gitlab_uses_private_token_header() {
        let request = GitProvider::GitLab.whoami_request("tok");
        assert!(
            request
                .headers
                .contains(&("PRIVATE-TOKEN".to_string(), "tok".to_string()))
        );
    }

    #[test]
    fn gitee_uses_query_token() {
        let request = GitProvider::Gitee.whoami_request("tok");
        assert_eq!(request.query, vec![("access_token".into(), "tok".into())]);
        assert!(!request.url.contains("tok"));
    }

    #[test]
    fn extract_username_reads_login() {
        assert_eq!(
            GitProvider::GitHub
                .extract_username(r#"{"login":"octocat","id":1}"#)
                .unwrap()
                .username,
            "octocat"
        );
        assert_eq!(
            GitProvider::GitHub.extract_username(r#"{"id":1}"#),
            Err(GitAccountError::MissingLoginField)
        );
        assert!(matches!(
            GitProvider::GitHub.extract_username("not json"),
            Err(GitAccountError::ParseResponse { .. })
        ));
    }

    #[test]
    fn parse_repo_list_per_provider() {
        let github = GitProvider::GitHub
            .parse_repo_list(r#"[{"full_name":"a/b","clone_url":"https://github.com/a/b.git"}]"#)
            .unwrap();
        assert_eq!(github[0].full_name, "a/b");

        let gitlab = GitProvider::GitLab
            .parse_repo_list(
                r#"[{"path_with_namespace":"a/c","http_url_to_repo":"https://gitlab.com/a/c.git"}]"#,
            )
            .unwrap();
        assert_eq!(gitlab[0].clone_url, "https://gitlab.com/a/c.git");

        let gitee = GitProvider::Gitee
            .parse_repo_list(r#"[{"full_name":"a/d","html_url":"https://gitee.com/a/d"}]"#)
            .unwrap();
        assert_eq!(gitee[0].clone_url, "https://gitee.com/a/d");
    }

    #[test]
    fn repo_list_skips_dirty_items_and_rejects_non_arrays() {
        let repos = GitProvider::GitHub
            .parse_repo_list(r#"[{"full_name":"a/b"},{"full_name":"c/d","clone_url":"u"}]"#)
            .unwrap();
        assert_eq!(repos.len(), 1);
        assert_eq!(
            GitProvider::GitHub.parse_repo_list(r#"{"login":"x"}"#),
            Err(GitAccountError::NotArray)
        );
    }
}
