mod cli;

use anyhow::Result;
use clap::Parser;

fn main() -> Result<()> {
    let args = cli::Cli::parse();
    let ctx = cli::CliContext::resolve(&args.config)?;

    // Derive the tracing filter from --quiet / --verbose, unless RUST_LOG is set.
    let filter = if args.quiet {
        tracing_subscriber::EnvFilter::new("warn")
    } else {
        let level = match args.verbose {
            0 => "info",
            1 => "debug",
            _ => "trace",
        };
        tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(level))
    };

    tracing_subscriber::fmt().with_env_filter(filter).init();

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(args.run(&ctx))
}
