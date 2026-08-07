use std::collections::{BTreeMap, BTreeSet};

use aeordb::engine::v4::contract_generated::CONFIGURATION_PROPERTIES;
use aeordb_cli::configuration_cli::{collect_configuration_overrides, with_configuration_arguments};
use clap::{Command, error::ErrorKind};

fn command() -> Command {
  with_configuration_arguments(Command::new("aeordb").subcommand(Command::new("start")).subcommand(Command::new("verify")))
}

#[test]
fn start_exposes_exactly_the_frozen_configuration_cli_registry() {
  let command = command();
  let start = command.find_subcommand("start").unwrap();
  let actual = start.get_arguments().filter_map(|argument| argument.get_long()).map(|name| format!("--{name}")).collect::<BTreeSet<_>>();
  let expected = CONFIGURATION_PROPERTIES.iter().map(|property| property.cli.to_string()).collect::<BTreeSet<_>>();
  assert_eq!(actual, expected);
  assert_eq!(actual.len(), 41);
}

#[test]
fn parser_captures_raw_values_under_the_registered_cli_names() {
  let matches = command()
    .try_get_matches_from([
      "aeordb",
      "start",
      "--memory-hard-limit-bytes",
      "4GiB",
      "--lifecycle-snapshot-writes-enabled",
      "false",
      "--garbage-collection-mark-scratch-max-bytes",
      "null",
    ])
    .unwrap();
  let overrides = collect_configuration_overrides(&matches).unwrap();
  assert_eq!(overrides.len(), 3);
  assert_eq!(overrides.get("--memory-hard-limit-bytes").unwrap(), "4GiB");
  assert_eq!(overrides.get("--lifecycle-snapshot-writes-enabled").unwrap(), "false");
  assert_eq!(overrides.get("--garbage-collection-mark-scratch-max-bytes").unwrap(), "null");
}

#[test]
fn every_registered_argument_accepts_one_explicit_value() {
  for property in CONFIGURATION_PROPERTIES {
    let value = match property.kind {
      "boolean" => "true",
      "optional_bytes" => "null",
      "path_or_auto" => "/var/lib/aeordb/runtime",
      _ => "1",
    };
    let matches = command()
      .try_get_matches_from(["aeordb", "start", property.cli, value])
      .unwrap_or_else(|error| panic!("{} ({}) was not accepted: {error}", property.cli, property.path));
    let overrides = collect_configuration_overrides(&matches).unwrap();
    assert_eq!(overrides.len(), 1, "{}", property.cli);
    assert_eq!(overrides.get(property.cli).unwrap(), value, "{}", property.cli);
  }
}

#[test]
fn missing_duplicate_and_unknown_configuration_arguments_fail_closed() {
  let missing = command().try_get_matches_from(["aeordb", "start", "--memory-hard-limit-bytes"]).unwrap_err();
  assert_eq!(missing.kind(), ErrorKind::InvalidValue);

  let duplicate = command()
    .try_get_matches_from(["aeordb", "start", "--memory-hard-limit-bytes", "2GiB", "--memory-hard-limit-bytes", "4GiB"])
    .unwrap_err();
  assert_eq!(duplicate.kind(), ErrorKind::ArgumentConflict);

  let unknown = command().try_get_matches_from(["aeordb", "start", "--memory-hard-limit-bytez", "4GiB"]).unwrap_err();
  assert_eq!(unknown.kind(), ErrorKind::UnknownArgument);
}

#[test]
fn non_start_commands_never_receive_configuration_overrides() {
  let matches = command().try_get_matches_from(["aeordb", "verify"]).unwrap();
  assert!(collect_configuration_overrides(&matches).unwrap().is_empty());
}

#[test]
fn typed_override_container_rejects_names_outside_the_registry() {
  let values = BTreeMap::from([("--memory-hard-limit-bytez".to_string(), "4GiB".into())]);
  let error = aeordb::engine::config_resolver::CommandLineConfigOverrides::from_registered(values).unwrap_err();
  assert!(error.contains("unregistered"));
}
