//! A report worth sharing: markdown for the PR, an SVG card for the README / socials.

use crate::android::{self, Evidence};
use crate::bench::{BenchResult, Scope, Verdict};
use crate::hotspot::Hotspot;
use crate::state::{Ctx, Port, State};
use crate::util::{nanos, secs, thousands, times};
use anyhow::{Result, bail, ensure};
use serde::Serialize;
use std::fs;
use std::path::PathBuf;

#[derive(Serialize)]
pub struct ReportOut {
    pub markdown: PathBuf,
    /// None when there's no bench, only on-device evidence.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub svg: Option<PathBuf>,
    /// The imported on-device reports the markdown includes.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub android: Vec<PathBuf>,
    pub text: String,
}

fn xml(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn port_line(p: &Port) -> String {
    let v = p.verify.clone().unwrap_or_default();
    match v.micro {
        Some(m) => format!("{} → Rust · {} per call · {}/{} calls verified", p.id, times(m.speedup), v.passed, v.total),
        None => format!("{} → Rust · {}/{} calls verified", p.id, v.passed, v.total),
    }
}

/// "8.0×" and "faster", or "1.1×" and "slower" when the app got slower (#142).
fn headline(speedup: f64) -> (String, &'static str) {
    if speedup >= 1.0 { (times(speedup), "faster") } else { (times(1.0 / speedup.max(1e-12)), "slower") }
}

pub fn svg(project: &str, b: &BenchResult, ports: &[&Port]) -> String {
    let (headline, word) = headline(b.speedup);
    let after_w = (300.0 * b.after.median / b.before.median.max(1e-12)).clamp(4.0, 300.0);
    let mut detail = ports.iter().take(2).map(|p| port_line(p)).collect::<Vec<_>>().join("   ");
    // the card's detail line holds about 95 characters
    if let Some(cpu) = b.cpu_note()
        && detail.chars().count() + cpu.chars().count() + 3 <= 95
    {
        detail = if detail.is_empty() { cpu } else { format!("{detail}   {cpu}") };
    }
    let sans = "system-ui,-apple-system,Segoe UI,Helvetica,Arial,sans-serif";
    let mono = "ui-monospace,SFMono-Regular,Menlo,Consolas,monospace";
    format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="760" height="220" viewBox="0 0 760 220" role="img" aria-label="{project}: {headline} {word}">
  <rect width="760" height="220" rx="16" fill="#0d1117"/>
  <rect x="0.5" y="0.5" width="759" height="219" rx="15.5" fill="none" stroke="#30363d"/>
  <text x="32" y="44" font-family="{mono}" font-size="14" fill="#f74c00">⚡ fast-anything</text>
  <text x="32" y="78" font-family="{sans}" font-size="20" fill="#e6edf3">{project}</text>
  <text x="32" y="148" font-family="{sans}" font-size="64" font-weight="800" fill="#f74c00">{headline}<tspan dx="12" font-size="28" font-weight="600" fill="#e6edf3">{word}</tspan></text>
  <text x="420" y="92" font-family="{sans}" font-size="13" fill="#8b949e">{original}</text>
  <text x="720" y="92" font-family="{mono}" font-size="13" fill="#e6edf3" text-anchor="end">{before}</text>
  <rect x="420" y="100" width="300" height="16" rx="4" fill="#30363d"/>
  <text x="420" y="146" font-family="{sans}" font-size="13" fill="#8b949e">Rust</text>
  <text x="720" y="146" font-family="{mono}" font-size="13" fill="#e6edf3" text-anchor="end">{after}</text>
  <rect x="420" y="154" width="{after_w:.1}" height="16" rx="4" fill="#f74c00"/>
  <text x="32" y="196" font-family="{sans}" font-size="13" fill="#8b949e">{detail}</text>
</svg>
"##,
        project = xml(project),
        original = b.lang.label(),
        before = secs(b.before.median),
        after = secs(b.after.median),
        detail = xml(&detail),
    )
}

