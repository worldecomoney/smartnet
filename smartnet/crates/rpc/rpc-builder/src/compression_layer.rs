use jsonrpsee_http_client::{HttpBody, HttpRequest, HttpResponse};
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use tower::{Layer, Service};
use tower_http::compression::{Compression, CompressionLayer as TowerCompressionLayer};

/// This layer is a wrapper around [`tower_http::compression::CompressionLayer`] that integrates
/// with jsonrpsee's HTTP types. It automatically compresses responses based on the client's
/// Accept-Encoding header.
#[expect(missing_debug_implementations)]
#[derive(Clone)]
pub struct CompressionLayer {
    inner_layer: TowerCompressionLayer,
}

impl CompressionLayer {
    /// Creates a new compression layer with gzip, brotli and deflate enabled.
    pub fn new() -> Self {
        Self { inner_layer: TowerCompressionLayer::new().gzip(true).br(true).deflate(true) }
    }
}

impl Default for CompressionLayer {
    /// Creates a new compression layer with default settings.
    /// See [`CompressionLayer::new`] for details.
    fn default() -> Self {
        Self::new()
    }
}

impl<S> Layer<S> for CompressionLayer {
    type Service = CompressionService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        CompressionService { compression: self.inner_layer.layer(inner) }
    }
}

/// Service that performs response compression.
///
/// Created by [`CompressionLayer`].
#[expect(missing_debug_implementations)]
#[derive(Clone)]
pub struct CompressionService<S> {
    compression: Compression<S>,
}

impl<S> Service<HttpRequest> for CompressionService<S>
where
    S: Service<HttpRequest, Response = HttpResponse>,
    S::Future: Send + 'static,
{
    type Response = HttpResponse;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.compression.poll_ready(cx)
    }

    fn call(&mut self, req: HttpRequest) -> Self::Future {
        let fut = self.compression.call(req);

        Box::pin(async move {
            let resp = fut.await?;
            let (parts, compressed_body) = resp.into_parts();
            let http_body = HttpBody::new(compressed_body);

            Ok(Self::Response::from_parts(parts, http_body))
        })
    }
}
