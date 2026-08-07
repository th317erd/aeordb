use std::collections::BTreeMap;
use std::ffi::OsString;

use aeordb::engine::config_resolver::CommandLineConfigOverrides;
use aeordb::engine::v4::contract_generated::CONFIGURATION_PROPERTIES;
use clap::builder::OsStringValueParser;
use clap::{Arg, ArgMatches, Command};

const CONFIGURATION_HELP_HEADING: &str = "Runtime and lifecycle configuration";

pub fn with_configuration_arguments(command: Command) -> Command {
  command.mut_subcommand("start", |start| {
    CONFIGURATION_PROPERTIES.iter().fold(start, |start, property| {
      start.arg(
        Arg::new(property.path)
          .long(property.cli.trim_start_matches("--"))
          .value_name(property.kind)
          .value_parser(OsStringValueParser::new())
          .allow_hyphen_values(true)
          .help(property.path)
          .help_heading(CONFIGURATION_HELP_HEADING),
      )
    })
  })
}

pub fn collect_configuration_overrides(matches: &ArgMatches) -> Result<CommandLineConfigOverrides, String> {
  let mut values = BTreeMap::<String, OsString>::new();
  let Some(start) = matches.subcommand_matches("start") else {
    return CommandLineConfigOverrides::from_registered(values);
  };
  for property in CONFIGURATION_PROPERTIES {
    if let Some(value) = start.get_raw(property.path).and_then(|values| values.last()) {
      values.insert(property.cli.to_string(), value.to_os_string());
    }
  }
  CommandLineConfigOverrides::from_registered(values)
}
