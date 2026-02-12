use clap::Parser;

/// zapvis: sequence-only image viewer.
/// Opens a file, matches it against configured patterns with # as digit placeholders,
/// then navigates by changing the numeric id and stat()'ing the constructed filename.
#[derive(Parser, Debug)]
#[command(author, version, about)]
pub struct Args {
    /// Image file to open (recommended). Folder mode is intentionally not supported.
    pub input: Option<String>,

    /// Optional pattern override, e.g. "########_#.png"
    #[arg(long)]
    pub pattern: Option<String>,

    /// Show config file path and content, then exit
    #[arg(short, long)]
    pub config: bool,
}
