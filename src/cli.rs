use clap::{ArgGroup, Parser, ValueEnum};
use lazy_static::lazy_static;

#[derive(ValueEnum, Clone, Debug, Copy, PartialEq, Eq)]
pub enum KeypairType {
    X25519,
    X448,
}

fn version_string() -> &'static str {
    option_env!("VERGEN_GIT_DESCRIBE")
        .or(option_env!("VERGEN_GIT_SEMVER"))
        .unwrap_or(env!("CARGO_PKG_VERSION"))
}

lazy_static! {
    static ref LONG_VERSION: String = format!(
        "
Build Timestamp:     {}
Build Version:       {}
Commit SHA:          {:?}
Commit Date:         {:?}
Commit Branch:       {:?}
cargo Target Triple: {}
cargo Opt Level:     {}
cargo Features:      {}
",
        option_env!("VERGEN_BUILD_TIMESTAMP").unwrap_or("unknown"),
        version_string(),
        option_env!("VERGEN_GIT_SHA"),
        option_env!("VERGEN_GIT_COMMIT_TIMESTAMP"),
        option_env!("VERGEN_GIT_BRANCH"),
        option_env!("VERGEN_CARGO_TARGET_TRIPLE").unwrap_or("unknown"),
        option_env!("VERGEN_CARGO_OPT_LEVEL").unwrap_or("unknown"),
        option_env!("VERGEN_CARGO_FEATURES").unwrap_or("unknown")
    );
}

#[derive(Parser, Debug, Default, Clone)]
#[command(
    about,
    version = version_string(),
    long_version(LONG_VERSION.as_str()),
    next_display_order = None
)]
#[command(group(
    ArgGroup::new("cmds")
        .required(true)
        .args(["config_path", "genkey"]),
))]
pub struct Cli {
    /// The path to the configuration file
    ///
    /// Running as a client or a server is automatically determined
    /// according to the configuration file.
    #[arg(value_name = "CONFIG")]
    pub config_path: Option<std::path::PathBuf>,

    /// Run as a server
    #[arg(long, short, group = "mode")]
    pub server: bool,

    /// Run as a client
    #[arg(long, short, group = "mode")]
    pub client: bool,

    /// Generate a keypair for the use of the noise protocol
    ///
    /// The DH function to use is x25519
    #[arg(
        long,
        value_enum,
        value_name = "CURVE",
        num_args = 0..=1,
        default_missing_value = "x25519"
    )]
    pub genkey: Option<KeypairType>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{CommandFactory, Parser};

    #[test]
    fn parses_positional_config_path() {
        let cli = Cli::try_parse_from(["rathole", "config.toml"]).expect("config path should parse");
        assert_eq!(cli.config_path.unwrap(), std::path::PathBuf::from("config.toml"));
        assert_eq!(cli.genkey, None);
    }

    #[test]
    fn parses_genkey_without_curve_as_default_curve() {
        let cli = Cli::try_parse_from(["rathole", "--genkey"]).expect("genkey should parse");
        assert_eq!(cli.genkey, Some(KeypairType::X25519));
        assert_eq!(cli.config_path, None);
    }

    #[test]
    fn parses_genkey_with_explicit_curve() {
        let cli =
            Cli::try_parse_from(["rathole", "--genkey", "x448"]).expect("curve should parse");
        assert_eq!(cli.genkey, Some(KeypairType::X448));
    }

    #[test]
    fn help_mentions_compatibility_flags() {
        let help = Cli::command().render_long_help().to_string();
        assert!(help.contains("--server"));
        assert!(help.contains("--client"));
        assert!(help.contains("--genkey"));
        assert!(help.contains("CONFIG"));
    }
}
