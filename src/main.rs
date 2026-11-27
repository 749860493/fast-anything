//! fast-anything: find the slow 5% of your app, rewrite just that in Rust, prove it behaves the same.

mod agent;
mod android;
mod backend;
mod bench;
mod cfamily;
mod csharp;
mod device;
mod doctor;
mod golang;
mod hotspot;
mod java;
mod kotlin;
mod mcp;
mod native;
mod node;
mod php;
mod pipeline;
mod python;
mod r;
mod report;
mod ruby;
mod sha256;
mod show;
mod state;
mod ui;
mod util;

use anyhow::{Result, bail, ensure};
use clap::{Args, Parser, Subcommand};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[derive(Parser)]
#[command(name = "fast-anything", version, about, arg_required_else_help = true)]
#[command(
    after_help = "One command:\n  fast-anything go --agent claude -- python app.py\n\nStep by step:\n  fast-anything scan -- python app.py\n  fast-anything port\n  fast-anything verify\n  fast-anything enable <id>\n  fast-anything bench"
)]
struct Cli {
    /// Project folder (default: the current folder)
    #[arg(short = 'C', long = "dir", global = true, default_value = ".")]
    dir: PathBuf,
    /// Print machine-readable JSON on stdout
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Everything in one go: scan, port the hottest functions with your agent, verify, switch on, benchmark
    Go {
        /// Coding agent that writes the Rust: claude, codex, or any command that reads a prompt on stdin
        #[arg(long)]
        agent: Option<String>,
        /// How many hot functions to port
        #[arg(long, default_value_t = 1)]
        top: usize,
        /// Agent attempts per function before giving up
        #[arg(long, default_value_t = 3)]
        attempts: usize,
        /// Benchmark rounds: each runs the original twice and Rust once, plus one warm-up per side (5 = 11 original runs, 6 Rust runs)
        #[arg(long, default_value_t = 5)]
        runs: usize,
        /// Real calls to record per function
        #[arg(long, default_value_t = 500)]
        samples: usize,
        /// How you run your app, after --
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
    /// Run your app under a profiler and rank what's worth porting
    Scan {
        /// Stop the app after this many seconds and rank what ran (Python, Node, Go, Java, Kotlin, Ruby, PHP, C, C++, R, C#: servers and GUIs that don't exit)
        #[arg(long, value_name = "SECS")]
        duration: Option<f64>,
        /// Also time the app 3 times without the profiler, to show how much profiling slowed it down
        #[arg(long, conflicts_with = "duration")]
        calibrate: bool,
        /// Rank an installed Android app's methods on a device (adb + simpleperf, debuggable or profileable build); default 10s
        #[arg(long, value_name = "PACKAGE", conflicts_with_all = ["calibrate", "explain"])]
        android: Option<String>,
        /// With --android: the device's serial, as adb devices lists it
        #[arg(long, requires = "android")]
        device: Option<String>,
        /// Say what the scan would profile and how (language, runtime, Android modules), without running the app
        #[arg(long)]
        explain: bool,
        /// How you run your app, after --
        #[arg(last = true, required_unless_present = "android")]
        command: Vec<String>,
    },
    /// Record a function's real calls and set up its Rust crate (default: the top candidate)
    Port {
        id: Option<String>,
        /// Have this coding agent write the port right away
        #[arg(long)]
        agent: Option<String>,
        #[arg(long, default_value_t = 3)]
        attempts: usize,
        #[arg(long, default_value_t = 500)]
        samples: usize,
    },
    /// Build the Rust port and replay every recorded call against it
    Verify {
        id: Option<String>,
        /// Verify every port (after a toolchain or dependency upgrade, or changes to the app)
        #[arg(long, conflicts_with = "id")]
        all: bool,
    },
    /// Switch your app over to the verified Rust port
    Enable {
        id: Option<String>,
        /// Enable even if it hasn't passed verify
        #[arg(long)]
        force: bool,
    },
    /// Switch back to the original code
    Disable {
        id: Option<String>,
        /// Switch every enabled port back
        #[arg(long, conflicts_with = "id")]
        all: bool,
    },
    /// Rebuild every port (after cloning, or a toolchain upgrade)
    Build,
    /// Remove compiler build cache (.fast/target) or all temporary state
    Clean {
        /// Remove all ports and reset .fast completely (disables any enabled ports first)
        #[arg(long)]
        all: bool,
    },
    /// Time your app with the originals vs the Rust ports
    Bench {
        /// Rounds: each runs the original twice (an A/A control for noise) and Rust once, plus one warm-up per side (5 = 11 original runs, 6 Rust runs)
        #[arg(long, default_value_t = 5)]
        runs: usize,
    },
    /// Attach evidence measured on an Android device; the report keeps it apart from the host numbers
    Import {
        #[command(subcommand)]
        what: Box<ImportCmd>,
    },
    /// Write a shareable report (markdown + SVG card)
    Report {
        /// Require on-device (ART) evidence from fast-anything import (it's included whenever there is some)
        #[arg(long)]
        include_android: bool,
    },
    /// Show the last scan and every port
    Status,
    /// Check that Rust, Python/Node and your agents are installed
    Doctor,
    /// Run as an MCP server on stdio (for Claude Code, Codex, Cursor, ...)
    Mcp,
    /// Replays recorded calls against a C-bridge port (used by verify, in its own process)
    #[command(name = "__replay", hide = true)]
    Replay { job: PathBuf },
}

#[derive(Subcommand)]
enum ImportCmd {
    /// Frame timing: the text of  adb shell dumpsys gfxinfo <package>
    Gfxinfo(ImportArgs),
    /// A report in fast-anything's on-device schema (version 1), e.g. made in CI
    Json(ImportArgs),
    /// A Perfetto trace, kept with its hash (not parsed yet)
    Perfetto {
        #[command(flatten)]
        args: ImportArgs,
        /// The R8/ProGuard mapping.txt of the build, recorded with the trace
        #[arg(long)]
        mapping: Option<PathBuf>,
    },
    /// A simpleperf perf.data, kept with its hash (not parsed yet)
    Simpleperf {
        #[command(flatten)]
        args: ImportArgs,
        /// The folder with the build's unstripped libraries, recorded with the profile
        #[arg(long)]
        symfs: Option<PathBuf>,
    },
}

/// What was measured: --device, --package, --android-version, --abi and --build-variant are required,
/// as flags or (for json) in the file.
#[derive(Args)]
struct ImportArgs {
    /// The file to import
    #[arg(long)]
    input: PathBuf,
    /// The app's package, e.g. com.example.app
    #[arg(long)]
    package: Option<String>,
    /// The device's serial, as adb devices lists it
    #[arg(long)]
    device: Option<String>,
    /// The device's model, e.g. 22041216G
    #[arg(long)]
    model: Option<String>,
    /// e.g. 14
    #[arg(long)]
    android_version: Option<String>,
    /// API level, e.g. 34
    #[arg(long)]
    api: Option<u32>,
    /// e.g. arm64-v8a
    #[arg(long)]
    abi: Option<String>,
    /// e.g. debug, release, benchmark
    #[arg(long)]
    build_variant: Option<String>,
    #[arg(long)]
    version_name: Option<String>,
    #[arg(long)]
    version_code: Option<u64>,
    /// Android's thermal status at capture, 0 (none) to 6 (shutdown)
    #[arg(long, value_parser = clap::value_parser!(u8).range(0..=6))]
    thermal_status: Option<u8>,
    /// Measured runs in the capture
    #[arg(long)]
    runs: Option<u32>,
    /// Warm-up runs before them
    #[arg(long)]
    warmup: Option<u32>,
    /// When it was captured, e.g. 2026-11-24T18:40:00Z
    #[arg(long)]
    captured_at: Option<String>,
    /// What captured it, e.g. "androidforge 1.0.0"
    #[arg(long)]
    captured_by: Option<String>,
    /// Where to keep it, inside .fast/android (default: .fast/android/<format>)
    #[arg(long)]
    out: Option<PathBuf>,
}

impl ImportCmd {
    fn into_import(self) -> android::Import {
        use android::Format;
        let (format, a, symbols) = match self {
            ImportCmd::Gfxinfo(a) => (Format::Gfxinfo, a, None),
            ImportCmd::Json(a) => (Format::Json, a, None),
            ImportCmd::Perfetto { args, mapping } => (Format::Perfetto, args, mapping),
            ImportCmd::Simpleperf { args, symfs } => (Format::Simpleperf, args, symfs),
        };
        let meta = android::Meta {
            package: a.package,
            device: a.device,
            model: a.model,
            android_version: a.android_version,
            api: a.api,
            abi: a.abi,
            build_variant: a.build_variant,
            version_name: a.version_name,
            version_code: a.version_code,
            thermal_status: a.thermal_status,
            runs: a.runs,
            warmup: a.warmup,
            captured_at: a.captured_at,
            captured_by: a.captured_by,
        };
        android::Import { format, input: a.input, meta, out: a.out, symbols }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let root = util::absolute(&cli.dir);
    match run(cli.cmd, &root, cli.json) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            ui::error(format!("{e:#}"));
            ExitCode::FAILURE
        }
    }
}