/// "Measured 2026-11-26 on AMD Ryzen 5 PRO 4650GE (12 logical CPUs), Linux x86_64.", leaving out what
/// isn't known; None when nothing is.
fn scope_line(s: &Scope) -> Option<String> {
    let cpus = s.cpus.map(|n| format!("{n} logical CPU{}", if n == 1 { "" } else { "s" }));
    let machine = match (&s.cpu, cpus) {
        (Some(model), Some(cpus)) => Some(format!("{model} ({cpus})")),
        (model, cpus) => model.clone().or(cpus),
    };
    let os = match s.os.as_str() {
        "linux" => "Linux",
        "macos" => "macOS",
        "windows" => "Windows",
        "freebsd" => "FreeBSD",
        os => os,
    };
    let platform = format!("{os} {}", s.arch).trim().to_string();
    let on: Vec<String> = machine.into_iter().chain(Some(platform).filter(|p| !p.is_empty())).collect();
    if s.date.is_none() && on.is_empty() {
        return None;
    }
    let date = s.date.as_ref().map(|d| format!(" {d}")).unwrap_or_default();
    let on = if on.is_empty() { String::new() } else { format!(" on {}", on.join(", ")) };
    Some(format!("Measured{date}{on}."))
}

/// How many of the last scan's functions the report lists.
const TOP: usize = 8;

/// What became of a function from the scan, in the words `status` and `scan` use.
fn outcome(h: &Hotspot, s: &State) -> String {
    match s.ports.get(&h.id) {
        Some(p) => match &p.verify {
            _ if p.enabled => "ported and enabled".into(),
            Some(v) if v.ok() => "ported and verified, not enabled".into(),
            Some(v) if !v.built => "ported, doesn't compile".into(),
            Some(_) => "ported, doesn't match the original".into(),
            None => "ported, waiting for the Rust".into(),
        },
        None if h.worth_it() => "portable, not ported".into(),
        None => h.why_not().unwrap_or_default(),
    }
}

/// How the ports were chosen: the last scan's top functions and how much the profiler slowed the app.
/// Empty without scan data.
fn scan_section(s: &State, b: &BenchResult) -> String {
    if s.hotspots.is_empty() {
        return String::new();
    }
    let mut md = format!(
        "\n## How the ports were chosen\n\nThe last scan profiled `{}`, which took **{}** under the profiler",
        s.command.join(" "),
        secs(s.wall)
    );
    let (wall, original) = (s.wall, s.unprofiled.filter(|&u| u > 0.0).unwrap_or(b.before.median));
    let baseline =
        if s.unprofiled.is_some_and(|u| u > 0.0) { "timed by scan --calibrate" } else { "the original's median above" };
    if wall > 0.0 && original > 0.0 {
        md.push_str(&if wall >= original {
            format!(
                " against **{}** without it ({baseline}): profiling made it run {} as long",
                secs(original),
                times(wall / original)
            )
        } else {
            format!(", less than the **{}** without it ({baseline}): the scan was likely stopped early", secs(original))
        });
    }
    md.push_str(
        ". Shares are of that profiled run (`total` includes callees; `self` is the function's own body). They are not direct ratios: the profiler slows some code more than the rest, so a share isn't the part of the original's time a port can save.\n\n| # | function | total | self | calls | outcome |\n|---|---|---|---|---|---|\n",
    );
    for (i, h) in s.hotspots.iter().take(TOP).enumerate() {
        let calls = if h.ncalls == 0 { "-".to_string() } else { thousands(h.ncalls) };
        let outcome = outcome(h, s).replace('|', "\\|").replace('\n', " ");
        md.push_str(&format!(
            "| {} | `{}` | {:.1}% | {:.1}% | {calls} | {outcome} |\n",
            i + 1,
            h.id,
            h.share * 100.0,
            h.effective_self_share(s.wall) * 100.0
        ));
    }
    if s.hotspots.len() > TOP {
        md.push_str(&format!("\n{} more functions from the scan ranked lower.\n", s.hotspots.len() - TOP));
    }
    md
}

fn enabled(s: &State) -> Vec<&Port> {
    s.ports.values().filter(|p| p.enabled).collect()
}

const MADE_WITH: &str = "\n<sub>Made with [fast-anything](https://github.com/EL4CTEO/fast-anything)</sub>\n";

