use axum::extract::Path;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};

include!(concat!(env!("OUT_DIR"), "/docs_assets.rs"));

/// Redirect `/docs` to `/docs/` so mdBook relative asset paths resolve.
pub async fn docs_redirect() -> Redirect {
  Redirect::permanent("/docs/")
}

/// Serve `/docs/`.
pub async fn docs_index(request_headers: HeaderMap) -> Response {
  serve_docs_path("", request_headers)
}

/// Serve embedded mdBook documentation and bot-facing docs assets.
pub async fn docs_asset(Path(path): Path<String>, request_headers: HeaderMap) -> Response {
  serve_docs_path(&path, request_headers)
}

/// Whether this binary embedded a full mdBook build rather than the fallback.
pub fn docs_built_with_mdbook() -> bool {
  DOCS_BUILT_WITH_MDBOOK
}

fn serve_docs_path(path: &str, request_headers: HeaderMap) -> Response {
  let requested = normalize_docs_path(&path);
  let Some(asset) = find_doc_asset(&requested) else {
    return (StatusCode::NOT_FOUND, [(header::CONTENT_TYPE, "text/plain; charset=utf-8")], "Documentation asset not found").into_response();
  };

  let etag = docs_etag();
  if let Some(if_none_match) = request_headers.get(header::IF_NONE_MATCH) {
    if if_none_match.as_bytes() == etag.as_bytes() {
      return (StatusCode::NOT_MODIFIED, [(header::ETAG, etag), (header::CACHE_CONTROL, "public, max-age=300, must-revalidate")])
        .into_response();
    }
  }

  (
    StatusCode::OK,
    [(header::CONTENT_TYPE, asset.content_type), (header::CACHE_CONTROL, "public, max-age=300, must-revalidate"), (header::ETAG, etag)],
    asset.bytes,
  )
    .into_response()
}

fn normalize_docs_path(path: &str) -> String {
  let trimmed = path.trim_start_matches('/');
  if trimmed.is_empty() || trimmed.ends_with('/') {
    return format!("{}index.html", trimmed);
  }
  trimmed.to_string()
}

fn find_doc_asset(path: &str) -> Option<&'static EmbeddedDocAsset> {
  DOC_ASSETS.iter().find(|asset| asset.path == path)
}

fn docs_etag() -> &'static str {
  static ETAG: std::sync::OnceLock<String> = std::sync::OnceLock::new();
  ETAG.get_or_init(|| {
    let mut hash = 0xcbf29ce484222325u64;
    for asset in DOC_ASSETS {
      for byte in asset.path.as_bytes().iter().chain(asset.bytes.iter()) {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
      }
    }
    format!("\"docs-{}-{hash:016x}\"", env!("CARGO_PKG_VERSION"))
  })
}
