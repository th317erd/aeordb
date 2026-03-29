use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Instant;

use axum::http::{Request, Response};
use tower::{Layer, Service};

use super::definitions::{
  HTTP_REQUESTS_TOTAL, HTTP_REQUEST_BYTES, HTTP_REQUEST_DURATION, HTTP_RESPONSE_BYTES,
};

/// A tower Layer that records HTTP metrics for every request.
#[derive(Clone, Debug)]
pub struct HttpMetricsLayer;

impl<S> Layer<S> for HttpMetricsLayer {
  type Service = HttpMetricsService<S>;

  fn layer(&self, inner: S) -> Self::Service {
    HttpMetricsService { inner }
  }
}

/// The tower Service wrapping an inner service to record HTTP metrics.
#[derive(Clone, Debug)]
pub struct HttpMetricsService<S> {
  inner: S,
}

impl<S, ReqBody, ResBody> Service<Request<ReqBody>> for HttpMetricsService<S>
where
  S: Service<Request<ReqBody>, Response = Response<ResBody>> + Clone + Send + 'static,
  S::Future: Send + 'static,
  ReqBody: http_body::Body + Send + 'static,
  ResBody: http_body::Body + 'static,
{
  type Response = S::Response;
  type Error = S::Error;
  type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

  fn poll_ready(&mut self, context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
    self.inner.poll_ready(context)
  }

  fn call(&mut self, request: Request<ReqBody>) -> Self::Future {
    let method = request.method().to_string();
    let path = request.uri().path().to_string();
    let request_size = request
      .body()
      .size_hint()
      .exact()
      .unwrap_or(0);

    let mut inner = self.inner.clone();
    let start = Instant::now();

    Box::pin(async move {
      let response = inner.call(request).await?;

      let duration = start.elapsed().as_secs_f64();
      let status = response.status().as_u16().to_string();
      let response_size = response
        .body()
        .size_hint()
        .exact()
        .unwrap_or(0);

      metrics::counter!(HTTP_REQUESTS_TOTAL, "method" => method.clone(), "path" => path.clone(), "status" => status.clone()).increment(1);
      metrics::histogram!(HTTP_REQUEST_DURATION, "method" => method.clone(), "path" => path.clone(), "status" => status).record(duration);
      metrics::counter!(HTTP_REQUEST_BYTES, "method" => method.clone(), "path" => path.clone()).increment(request_size);
      metrics::counter!(HTTP_RESPONSE_BYTES, "method" => method, "path" => path).increment(response_size);

      Ok(response)
    })
  }
}
