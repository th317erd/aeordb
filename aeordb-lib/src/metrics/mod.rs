pub mod definitions;
pub mod http_metrics_layer;

use metrics_exporter_prometheus::PrometheusHandle;

/// Install the Prometheus recorder globally and return the handle used to
/// render metrics in Prometheus text format.
///
/// If a global recorder is already installed (e.g. from a previous call in
/// the same process), this returns a standalone handle instead of panicking.
pub fn initialize_metrics() -> PrometheusHandle {
  let builder = metrics_exporter_prometheus::PrometheusBuilder::new();
  match builder.install_recorder() {
    Ok(handle) => handle,
    Err(_) => {
      // A recorder is already installed; build a standalone handle.
      metrics_exporter_prometheus::PrometheusBuilder::new()
        .build_recorder()
        .handle()
    }
  }
}