/// The report for the last bench `b`; `s` holds the ports and the scan that chose them. On-device
/// evidence gets its own section, and the host part is labelled as such.
pub fn markdown(project: &str, b: &BenchResult, s: &State, on_device: &[Evidence]) -> String {
    // results saved before rounds and the A/A control have no verdict and no range
    let how = if b.verdict.is_some() {
        format!(
            "median of {} rounds, each running the original twice and Rust once in rotated order; middle half of the rounds {}–{}",
            b.runs,
            times(b.speedup_q1).trim_end_matches('×'),
            times(b.speedup_q3)
        )
    } else {
        format!("median of {} runs each, original and Rust interleaved", b.runs)
    };
    let (head, word) = headline(b.speedup);
    let scope = b.scope.as_ref().and_then(scope_line).map(|l| format!("{l}\n\n")).unwrap_or_default();
    let host = if on_device.is_empty() { "" } else { "## Host scan and bench\n\n" };
    let mut md = format!(
        "# ⚡ {project}: {head} {word}\n\n{host}{scope}`{}` went from **{}** to **{}** ({how}).\n\n",
        b.command.join(" "),
        secs(b.before.median),
        secs(b.after.median),
    );
    if let (Some(before), Some(after), Some(cpu)) = (&b.cpu_before, &b.cpu_after, b.cpu_note()) {
        md.push_str(&format!(
            "- {cpu}: **{}** → **{}** (user + system, the app and the processes it waits for)\n",
            secs(before.median),
            secs(after.median)
        ));
    }
    // results from before the A/A control have no verdict
    if let Some(v) = b.verdict {
        let verdict = match v {
            Verdict::Faster => "the Rust ports' change is clearly beyond that",
            Verdict::Slower => "the Rust ports make it slower, beyond that",
            Verdict::Noise => "the Rust ports' change is within that noise",
        };
        md.push_str(&format!(
            "- A/A control, the original against itself in the same rounds: {:+.1}% (middle half {:+.1}% to {:+.1}%); {verdict}\n",
            b.aa.median, b.aa.q1, b.aa.q3
        ));
    }
    let on = enabled(s);
    md.push_str("\n![fast-anything report](report.svg)\n");
    if !on.is_empty() {
        md.push_str("\n| function | per call | checked against |\n|---|---|---|\n");
    }
    for p in &on {
        let v = p.verify.clone().unwrap_or_default();
        let per_call = v
            .micro
            .map(|m| format!("**{}** ({} → {})", times(m.speedup), nanos(m.original_ns), nanos(m.rust_ns)))
            .unwrap_or_else(|| "n/a".into());
        md.push_str(&format!("| `{}` | {per_call} | {}/{} recorded real calls |\n", p.id, v.passed, v.total));
    }
    md.push_str(&scan_section(s, b));
    if on.is_empty() {
        // ports switched off after the bench: there's no switch to describe (#142)
        let with =
            if b.ports.is_empty() { String::new() } else { format!(", with {} switched on", b.ports.join(", ")) };
        md.push_str(&format!("\nNo port is switched on now, so the app runs its original code: the numbers above are from the last bench{with}. `fast-anything enable <id>` switches a port back on.\n"));
    } else {
        md.push_str("\nThe original code stays in place: set `FAST_ANYTHING_DISABLE=1` to run it, or `fast-anything disable <id>` to remove the switch.\n");
    }
    if !on_device.is_empty() {
        md.push_str(&android::markdown(on_device, true));
    }
    md.push_str(MADE_WITH);
    md
}

/// The report when there's on-device evidence but no host bench.
fn on_device_only(project: &str, on_device: &[Evidence]) -> String {
    format!("# ⚡ {project}: on-device evidence\n{}{MADE_WITH}", android::markdown(on_device, false))
}

