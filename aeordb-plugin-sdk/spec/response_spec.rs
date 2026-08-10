use aeordb_plugin_sdk::{encode_plugin_response, PluginResponse};

#[test]
fn plugin_error_response_is_always_valid_json() {
  let response = PluginResponse::error(422, "bad \"value\"\nnext line");
  let body: serde_json::Value = serde_json::from_slice(&response.body).unwrap();
  assert_eq!(body["error"], "bad \"value\"\nnext line");
  assert_eq!(response.content_type.as_deref(), Some("application/json"));
}

#[test]
fn encoded_plugin_response_round_trips_the_complete_envelope() {
  let response = PluginResponse::text(201, "created");
  let decoded: PluginResponse = serde_json::from_slice(&encode_plugin_response(&response)).unwrap();
  assert_eq!(decoded.status_code, 201);
  assert_eq!(decoded.body, b"created");
  assert_eq!(decoded.content_type.as_deref(), Some("text/plain"));
}

#[test]
fn fallible_json_constructor_propagates_custom_serializer_failures() {
  struct FailingSerializer;

  impl serde::Serialize for FailingSerializer {
    fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
    where
      S: serde::Serializer,
    {
      Err(serde::ser::Error::custom("deliberate serializer failure"))
    }
  }

  let error = PluginResponse::json(200, &FailingSerializer).expect_err("fallible response construction must preserve serializer errors");
  assert!(error.to_string().contains("deliberate serializer failure"));
}
