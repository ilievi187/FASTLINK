//! Command-line options for the `fastlink-gui` binary.
//!
//! The CLI path is the only configurable input. Resolution order is explicit:
//! `--fastlink-bin PATH`, then `FASTLINK_BIN`, then `fastlink` on `PATH`.

use std::ffi::OsString;
use std::path::PathBuf;

use crate::inventory::{InventoryCommand, DEFAULT_PROGRAM, PROGRAM_ENV};

/// What the binary should do after parsing arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Options {
    Run(InventoryCommand),
    Help,
    Version,
}

impl Options {
    /// Parse process arguments, skipping the executable name.
    pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Self, String> {
        Self::parse_with(args, std::env::var_os(PROGRAM_ENV))
    }

    /// Parse with an injected environment value so tests never mutate the
    /// process environment.
    pub fn parse_with(
        args: impl IntoIterator<Item = OsString>,
        env_program: Option<OsString>,
    ) -> Result<Self, String> {
        let mut args = args.into_iter();
        let mut cli_program: Option<PathBuf> = None;

        while let Some(arg) = args.next() {
            let Some(text) = arg.to_str() else {
                return Err(format!("argument {arg:?} is not valid UTF-8"));
            };
            match text {
                "-h" | "--help" => return Ok(Self::Help),
                "-V" | "--version" => return Ok(Self::Version),
                "--fastlink-bin" => {
                    let value = args
                        .next()
                        .ok_or_else(|| "--fastlink-bin requires a path".to_owned())?;
                    set_program(&mut cli_program, value)?;
                }
                _ if text.starts_with("--fastlink-bin=") => {
                    let value = text
                        .strip_prefix("--fastlink-bin=")
                        .expect("prefix checked above");
                    set_program(&mut cli_program, OsString::from(value))?;
                }
                _ => {
                    return Err(format!(
                        "unrecognized argument {text:?}; run with --help to see supported options"
                    ));
                }
            }
        }

        Ok(Self::Run(InventoryCommand::resolve(
            cli_program,
            env_program,
        )))
    }

    /// Short usage text shown by `--help` and on argument errors.
    #[must_use]
    pub fn usage() -> String {
        format!(
            "FASTLINK desktop shell — local device overview (pre-alpha)\n\
             \n\
             Usage:\n  fastlink-gui [--fastlink-bin PATH]\n\
             \n\
             Options:\n\
             \x20 --fastlink-bin PATH  FASTLINK CLI invoked as `<PATH> devices --json`\n\
             \x20                      (default: ${PROGRAM_ENV}, then `{DEFAULT_PROGRAM}` on PATH)\n\
             \x20 -h, --help           Print this help\n\
             \x20 -V, --version        Print the GUI version\n\
             \n\
             The GUI reads the local-only fastlink.devices.v1 inventory and shows the\n\
             opaque hashed node_id reported by the CLI. It performs no network scans,\n\
             listening sockets, model downloads, inference, or GPU compute."
        )
    }
}

fn set_program(slot: &mut Option<PathBuf>, value: OsString) -> Result<(), String> {
    if slot.is_some() {
        return Err("--fastlink-bin was provided more than once".to_owned());
    }
    let path = PathBuf::from(value);
    if path.as_os_str().is_empty() {
        return Err("--fastlink-bin requires a non-empty path".to_owned());
    }
    *slot = Some(path);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn defaults_to_path_lookup() {
        let options = Options::parse_with(args(&[]), None).unwrap();
        let Options::Run(command) = options else {
            panic!("expected run options");
        };
        assert_eq!(command.program(), std::path::Path::new(DEFAULT_PROGRAM));
    }

    #[test]
    fn environment_is_used_before_the_default() {
        let env = OsString::from("/opt/fastlink-env");
        let options = Options::parse_with(args(&[]), Some(env)).unwrap();
        let Options::Run(command) = options else {
            panic!("expected run options");
        };
        assert_eq!(command.program(), std::path::Path::new("/opt/fastlink-env"));
    }

    #[test]
    fn cli_path_wins_over_environment() {
        let env = OsString::from("/opt/fastlink-env");
        let options =
            Options::parse_with(args(&["--fastlink-bin", "/opt/fastlink-cli"]), Some(env)).unwrap();
        let Options::Run(command) = options else {
            panic!("expected run options");
        };
        assert_eq!(command.program(), std::path::Path::new("/opt/fastlink-cli"));
    }

    #[test]
    fn equals_form_is_accepted() {
        let options = Options::parse_with(args(&["--fastlink-bin=/opt/fastlink"]), None).unwrap();
        let Options::Run(command) = options else {
            panic!("expected run options");
        };
        assert_eq!(command.program(), std::path::Path::new("/opt/fastlink"));
    }

    #[test]
    fn help_and_version_are_terminating() {
        assert_eq!(
            Options::parse_with(args(&["--help", "--bogus"]), None).unwrap(),
            Options::Help
        );
        assert_eq!(
            Options::parse_with(args(&["-V"]), None).unwrap(),
            Options::Version
        );
        assert!(Options::usage().contains("--fastlink-bin"));
        assert!(Options::usage().contains("no network"));
    }

    #[test]
    fn missing_duplicate_and_empty_paths_are_rejected() {
        assert!(Options::parse_with(args(&["--fastlink-bin"]), None)
            .unwrap_err()
            .contains("requires a path"));
        assert!(Options::parse_with(
            args(&["--fastlink-bin", "/a", "--fastlink-bin", "/b"]),
            None
        )
        .unwrap_err()
        .contains("more than once"));
        assert!(Options::parse_with(args(&["--fastlink-bin="]), None)
            .unwrap_err()
            .contains("non-empty"));
        assert!(Options::parse_with(args(&["--fastlink-bin", ""]), None)
            .unwrap_err()
            .contains("non-empty"));
    }

    #[test]
    fn unknown_positional_arguments_are_rejected() {
        let error = Options::parse_with(args(&["devices"]), None).unwrap_err();
        assert!(error.contains("unrecognized argument"));
    }
}