/// `include_android`: fail when there's no on-device evidence (it's included whenever there is).
pub fn write(ctx: &Ctx, include_android: bool) -> Result<ReportOut> {
    let on_device = android::load(&ctx.fast)?;
    ensure!(
        !include_android || !on_device.is_empty(),
        "no on-device evidence in .fast/android yet: run  fast-anything import gfxinfo --input gfxinfo.txt ...  first"
    );
    let project = ctx.root.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "your app".into());
    let md_path = ctx.fast.join("report.md");
    let android: Vec<PathBuf> = on_device.iter().map(|e| e.path.clone()).collect();
    let Some(b) = ctx.state.bench.as_ref() else {
        if on_device.is_empty() {
            bail!("no benchmark yet: run  fast-anything bench");
        }
        let text = on_device_only(&project, &on_device);
        fs::write(&md_path, &text)?;
        return Ok(ReportOut { markdown: md_path, svg: None, android, text });
    };
    let ports = enabled(&ctx.state);
    let text = markdown(&project, b, &ctx.state, &on_device);
    let svg_path = ctx.fast.join("report.svg");
    fs::write(&md_path, &text)?;
    fs::write(&svg_path, svg(&project, b, &ports))?;
    Ok(ReportOut { markdown: md_path, svg: Some(svg_path), android, text })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bench::{Spread, Stats};
    use crate::state::Verify;

    #[test]
    fn on_device_evidence_gets_its_own_section() {
        let doc = crate::android::validate(serde_json::json!({
            "schema_version": 1, "source": "android-gfxinfo", "measured_on": "ART",
            "device": { "serial": "DM9X8", "android_version": "14", "abi": "arm64-v8a" },
            "build": { "package": "com.example.app", "variant": "debug" },
            "thermal": { "status": 0 },
            "samples": { "frames": 412, "janky_frames": 91, "janky_percent": 22.09, "p90_ms": 22 },
            "provenance": { "input": "gfxinfo.txt", "input_sha256": "ab".repeat(32) }
        }))
        .unwrap();
        let evidence = [Evidence { path: ".fast/android/gfxinfo/report.json".into(), doc }];
        let b = BenchResult {
            before: Stats { median: 4.0, ..Stats::default() },
            after: Stats { median: 0.5, ..Stats::default() },
            speedup: 8.0,
            ..BenchResult::default()
        };
        let md = markdown("app", &b, &State::default(), &evidence);
        assert!(md.starts_with("# ⚡ app: 8.0× faster\n\n## Host scan and bench\n\n"), "{md}");
        let device = &md[md.find("## On-device (ART)").expect("an on-device section")..];
        assert!(device.contains("no speed-up is computed across the two"), "{device}");
        assert!(!device.contains('×') && !device.contains("faster"), "no speed-up across host and ART: {device}");
        for part in [
            "measured on ART: DM9X8, Android 14, arm64-v8a",
            "thermal status at capture: 0 (none)",
            "**22.09%** janky (91)",
            "p90 22ms",
            "sha256 `abab",
        ] {
            assert!(device.contains(part), "{part}\n{device}");
        }
        assert!(md.ends_with(MADE_WITH));
        assert!(!markdown("app", &b, &State::default(), &[]).contains("On-device"));
        let alone = on_device_only("app", &evidence);
        assert!(
            alone.starts_with("# ⚡ app: on-device evidence\n\n## On-device (ART)") && !alone.contains("Host"),
            "{alone}"
        );
    }

    #[test]
    fn a_report_without_enabled_ports_says_so() {
        let b = BenchResult { speedup: 8.0, ports: vec!["fuzzy.lev".into()], ..BenchResult::default() };
        let md = markdown("app", &b, &State::default(), &[]);
        assert!(
            md.contains("No port is switched on now, so the app runs its original code: the numbers above are from the last bench, with fuzzy.lev switched on."),
            "{md}"
        );
        assert!(!md.contains("FAST_ANYTHING_DISABLE") && !md.contains("| function | per call"), "{md}");
    }

    #[test]
    fn svg_is_escaped_and_scaled() {
        let b = BenchResult {
            before: Stats { median: 4.0, ..Stats::default() },
            after: Stats { median: 0.5, ..Stats::default() },
            speedup: 8.0,
            ..BenchResult::default()
        };
        let card = svg("a<b>&c", &b, &[]);
        assert!(card.contains("a&lt;b&gt;&amp;c"));
        assert!(card.contains("8.0×"));
        assert!(card.contains(r#"width="37.5""#));
    }

    #[test]
    fn a_slower_app_is_called_slower() {
        let b = BenchResult {
            before: Stats { median: 8.42, ..Stats::default() },
            after: Stats { median: 9.27, ..Stats::default() },
            speedup: 8.42 / 9.27,
            ..BenchResult::default()
        };
        let (md, card) = (markdown("app", &b, &State::default(), &[]), svg("app", &b, &[]));
        assert!(md.starts_with("# ⚡ app: 1.1× slower"), "{md}");
        assert!(card.contains(">slower</tspan>") && !card.contains("faster"), "{card}");
    }

    #[test]
    fn report_shows_cpu_time_and_the_aa_control() {
        let b = BenchResult {
            command: vec!["python".into(), "app.py".into()],
            runs: 5,
            before: Stats { median: 12.0, ..Stats::default() },
            after: Stats { median: 0.05, ..Stats::default() },
            speedup: 234.0,
            speedup_q1: 228.0,
            speedup_q3: 240.0,
            aa: Spread { median: 0.7, q1: -0.3, q3: 0.9 },
            verdict: Some(Verdict::Faster),
            cpu_before: Some(Stats { median: 11.9, ..Stats::default() }),
            cpu_after: Some(Stats { median: 0.125, ..Stats::default() }),
            cpu_speedup: Some(95.2),
            ..BenchResult::default()
        };
        let md = markdown("demo", &b, &State::default(), &[]);
        assert!(md.contains("median of 5 rounds") && md.contains("228–240×"), "{md}");
        assert!(md.contains("- CPU time 95×: **11.90s** → **125ms**"), "{md}");
        assert!(
            md.contains("+0.7% (middle half -0.3% to +0.9%); the Rust ports' change is clearly beyond that"),
            "{md}"
        );
        let card = svg("demo", &b, &[]);
        assert!(card.contains("234×") && card.contains("CPU time 95×"));

        let old = BenchResult { speedup: 8.0, ..BenchResult::default() };
        let md = markdown("demo", &old, &State::default(), &[]);
        assert!(!md.contains("CPU time") && !md.contains("A/A") && md.contains("interleaved"), "{md}");
    }

    #[test]
    fn report_says_when_and_where_it_was_measured() {
        let scope = Scope {
            date: Some("2026-11-26".into()),
            os: "linux".into(),
            arch: "x86_64".into(),
            cpus: Some(12),
            cpu: Some("AMD Ryzen 5 PRO 4650GE".into()),
        };
        let b = BenchResult { speedup: 8.0, scope: Some(scope.clone()), ..BenchResult::default() };
        let md = markdown("app", &b, &State::default(), &[]);
        assert!(
            md.starts_with(
                "# ⚡ app: 8.0× faster\n\nMeasured 2026-11-26 on AMD Ryzen 5 PRO 4650GE (12 logical CPUs), Linux x86_64.\n\n`"
            ),
            "{md}"
        );
        let unknown_cpu = Scope { os: "windows".into(), cpu: None, cpus: Some(1), ..scope };
        assert_eq!(scope_line(&unknown_cpu).as_deref(), Some("Measured 2026-11-26 on 1 logical CPU, Windows x86_64."));
        assert_eq!(scope_line(&Scope::default()), None);

        // a result saved before bench recorded its scope
        let old: BenchResult =
            serde_json::from_str(r#"{"runs": 5, "before": {"median": 4.0}, "speedup": 8.0}"#).unwrap();
        assert!(old.scope.is_none());
        let md = markdown("app", &old, &State::default(), &[]);
        assert!(md.starts_with("# ⚡ app: 8.0× faster\n\n`") && !md.contains("Measured"), "{md}");
    }

    #[test]
    fn report_shows_how_the_ports_were_chosen() {
        let hotspot = |id: &str, share: f64, self_share: f64, ncalls: u64| Hotspot {
            id: id.into(),
            share,
            self_share,
            ncalls,
            portability: 0.9,
            ..Hotspot::default()
        };
        let mut hotspots = vec![
            Hotspot { blockers: vec!["does I/O: print".into()], ..hotspot("app.main", 0.99, 0.02, 1) },
            hotspot("fuzzy.lev", 0.81, 0.51, 1_250_000),
            Hotspot { covered_by: Some("fuzzy.lev".into()), ..hotspot("fuzzy.cost", 0.3, 0.3, 9_000_000) },
            hotspot("fuzzy.norm", 0.05, 0.05, 2_000),
            hotspot("fuzzy.tiny", 0.004, 0.004, 10),
        ];
        hotspots.extend((0..5).map(|i| hotspot(&format!("util.f{i}"), 0.001, 0.001, 0)));
        let verify = Verify {
            built: true,
            total: 500,
            passed: 500,
            micro: serde_json::from_str(r#"{"original_ns": 9000.0, "rust_ns": 300.0, "speedup": 30.0}"#).unwrap(),
            ..Verify::default()
        };
        let port = Port {
            id: "fuzzy.lev".into(),
            file: "fuzzy.py".into(),
            module: "fuzzy".into(),
            func: "lev".into(),
            ext: "fa_fuzzy_lev".into(),
            crate_dir: ".fast/ports/fuzzy.lev".into(),
            recorded: 500,
            calls_seen: 1_250_000,
            verify: Some(verify),
            enabled: true,
            lang: Default::default(),
            owner: None,
            line: 1,
            sources: vec![],
        };
        let s = State {
            command: vec!["python".into(), "app.py".into()],
            wall: 12.4,
            hotspots,
            ports: [("fuzzy.lev".to_string(), port)].into(),
            ..State::default()
        };
        let b = BenchResult {
            command: s.command.clone(),
            before: Stats { median: 4.0, ..Stats::default() },
            after: Stats { median: 0.5, ..Stats::default() },
            speedup: 8.0,
            ..BenchResult::default()
        };
        let md = markdown("app", &b, &s, &[]);
        let (table, section, stays) = (
            md.find("| `fuzzy.lev` | **30×**").unwrap(),
            md.find("## How the ports were chosen").unwrap(),
            md.find("The original code stays in place").unwrap(),
        );
        assert!(table < section && section < stays, "after the ports' table: {md}");
        assert!(
            md.contains("`python app.py`, which took **12.40s** under the profiler against **4.00s** without it (the original's median above): profiling made it run 3.1× as long."),
            "{md}"
        );
        assert!(md.contains("They are not direct ratios"), "{md}");
        for row in [
            "| 1 | `app.main` | 99.0% | 2.0% | 1 | does I/O: print |",
            "| 2 | `fuzzy.lev` | 81.0% | 51.0% | 1,250,000 | ported and enabled |",
            "| 3 | `fuzzy.cost` | 30.0% | 30.0% | 9,000,000 | ported inside fuzzy.lev |",
            "| 4 | `fuzzy.norm` | 5.0% | 5.0% | 2,000 | portable, not ported |",
            "| 5 | `fuzzy.tiny` | 0.4% | 0.4% | 10 | too small to matter (0.4% of the run, 2% needed) |",
            "| 6 | `util.f0` | 0.1% | 0.1% | - |",
        ] {
            assert!(md.contains(row), "{row}\n{md}");
        }
        assert!(md.contains("| 8 | `util.f2`") && !md.contains("`util.f3`"), "{md}");
        assert!(md.contains("2 more functions from the scan ranked lower."), "{md}");

        let stopped_early = State { wall: 2.0, ..s.clone() };
        let md = markdown("app", &b, &stopped_early, &[]);
        assert!(
            md.contains("less than the **4.00s** without it (the original's median above)") && !md.contains("as long"),
            "{md}"
        );

        let calibrated = State { unprofiled: Some(3.1), ..s.clone() };
        let md = markdown("app", &b, &calibrated, &[]);
        assert!(
            md.contains("`python app.py`, which took **12.40s** under the profiler against **3.10s** without it (timed by scan --calibrate): profiling made it run 4.0× as long."),
            "{md}"
        );

        let no_scan = State { hotspots: vec![], ..s };
        let md = markdown("app", &b, &no_scan, &[]);
        assert!(!md.contains("How the ports were chosen") && md.contains("| `fuzzy.lev` |"), "{md}");
    }
}
