pub mod definitions;
pub mod http_metrics_layer;

use std::sync::OnceLock;

use metrics_exporter_prometheus::PrometheusHandle;

static GLOBAL_HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();

/// Install the Prometheus recorder globally and return the handle used to
/// render metrics in Prometheus text format.
///
/// Safe to call multiple times -- only the first call installs the recorder;
/// subsequent calls return the same handle.
pub fn initialize_metrics() -> PrometheusHandle {
  GLOBAL_HANDLE
    .get_or_init(|| {
      metrics_exporter_prometheus::PrometheusBuilder::new()
        .install_recorder()
        .expect("failed to install Prometheus metrics recorder")
    })
    .clone()
}
