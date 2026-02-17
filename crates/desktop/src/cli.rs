use std::fmt::{Display, Formatter};

const USAGE: &str = "Usage: kanban [--server] [--port <u16>|--port=<u16>]";

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CliArgs {
    pub server: bool,
    pub port: Option<u16>,
}

impl CliArgs {
    pub fn parse_env() -> Result<Self, CliError> {
        Self::parse_from(std::env::args().skip(1))
    }

    pub fn parse_from<I, S>(raw_args: I) -> Result<Self, CliError>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut args = raw_args.into_iter().map(Into::into).peekable();
        let mut parsed = CliArgs::default();

        while let Some(arg) = args.next() {
            if arg.starts_with("-psn_") {
                // Finder launches on macOS can inject a process serial number argument.
                continue;
            }

            if arg == "--server" {
                parsed.server = true;
                continue;
            }

            if let Some(value) = arg.strip_prefix("--port=") {
                parsed.port = Some(parse_port_value(value)?);
                continue;
            }

            if arg == "--port" {
                let value = args.next().ok_or(CliError::MissingPortValue)?;
                parsed.port = Some(parse_port_value(&value)?);
                continue;
            }

            if arg.starts_with("--") {
                return Err(CliError::UnknownFlag(arg));
            }

            return Err(CliError::UnexpectedArgument(arg));
        }

        if parsed.port.is_some() && !parsed.server {
            return Err(CliError::PortRequiresServer);
        }

        Ok(parsed)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliError {
    MissingPortValue,
    InvalidPort(String),
    UnknownFlag(String),
    UnexpectedArgument(String),
    PortRequiresServer,
}

impl Display for CliError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingPortValue => write!(f, "Missing value for --port.\n{USAGE}"),
            Self::InvalidPort(value) => {
                write!(f, "Invalid port '{value}'. Expected 0-65535.\n{USAGE}")
            }
            Self::UnknownFlag(flag) => write!(f, "Unknown option '{flag}'.\n{USAGE}"),
            Self::UnexpectedArgument(arg) => {
                write!(f, "Unexpected argument '{arg}'.\n{USAGE}")
            }
            Self::PortRequiresServer => {
                write!(f, "--port is only supported with --server.\n{USAGE}")
            }
        }
    }
}

impl std::error::Error for CliError {}

fn parse_port_value(raw: &str) -> Result<u16, CliError> {
    raw.trim()
        .parse::<u16>()
        .map_err(|_| CliError::InvalidPort(raw.to_string()))
}

#[cfg(test)]
mod tests {
    use super::{CliArgs, CliError};

    #[test]
    fn accepts_server_flag() {
        let parsed = CliArgs::parse_from(["--server"]).expect("expected --server to parse");
        assert!(parsed.server);
        assert_eq!(parsed.port, None);
    }

    #[test]
    fn accepts_space_separated_port() {
        let parsed =
            CliArgs::parse_from(["--server", "--port", "8080"]).expect("expected --port to parse");
        assert!(parsed.server);
        assert_eq!(parsed.port, Some(8080));
    }

    #[test]
    fn accepts_equals_port() {
        let parsed =
            CliArgs::parse_from(["--server", "--port=8080"]).expect("expected --port to parse");
        assert!(parsed.server);
        assert_eq!(parsed.port, Some(8080));
    }

    #[test]
    fn rejects_missing_port_value() {
        let err = CliArgs::parse_from(["--server", "--port"]).expect_err("expected parse failure");
        assert_eq!(err, CliError::MissingPortValue);
    }

    #[test]
    fn rejects_invalid_port_value() {
        let err =
            CliArgs::parse_from(["--server", "--port=abc"]).expect_err("expected parse failure");
        assert_eq!(err, CliError::InvalidPort("abc".to_string()));
    }

    #[test]
    fn rejects_unknown_flags() {
        let err = CliArgs::parse_from(["--server", "--unknown"]).expect_err("expected failure");
        assert_eq!(err, CliError::UnknownFlag("--unknown".to_string()));
    }

    #[test]
    fn ignores_macos_finder_psn_argument() {
        let parsed = CliArgs::parse_from(["-psn_0_12345"]).expect("expected parse success");
        assert_eq!(parsed, CliArgs::default());
    }
}
