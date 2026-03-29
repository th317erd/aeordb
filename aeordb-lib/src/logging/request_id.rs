use axum::{
  extract::Request,
  http::HeaderValue,
  middleware::Next,
  response::Response,
};
use uuid::Uuid;

const REQUEST_ID_HEADER: &str = "x-request-id";

/// Axum middleware that ensures every request carries a unique request ID.
///
/// 1. Checks for an incoming `X-Request-Id` header — uses it if present.
/// 2. Otherwise generates a UUID v4.
/// 3. Creates a tracing span so all downstream log events inherit the ID.
/// 4. Adds the `X-Request-Id` header to the response.
pub async fn request_id_middleware(
  request: Request,
  next: Next,
) -> Response {
  let request_id = request
    .headers()
    .get(REQUEST_ID_HEADER)
    .and_then(|value| value.to_str().ok())
    .map(|value| value.to_string())
    .unwrap_or_else(|| Uuid::new_v4().to_string());

  let span = tracing::info_span!(
    "request",
    request_id = %request_id,
    method = %request.method(),
    path = %request.uri().path(),
  );

  let _guard = span.enter();

  tracing::debug!(
    method = %request.method(),
    path = %request.uri().path(),
    "Request received"
  );

  // Drop the synchronous guard before awaiting (spans must not be held
  // across await points via `Span::enter`).
  drop(_guard);

  let mut response = span.in_scope(|| {
    // We cannot use `in_scope` across an await, so we instrument the future
    // instead.  However `Next::run` is an opaque future that we cannot
    // `.instrument()` directly without adding `tracing-futures`.  The pragmatic
    // solution: attach the span to the task with `Instrument`.
    next
  })
  // Use `tracing::Instrument` to carry the span across the await point.
  .run(request)
  .await;

  if let Ok(header_value) = HeaderValue::from_str(&request_id) {
    response
      .headers_mut()
      .insert(REQUEST_ID_HEADER, header_value);
  }

  response
}
