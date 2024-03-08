use std::collections::{
    BTreeMap,
    HashMap,
};

use async_trait::async_trait;
use futures::{
    future::BoxFuture,
    StreamExt,
    TryStreamExt,
};
use http::StatusCode;
use reqwest::{
    redirect,
    Body,
    Proxy,
    Url,
};

use crate::http::{
    HttpRequestStream,
    HttpResponseStream,
};

/// Http client used for fetch syscall.
#[async_trait]
pub trait FetchClient: Send + Sync {
    async fn fetch(&self, request: HttpRequestStream) -> anyhow::Result<HttpResponseStream>;
}

#[derive(Clone)]
pub struct ProxiedFetchClient(reqwest::Client);

impl ProxiedFetchClient {
    pub fn new(proxy_url: Option<Url>, convex_site: String) -> Self {
        let mut builder = reqwest::Client::builder().redirect(redirect::Policy::none());
        // It's okay to panic on these errors, as they indicate a serious programming
        // error -- building the reqwest client is expected to be infallible.
        if let Some(proxy_url) = proxy_url {
            let proxy = Proxy::all(proxy_url)
                .expect("Infallible conversion from URL type to URL type")
                .custom_http_auth(
                    convex_site
                        .try_into()
                        .expect("Backend name is not valid ASCII?"),
                );
            builder = builder.proxy(proxy);
        }
        Self(builder.build().expect("Failed to build reqwest client"))
    }
}

#[async_trait]
impl FetchClient for ProxiedFetchClient {
    async fn fetch(&self, request: HttpRequestStream) -> anyhow::Result<HttpResponseStream> {
        let mut request_builder = self.0.request(request.method, request.url.as_str());
        let body = Body::wrap_stream(request.body);
        request_builder = request_builder.body(body);
        for (name, value) in &request.headers {
            request_builder = request_builder.header(name.as_str(), value.as_bytes());
        }
        let raw_request = request_builder.build()?;
        let raw_response = self.0.execute(raw_request).await?;
        if raw_response.status() == StatusCode::PROXY_AUTHENTICATION_REQUIRED {
            // SSRF mitigated -- our proxy blocked this request because it was
            // directed at a non-public IP range. Don't send back the raw HTTP response as
            // it leaks internal implementation details in the response headers.
            anyhow::bail!("Request to {} forbidden", request.url);
        }
        let status = raw_response.status();
        let headers = raw_response.headers().to_owned();
        let response = HttpResponseStream {
            status,
            headers,
            url: Some(request.url),
            body: Some(raw_response.bytes_stream().map_err(|e| e.into()).boxed()),
        };
        Ok(response)
    }
}

type HandlerFn = Box<
    dyn Fn(HttpRequestStream) -> BoxFuture<'static, anyhow::Result<HttpResponseStream>>
        + Send
        + Sync
        + 'static,
>;

pub struct StaticFetchClient {
    router: BTreeMap<url::Url, HashMap<http::Method, HandlerFn>>,
}

impl StaticFetchClient {
    pub fn new() -> Self {
        Self {
            router: BTreeMap::new(),
        }
    }

    pub fn register_http_route<F>(&mut self, url: url::Url, method: http::Method, handler: F)
    where
        F: Fn(HttpRequestStream) -> BoxFuture<'static, anyhow::Result<HttpResponseStream>>
            + Send
            + Sync
            + 'static,
    {
        self.router
            .entry(url)
            .or_default()
            .insert(method, Box::new(handler));
    }
}

#[async_trait]
impl FetchClient for StaticFetchClient {
    async fn fetch(&self, request: HttpRequestStream) -> anyhow::Result<HttpResponseStream> {
        let handler = self
            .router
            .get(&request.url)
            .and_then(|methods| methods.get(&request.method))
            .unwrap_or_else(|| {
                panic!(
                    "could not find route {} with method {}",
                    request.url, request.method
                )
            });
        handler(request).await
    }
}

pub enum InternalFetchPurpose {
    UsageTracking,
}

#[cfg(test)]
mod tests {
    use http::Method;

    use super::ProxiedFetchClient;
    use crate::http::{
        fetch::FetchClient,
        HttpRequest,
    };

    #[tokio::test]
    async fn test_fetch_bad_url() -> anyhow::Result<()> {
        let client =
            ProxiedFetchClient::new(None, "http://example-deployment.convex.site/".to_owned());
        let request = HttpRequest {
            headers: Default::default(),
            url: "http://\"".parse()?,
            method: Method::GET,
            body: None,
        };
        let Err(err) = client.fetch(request.into()).await else {
            panic!("Expected Invalid URL error");
        };

        // Ensure it doesn't panic. Regression test for.
        // https://github.com/seanmonstar/reqwest/issues/668
        assert!(err.to_string().contains("Parsed Url is not a valid Uri"));

        Ok(())
    }
}
