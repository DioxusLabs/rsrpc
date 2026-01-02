//! HTTP client and server support for rrpc services.
//!
//! This module provides HTTP/REST bindings for services that have methods
//! annotated with `#[get]`, `#[post]`, etc.

use std::marker::PhantomData;

use anyhow::Result;
use serde::{de::DeserializeOwned, Serialize};

pub use ::http::Method;

/// HTTP client that can call service methods over REST.
///
/// Methods annotated with HTTP attributes (like `#[get("/path")]`) can be
/// called through this client. Methods without HTTP attributes will panic.
///
/// # Example
///
/// ```ignore
/// let client: HttpClient<dyn MyService> = HttpClient::new("http://localhost:8080");
/// let result = client.get_user("123".into()).await?;
/// ```
pub struct HttpClient<T: ?Sized> {
    base_url: String,
    client: reqwest::Client,
    _marker: PhantomData<T>,
}

impl<T: ?Sized> Clone for HttpClient<T> {
    fn clone(&self) -> Self {
        Self {
            base_url: self.base_url.clone(),
            client: self.client.clone(),
            _marker: PhantomData,
        }
    }
}

impl<T: ?Sized + 'static> HttpClient<T> {
    /// Create a new HTTP client with the given base URL.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let client: HttpClient<dyn MyService> = HttpClient::new("http://localhost:8080");
    /// ```
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            client: reqwest::Client::new(),
            _marker: PhantomData,
        }
    }

    /// Create a new HTTP client with a custom reqwest client.
    pub fn with_client(base_url: impl Into<String>, client: reqwest::Client) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            client,
            _marker: PhantomData,
        }
    }

    /// Make an HTTP request to the service.
    ///
    /// This is called by generated code - you typically don't need to call this directly.
    pub async fn request<B, R>(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<&B>,
    ) -> Result<R>
    where
        B: Serialize,
        R: DeserializeOwned,
    {
        let url = format!("{}{}", self.base_url, path);

        let mut request = self.client.request(method.clone(), &url);

        // Add query parameters
        if !query.is_empty() {
            request = request.query(query);
        }

        // Add JSON body for POST/PUT/PATCH
        if let Some(body) = body {
            request = request.json(body);
        }

        let response = request.send().await?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            anyhow::bail!("HTTP {} {}: {}", status.as_u16(), status.as_str(), text);
        }

        let result: R = response.json().await?;
        Ok(result)
    }
}
