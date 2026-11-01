mod archive;
mod collection;
mod error;
mod folder;
mod jobs;
mod media;
mod novel;
mod target;
mod transport;
mod tui;
mod update;

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
    /// Disable automatic GitHub update checks in the TUI
    #[arg(long, global = true)]
    no_update_check: bool,
    #[command(subcommand)]
    command: Option<Command>,
}
#[derive(Subcommand)]
enum Command {
    /// Launch the interactive terminal interface
    #[command(alias = "interactive")]
    Tui,
    /// Update this executable from the official main branch
    Update {
        /// Check the latest version without installing
        #[arg(long, conflicts_with = "force")]
        check: bool,
        /// Rebuild/reinstall without the advisory version check
        #[arg(long)]
        force: bool,
    },
    /// List target IDs without downloading media
    Plan(FetchArgs),
    /// Save works with checksums and resumable state
    Fetch(FetchArgs),
    /// Verify saved checksums without network access
    Verify { out: PathBuf },
    /// Collect saved images and plain novel text into works/image and works/novel
    Collect { out: PathBuf },
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
    /// Global request start interval; defaults to 2 seconds, or 1 with --balanced
    #[arg(long)]
    interval: Option<f64>,
    /// Separate API/media timing (API >=2 seconds, global/media >=1 second)
    #[arg(long)]
    balanced: bool,
    /// Resume the saved job targets instead of discovering the author again
    #[arg(long)]
    resume: bool,
    #[arg(long, default_value_t = 500, value_parser = clap::value_parser!(u32).range(1..=100_000))]
    request_budget: u32,
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
            interval: a.interval.unwrap_or(if a.balanced { 1.0 } else { 2.0 }),
            balanced: a.balanced,
            resume: a.resume,
            request_budget: a.request_budget as usize,
            max_file_mib: a.max_file_mib,
            retry_unavailable: a.retry_unavailable,
        }
    }
}
fn execute() -> error::Result<i32> {
    let cli = Cli::parse();
    if let Some(Command::Update { check, force }) = &cli.command {
        return update::install(*check, *force);
    }
    if matches!(cli.command, None | Some(Command::Tui)) {
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
            return Err(error::invalid(
                "TUI requires an interactive terminal. Use opixiv --help for CLI commands.",
            ));
        }
        let automatic =
            !cli.no_update_check && std::env::var_os("OPIXIV_NO_UPDATE_CHECK").is_none();
        return tui::run(automatic).map(|()| 0);
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
        Command::Collect { out } => {
            archive::Archive::open(&out, true)?.collect(&|| stop.load(Ordering::SeqCst))?
        }
        Command::Pack { out, zip } => archive::Archive::open(&out, true)?.pack(&zip)?,
        Command::Tui | Command::Update { .. } => unreachable!(),
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

#[cfg(test)]
mod cli_tests {
    use super::*;
    #[test]
    fn update_cli_supports_check_and_force_as_separate_modes() {
        for args in [
            vec!["opixiv", "update"],
            vec!["opixiv", "update", "--check"],
            vec!["opixiv", "update", "--force"],
        ] {
            assert!(matches!(
                Cli::try_parse_from(args).unwrap().command,
                Some(Command::Update { .. })
            ));
        }
        assert!(Cli::try_parse_from(["opixiv", "update", "--check", "--force"]).is_err());
    }
    fn options(args: &[&str]) -> Options {
        let cli = Cli::try_parse_from(args).unwrap();
        let Some(Command::Fetch(args)) = cli.command else {
            panic!("expected fetch");
        };
        args.into()
    }
    #[test]
    fn profile_defaults_and_explicit_interval_are_distinct() {
        let legacy = options(&["opixiv", "fetch", "art:1", "--online"]);
        assert_eq!(legacy.interval, 2.0);
        assert_eq!(legacy.request_budget, 500);
        assert!(!legacy.balanced);
        let balanced = options(&["opixiv", "fetch", "art:1", "--online", "--balanced"]);
        assert_eq!(balanced.interval, 1.0);
        assert!(balanced.balanced);
        let explicit = options(&[
            "opixiv",
            "fetch",
            "art:1",
            "--online",
            "--balanced",
            "--interval",
            "3",
        ]);
        assert_eq!(explicit.interval, 3.0);
    }
    #[test]
    fn resume_accepts_no_targets_and_budget_is_finite() {
        let resumed = options(&[
            "opixiv",
            "fetch",
            "--resume",
            "--online",
            "--request-budget",
            "20000",
        ]);
        assert!(resumed.resume);
        assert!(resumed.targets.is_empty());
        assert_eq!(resumed.request_budget, 20_000);
        for value in ["0", "100001"] {
            assert!(Cli::try_parse_from([
                "opixiv",
                "fetch",
                "--online",
                "--request-budget",
                value
            ])
            .is_err());
        }
    }
}
