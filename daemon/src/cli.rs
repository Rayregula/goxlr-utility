use clap::{Parser, ValueEnum};
use directories::ProjectDirs;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[clap(about, version, author)]
pub struct Cli {
    /// Minimum log level to print out
    #[clap(long, value_enum, default_value = "warn")]
    pub log_level: LevelFilter,

    /// Location of the daemon configuration file on disk
    #[clap(long, default_value_os_t = default_config_location())]
    pub config: PathBuf,

    /// Disable the HTTP Server and Client Web UI
    #[clap(long)]
    pub disable_http: bool,
}

fn default_config_location() -> PathBuf {
    let proj_dirs = ProjectDirs::from("org", "GoXLR-on-Linux", "GoXLR-Utility")
        .expect("Couldn't find project directory");

    proj_dirs.config_dir().join("settings.json")
}

#[repr(usize)]
#[derive(ValueEnum, Copy, Clone, Eq, PartialEq, Debug)]
pub enum LevelFilter {
    /// A level lower than all log levels.
    Off,
    /// Corresponds to the `Error` log level.
    Error,
    /// Corresponds to the `Warn` log level.
    Warn,
    /// Corresponds to the `Info` log level.
    Info,
    /// Corresponds to the `Debug` log level.
    Debug,
    /// Corresponds to the `Trace` log level.
    Trace,
}
