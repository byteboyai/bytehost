//! HTTP 传输接缝:默认只有 trait 与 fake,真实网络由 `http` feature 提供。

use std::future::Future;

use crate::model::GitAccountError;
#[cfg(feature = "http")]
use crate::providers::redact_secrets;
use crate::providers::{HttpRequest, HttpResponse};

/// 可注入的 HTTP GET 实现。
pub trait HttpTransport: Send + Sync {
    fn send(
        &self,
        request: HttpRequest,
    ) -> impl Future<Output = Result<HttpResponse, GitAccountError>> + Send;
}

type FakeResponse = (String, Result<HttpResponse, GitAccountError>);

/// 测试 / 示例用:按 URL 前缀返回预置响应,并记录全部请求。
#[derive(Debug, Default, Clone)]
pub struct FakeTransport {
    responses: std::sync::Arc<std::sync::Mutex<Vec<FakeResponse>>>,
    requests: std::sync::Arc<std::sync::Mutex<Vec<HttpRequest>>>,
}

impl FakeTransport {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push_json(
        self,
        url_prefix: impl Into<String>,
        status: u16,
        body: impl Into<String>,
    ) -> Self {
        self.responses.lock().expect("fake transport lock").push((
            url_prefix.into(),
            Ok(HttpResponse {
                status,
                body: body.into(),
            }),
        ));
        self
    }

    pub fn push_error(self, url_prefix: impl Into<String>, error: GitAccountError) -> Self {
        self.responses
            .lock()
            .expect("fake transport lock")
            .push((url_prefix.into(), Err(error)));
        self
    }

    pub fn requests(&self) -> Vec<HttpRequest> {
        self.requests.lock().expect("fake request lock").clone()
    }
}

fn join_query(request: &HttpRequest) -> String {
    if request.query.is_empty() {
        return request.url.clone();
    }
    let separator = if request.url.contains('?') { '&' } else { '?' };
    let query = request
        .query
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("&");
    format!("{}{separator}{query}", request.url)
}

impl HttpTransport for FakeTransport {
    async fn send(&self, request: HttpRequest) -> Result<HttpResponse, GitAccountError> {
        self.requests
            .lock()
            .expect("fake request lock")
            .push(request.clone());
        let url = join_query(&request);
        let responses = self.responses.lock().expect("fake transport lock");
        responses
            .iter()
            .find(|(prefix, _)| url.starts_with(prefix))
            .map(|(_, response)| response.clone())
            .unwrap_or(Err(GitAccountError::Network {
                detail: format!("fake transport has no response for {url}"),
            }))
    }
}

/// reqwest 实现。客户端每次新建,避免把运行时 / 连接池塞进共享状态。
#[cfg(feature = "http")]
#[derive(Debug, Clone, Default)]
pub struct ReqwestTransport;

#[cfg(feature = "http")]
impl HttpTransport for ReqwestTransport {
    async fn send(&self, request: HttpRequest) -> Result<HttpResponse, GitAccountError> {
        let client = reqwest::Client::new();
        let mut builder = client.get(&request.url);
        for (name, value) in &request.headers {
            builder = builder.header(name, value);
        }
        if !request.query.is_empty() {
            builder = builder.query(&request.query);
        }
        // reqwest 的错误字符串 / 诊断文本可能包含带 `access_token` 的完整
        // URL(Gitee),以及请求头;进入错误类型前统一脱敏。
        let response = builder.send().await.map_err(|e| GitAccountError::Network {
            detail: redact_secrets(e.to_string(), &request),
        })?;
        let status = response.status().as_u16();
        let body = response
            .text()
            .await
            .map_err(|e| GitAccountError::Network {
                detail: redact_secrets(e.to_string(), &request),
            })?;
        Ok(HttpResponse { status, body })
    }
}
