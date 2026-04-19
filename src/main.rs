mod actions;
mod collect;
mod diagnose;
mod display;

use clap::Parser;

#[derive(Parser, Debug)]
#[command(
    name = "stop",
    about = "Smart top: diagnoses what's making your computer slow and shows who to blame",
    version
)]
struct Args {
    /// One-shot snapshot instead of the default continuous refresh.
    #[arg(short = '1', long)]
    once: bool,

    /// Refresh interval in seconds (watch mode only)
    #[arg(short, long, default_value_t = 2, value_name = "N")]
    interval: u64,

    /// Number of processes shown per category
    #[arg(short = 'n', long = "top", default_value_t = 5, value_name = "N")]
    top: usize,

    /// Output raw metrics as JSON and exit
    #[arg(long)]
    json: bool,

    /// Disable ANSI color
    #[arg(long)]
    no_color: bool,
}

fn main() -> std::io::Result<()> {
    let args = Args::parse();

    if args.json {
        // Single-shot JSON output
        let metrics = collect::collect(1_000, args.top);
        let report = diagnose::diagnose(&metrics);
        let out = serde_json::json!({
            "metrics": metrics,
            "report": report,
        });
        println!("{}", serde_json::to_string_pretty(&out).unwrap());
        return Ok(());
    }

    if args.once {
        let metrics = collect::collect(1_000, args.top);
        display::render_once(&metrics)
    } else {
        display::render_watch(args.interval, args.top, args.no_color)
    }
}