fn run(cmd: Cmd, root: &Path, json: bool) -> Result<()> {
    if let Cmd::Replay { job } = cmd {
        return native::replay(&job);
    }
    if json {
        match &cmd {
            Cmd::Go { .. } | Cmd::Build | Cmd::Clean { .. } | Cmd::Mcp => {
                bail!(
                    "--json isn't supported for this command (supported: scan, port, verify, enable, disable, bench, import, report, status, doctor)"
                );
            }
            _ => ui::set_quiet(true),
        }
    }
    if matches!(cmd, Cmd::Mcp) {
        return mcp::serve();
    }
    if !json {
        ui::banner();
    }
    match cmd {
        Cmd::Go { agent, top, attempts, runs, samples, command } => {
            pipeline::go(root, command, agent.as_deref(), top, attempts, runs, samples)
        }
        Cmd::Scan { android: Some(package), device, duration, .. } => {
            let opts = device::Options { package, device, seconds: duration.unwrap_or(device::DEFAULT_SECONDS) };
            let out = device::scan(root, &opts)?;
            if json {
                show::json(&out)
            } else {
                show::android_scan(&out);
                Ok(())
            }
        }
        Cmd::Scan { explain: true, command, .. } => {
            let out = pipeline::explain(root, &command)?;
            if json {
                show::json(&out)
            } else {
                show::explain(&out);
                Ok(())
            }
        }
        Cmd::Scan { command, duration, calibrate, .. } => {
            let out = pipeline::scan_with(root, command, duration, calibrate)?;
            if json {
                show::json(&show::scan_json(&out))
            } else {
                show::scan(&out);
                Ok(())
            }
        }
        Cmd::Port { id, agent, attempts, samples } => {
            let out = pipeline::port(root, id.as_deref(), samples)?;
            if let Some(agent) = agent {
                if !json {
                    show::port(&out, false);
                }
                let v = pipeline::agent_port(root, &out.id, &agent, attempts)?;
                if json {
                    show::json(&json!({ "port": out, "verify": show::verify_json(&out.id, &v) }))?;
                }
                if !v.ok() {
                    bail!("{agent} couldn't get {} right in {attempts} attempts", out.id);
                }
                if !json && !v.slower() && !v.too_cheap() {
                    show::next(&format!("fast-anything enable {}", out.id));
                }
            } else if json {
                show::json(&out)?;
            } else {
                show::port(&out, true);
            }
            Ok(())
        }
        Cmd::Verify { all: true, .. } => {
            let results = pipeline::verify_all(root)?;
            let failed: Vec<&str> = results.iter().filter(|(_, v)| !v.ok()).map(|(id, _)| id.as_str()).collect();
            if json {
                let ports: Vec<Value> = results.iter().map(|(id, v)| show::verify_json(id, v)).collect();
                show::json(&json!({ "ok": failed.is_empty(), "ports": ports }))?;
            }
            ensure!(
                failed.is_empty(),
                "{} of {} ports don't match: {}",
                failed.len(),
                results.len(),
                failed.join(", ")
            );
            ui::ok(format!("all {} ports match the original", results.len()));
            Ok(())
        }
        Cmd::Verify { id, .. } => {
            let (id, v) = pipeline::verify(root, id.as_deref())?;
            if json {
                show::json(&show::verify_json(&id, &v))?;
            } else {
                show::verify(&id, &v);
            }
            if !v.ok() {
                bail!("{id} doesn't match the original yet");
            }
            if !json && !v.slower() && !v.too_cheap() {
                show::next(&format!("fast-anything enable {id}"));
            }
            Ok(())
        }
        Cmd::Enable { id, force } => {
            let id = pipeline::enable(root, id.as_deref(), force)?;
            if json {
                return show::json(&json!({ "enabled": id }));
            }
            ui::ok(format!("{} now runs in Rust", ui::bold(&id)));
            show::next("fast-anything bench");
            Ok(())
        }
        Cmd::Disable { all: true, .. } => {
            let ids = pipeline::disable_all(root)?;
            if json {
                return show::json(&json!({ "disabled": ids }));
            }
            if ids.is_empty() {
                ui::ok("no port was switched on");
            }
            for id in ids {
                ui::ok(format!("{} is back to the original code", ui::bold(&id)));
            }
            Ok(())
        }
        Cmd::Disable { id, .. } => {
            let id = pipeline::disable(root, id.as_deref())?;
            if json {
                return show::json(&json!({ "disabled": id }));
            }
            ui::ok(format!("{} is back to the original code", ui::bold(&id)));
            Ok(())
        }
        Cmd::Build => pipeline::build_all(root),
        Cmd::Clean { all } => pipeline::clean(root, all),
        Cmd::Bench { runs } => {
            let out = pipeline::bench(root, runs)?;
            if json {
                show::json(&out)
            } else {
                show::bench(&out);
                show::next("fast-anything report");
                Ok(())
            }
        }
        Cmd::Import { what } => {
            let out = android::import(root, &(*what).into_import())?;
            if json {
                show::json(&out)
            } else {
                show::import(&out);
                Ok(())
            }
        }
        Cmd::Report { include_android } => {
            let out = pipeline::report(root, include_android)?;
            if json {
                show::json(&out)
            } else {
                show::report(&out);
                Ok(())
            }
        }
        Cmd::Status => {
            let ctx = state::Ctx::open(root)?;
            if json {
                show::json(&show::status_json(&ctx))
            } else {
                show::status(&ctx);
                Ok(())
            }
        }
        Cmd::Doctor => {
            if json {
                let report = doctor::check();
                show::json(&report)?;
                doctor::ensure_ready(&report)
            } else {
                doctor::run()
            }
        }
        Cmd::Mcp | Cmd::Replay { .. } => unreachable!(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disable_all_parses() {
        assert!(matches!(
            Cli::try_parse_from(["fast-anything", "disable", "--all"]).unwrap().cmd,
            Cmd::Disable { all: true, id: None }
        ));
        assert!(Cli::try_parse_from(["fast-anything", "disable", "fuzzy.lev", "--all"]).is_err());
    }

    #[test]
    fn clean_parses() {
        assert!(matches!(Cli::try_parse_from(["fast-anything", "clean"]).unwrap().cmd, Cmd::Clean { all: false }));
        assert!(matches!(
            Cli::try_parse_from(["fast-anything", "clean", "--all"]).unwrap().cmd,
            Cmd::Clean { all: true }
        ));
    }

    #[test]
    fn verify_all_parses() {
        let cli = Cli::try_parse_from(["fast-anything", "verify", "--all"]).unwrap();
        assert!(matches!(cli.cmd, Cmd::Verify { all: true, id: None }));
        assert!(Cli::try_parse_from(["fast-anything", "verify", "fuzzy.lev", "--all"]).is_err(), "an id or --all");
        assert!(matches!(
            Cli::try_parse_from(["fast-anything", "verify"]).unwrap().cmd,
            Cmd::Verify { all: false, .. }
        ));
    }

    #[test]
    fn json_flag_parses_before_and_after_subcommand() {
        for argv in [
            vec!["fast-anything", "--json", "doctor"],
            vec!["fast-anything", "doctor", "--json"],
            vec!["fast-anything", "status", "--json"],
            vec!["fast-anything", "bench", "--json", "--runs", "3"],
            vec!["fast-anything", "verify", "--all", "--json"],
            vec!["fast-anything", "report", "--json"],
            vec!["fast-anything", "port", "--json", "fuzzy.lev"],
            vec!["fast-anything", "enable", "--json", "fuzzy.lev"],
            vec!["fast-anything", "disable", "--all", "--json"],
        ] {
            let cli = Cli::try_parse_from(argv).unwrap();
            assert!(cli.json);
        }

        let scan_before = Cli::try_parse_from(["fast-anything", "--json", "scan", "--", "python", "app.py"]).unwrap();
        assert!(scan_before.json);
        let scan_after = Cli::try_parse_from(["fast-anything", "scan", "--json", "--", "python", "app.py"]).unwrap();
        assert!(scan_after.json);

        // --json after -- belongs to the user's app command, not fast-anything.
        let app_flag = Cli::try_parse_from(["fast-anything", "scan", "--", "python", "app.py", "--json"]).unwrap();
        assert!(!app_flag.json);
        assert!(matches!(app_flag.cmd, Cmd::Scan { command, .. } if command == ["python", "app.py", "--json"]));
    }

    #[test]
    fn import_lists_its_formats() {
        let err = Cli::try_parse_from(["fast-anything", "import", "--help"]).err().unwrap();
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayHelp, "exits 0");
        let help = err.to_string();
        for format in ["gfxinfo", "perfetto", "simpleperf", "json"] {
            assert!(help.contains(format), "{help}");
        }
        let cli = Cli::try_parse_from([
            "fast-anything",
            "import",
            "perfetto",
            "--input",
            "t.pftrace",
            "--mapping",
            "mapping.txt",
            "--thermal-status",
            "6",
            "--json",
        ])
        .unwrap();
        assert!(cli.json);
        let Cmd::Import { what } = cli.cmd else { panic!("import") };
        let imp = (*what).into_import();
        assert_eq!((imp.format, imp.meta.thermal_status), (android::Format::Perfetto, Some(6)));
        assert_eq!(imp.symbols, Some(PathBuf::from("mapping.txt")));
        assert!(
            Cli::try_parse_from(["fast-anything", "import", "gfxinfo", "--input", "g.txt", "--thermal-status", "7"])
                .is_err()
        );
        assert!(Cli::try_parse_from(["fast-anything", "import", "gfxinfo"]).is_err(), "--input is required");
        assert!(matches!(
            Cli::try_parse_from(["fast-anything", "report", "--include-android"]).unwrap().cmd,
            Cmd::Report { include_android: true }
        ));
    }

    #[test]
    fn android_scan_needs_no_command() {
        let cli = Cli::try_parse_from(["fast-anything", "scan", "--android", "com.example.app", "--device", "DM9X8"])
            .unwrap();
        let Cmd::Scan { android, device, command, .. } = cli.cmd else { panic!("scan") };
        assert_eq!((android.as_deref(), device.as_deref(), command.len()), (Some("com.example.app"), Some("DM9X8"), 0));
        assert!(Cli::try_parse_from(["fast-anything", "scan"]).is_err(), "a host scan still needs a command");
        assert!(Cli::try_parse_from(["fast-anything", "scan", "--device", "x", "--", "python", "a.py"]).is_err());
        let explain = Cli::try_parse_from(["fast-anything", "scan", "--explain", "--", "python", "a.py"]).unwrap();
        assert!(matches!(explain.cmd, Cmd::Scan { explain: true, .. }));
    }

    #[test]
    fn json_rejects_unsupported_subcommands() {
        let root = Path::new(".");
        for cmd in [
            Cmd::Build,
            Cmd::Clean { all: false },
            Cmd::Mcp,
            Cmd::Go { agent: None, top: 1, attempts: 3, runs: 5, samples: 500, command: vec!["python".into()] },
        ] {
            let err = run(cmd, root, true).unwrap_err();
            assert!(err.to_string().contains("--json isn't supported"), "{err}");
        }
    }
}
