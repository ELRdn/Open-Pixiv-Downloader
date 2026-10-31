mod archive;
mod error;
mod folder;
mod jobs;
mod novel;
mod target;
mod transport;
mod tui;

use clap::{Args, Parser, Subcommand};
use jobs::Options;
use std::{
    io::{self, IsTerminal},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

#[derive(Parser)]
#[command(
    name = "opixiv",
    version,
    about = "Open Pixiv Downloader — local archive CLI + Ratatui"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}
#[derive(Subcommand)]
enum Command {
    /// Launch the interactive terminal interface
    #[command(alias = "interactive")]
    Tui,
    /// List target IDs without downloading media
    Plan(FetchArgs),
    /// Save works with checksums and resumable state
    Fetch(FetchArgs),
    /// Verify saved checksums without network access
    Verify { out: PathBuf },
    /// Export verified complete works to ZIP
    Pack { out: PathBuf, zip: PathBuf },
}
#[derive(Args)]
struct FetchArgs {
    /// Work, author or novel-series URLs (also art:ID / novel:ID / user:ID / series:ID)
    targets: Vec<String>,
    #[arg(long)]
    input: Option<PathBuf>,
    #[arg(long, default_value = "archive")]
    out: PathBuf,
    #[arg(long,default_value_t=2,value_parser=clap::value_parser!(u8).range(1..=4))]
    jobs: u8,
    #[arg(long, conflicts_with = "limit")]
    all: bool,
    #[arg(long, default_value_t = 100)]
    limit: usize,
    #[arg(long,default_value="all",value_parser=["all","art","novel"])]
    types: String,
    #[arg(long,conflicts_with_all=["fixture","demo"])]
    online: bool,
    #[arg(long,conflicts_with_all=["online","demo"])]
    fixture: Option<PathBuf>,
    /// Use the embedded synthetic fixture (no Python or network needed)
    #[arg(long,conflicts_with_all=["online","fixture"])]
    demo: bool,
    #[arg(long, default_value_t = 2.0)]
    interval: f64,
    #[arg(long, default_value_t = 500)]
    request_budget: usize,
    #[arg(long, default_value_t = 256)]
    max_file_mib: u64,
    #[arg(long)]
    retry_unavailable: bool,
}
impl From<FetchArgs> for Options {
    fn from(a: FetchArgs) -> Self {
        Self {
            targets: a.targets,
            input: a.input,
            out: a.out,
            jobs: a.jobs.into(),
            all: a.all,
            limit: a.limit,
            types: a.types,
            online: a.online,
            fixture: a.fixture,
            demo: a.demo,
            interval: a.interval,
            request_budget: a.request_budget,
            max_file_mib: a.max_file_mib,
            retry_unavailable: a.retry_unavailable,
        }
    }
}
fn execute() -> error::Result<i32> {
    let cli = Cli::parse();
    if matches!(cli.command, None | Some(Command::Tui)) {
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
            return Err(error::invalid(
                "TUI requires an interactive terminal. Use opixiv --help for CLI commands.",
            ));
        }
        return tui::run().map(|()| 0);
    }
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    let interrupted = Arc::new(AtomicBool::new(false));
    let interrupt_flag = interrupted.clone();
    ctrlc::set_handler(move || {
        interrupt_flag.store(true, Ordering::SeqCst);
        flag.store(true, Ordering::SeqCst);
    })
    .map_err(|_| error::invalid("Unable to register interrupt handler"))?;
    let result = match cli.command.unwrap() {
        Command::Plan(args) => jobs::run(args.into(), true, stop.clone(), |_| {})?,
        Command::Fetch(args) => jobs::run(args.into(), false, stop.clone(), |_| {})?,
        Command::Verify { out } => archive::Archive::open(&out, true)?.verify()?,
        Command::Pack { out, zip } => archive::Archive::open(&out, true)?.pack(&zip)?,
        Command::Tui => unreachable!(),
    };
    println!("{}", serde_json::to_string_pretty(&result)?);
    if interrupted.load(Ordering::SeqCst) {
        return Ok(130);
    }
    Ok(
        if result
            .get("problems")
            .is_some_and(|v| v.as_array().is_some_and(|a| !a.is_empty()))
            || result
                .get("status")
                .is_some_and(|s| !matches!(s.as_str(), Some("complete" | "planned")))
        {
            1
        } else {
            0
        },
    )
}
fn main() {
    let code = match execute() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("Error: {e}");
            2
        }
    };
    std::process::exit(code);
}
