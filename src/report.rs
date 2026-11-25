//! A report worth sharing: markdown for the PR, an SVG card for the README / socials.

use crate::bench::{BenchResult, Verdict};
use crate::state::{Ctx, Port};
use crate::util::{nanos, secs, times};
use anyhow::{Context, Result};
use serde::Serialize;
use std::fs;
use std::path::PathBuf;

#[derive(Serialize)]
pub struct ReportOut {
    pub markdown: PathBuf,
    pub svg: PathBuf,
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

pub fn markdown(project: &str, b: &BenchResult, ports: &[&Port]) -> String {
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
    let mut md = format!(
        "# ⚡ {project}: {head} {word}\n\n`{}` went from **{}** to **{}** ({how}).\n\n",
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
    md.push_str("\n![fast-anything report](report.svg)\n\n| function | per call | checked against |\n|---|---|---|\n");
    for p in ports {
        let v = p.verify.clone().unwrap_or_default();
        let per_call = v
            .micro
            .map(|m| format!("**{}** ({} → {})", times(m.speedup), nanos(m.original_ns), nanos(m.rust_ns)))
            .unwrap_or_else(|| "n/a".into());
        md.push_str(&format!("| `{}` | {per_call} | {}/{} recorded real calls |\n", p.id, v.passed, v.total));
    }
    md.push_str("\nThe original code stays in place: set `FAST_ANYTHING_DISABLE=1` to run it, or `fast-anything disable <id>` to remove the switch.\n\n<sub>Made with [fast-anything](https://github.com/EL4CTEO/fast-anything)</sub>\n");
    md
}

pub fn write(ctx: &Ctx) -> Result<ReportOut> {
    let b = ctx.state.bench.as_ref().context("no benchmark yet: run  fast-anything bench")?;
    let project = ctx.root.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "your app".into());
    let ports: Vec<&Port> = ctx.state.ports.values().filter(|p| p.enabled).collect();
    let text = markdown(&project, b, &ports);
    let (md_path, svg_path) = (ctx.fast.join("report.md"), ctx.fast.join("report.svg"));
    fs::write(&md_path, &text)?;
    fs::write(&svg_path, svg(&project, b, &ports))?;
    Ok(ReportOut { markdown: md_path, svg: svg_path, text })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bench::{Spread, Stats};

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
        let (md, card) = (markdown("app", &b, &[]), svg("app", &b, &[]));
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
        let md = markdown("demo", &b, &[]);
        assert!(md.contains("median of 5 rounds") && md.contains("228–240×"), "{md}");
        assert!(md.contains("- CPU time 95×: **11.90s** → **125ms**"), "{md}");
        assert!(
            md.contains("+0.7% (middle half -0.3% to +0.9%); the Rust ports' change is clearly beyond that"),
            "{md}"
        );
        let card = svg("demo", &b, &[]);
        assert!(card.contains("234×") && card.contains("CPU time 95×"));

        let old = BenchResult { speedup: 8.0, ..BenchResult::default() };
        let md = markdown("demo", &old, &[]);
        assert!(!md.contains("CPU time") && !md.contains("A/A") && md.contains("interleaved"), "{md}");
    }
}
