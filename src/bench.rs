//! Times the whole app: original Python vs Rust ports, interleaved so drift hits both equally.

use crate::backend::Lang;
use crate::python::DISABLE_ENV;
use crate::state::Ctx;
use crate::{ui, util};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::process::{Command, Stdio};
use std::time::Instant;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Stats {
    pub median: f64,
    pub mean: f64,
    pub min: f64,
    pub max: f64,
    pub stddev: f64,
}

impl Stats {
    pub fn of(xs: &[f64]) -> Stats {
        if xs.is_empty() {
            return Stats::default();
        }
        let mut v = xs.to_vec();
        v.sort_by(f64::total_cmp);
        let n = v.len() as f64;
        let mean = v.iter().sum::<f64>() / n;
        let median = if v.len() % 2 == 1 { v[v.len() / 2] } else { (v[v.len() / 2 - 1] + v[v.len() / 2]) / 2.0 };
        let var = v.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / n;
        Stats { median, mean, min: v[0], max: v[v.len() - 1], stddev: var.sqrt() }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct BenchResult {
    pub lang: Lang,
    pub command: Vec<String>,
    pub runs: usize,
    pub ports: Vec<String>,
    pub before: Stats,
    pub after: Stats,
    pub speedup: f64,
}

fn time_once(ctx: &Ctx, python: &str, args: &[String], original: bool) -> Result<f64> {
    let mut cmd = Command::new(python);
    cmd.args(args).current_dir(&ctx.root).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
    if original {
        cmd.env(DISABLE_ENV, "1");
    } else {
        cmd.env_remove(DISABLE_ENV);
    }
    let start = Instant::now();
    let out = cmd.output().context("running the app")?;
    let took = start.elapsed().as_secs_f64();
    ensure!(
        out.status.success(),
        "the app failed ({}) with the {} version:\n{}",
        out.status,
        if original { ctx.state.lang.label() } else { "Rust" },
        util::tail(&String::from_utf8_lossy(&out.stderr), 20)
    );
    Ok(took)
}

pub fn run(ctx: &Ctx, runs: usize) -> Result<BenchResult> {
    let (python, args) = ctx.state.lang.bench_command(ctx)?;
    let (python, args) = (python.as_str(), args.as_slice());
    let ports: Vec<String> = ctx.state.ports.values().filter(|p| p.enabled).map(|p| p.id.clone()).collect();
    if ports.is_empty() {
        ui::warn("no ports are enabled yet, so before and after are the same code");
    }
    ui::step(format!(
        "timing {} ({runs} runs each, {} vs Rust, interleaved)",
        ui::bold(ctx.state.command.join(" ")),
        ctx.state.lang.label()
    ));
    time_once(ctx, python, args, true)?;
    time_once(ctx, python, args, false)?;
    let (mut before, mut after) = (Vec::with_capacity(runs), Vec::with_capacity(runs));
    for i in 0..runs {
        if i % 2 == 0 {
            before.push(time_once(ctx, python, args, true)?);
            after.push(time_once(ctx, python, args, false)?);
        } else {
            after.push(time_once(ctx, python, args, false)?);
            before.push(time_once(ctx, python, args, true)?);
        }
    }
    let (before, after) = (Stats::of(&before), Stats::of(&after));
    let speedup = if after.median > 0.0 { before.median / after.median } else { 0.0 };
    Ok(BenchResult { lang: ctx.state.lang, command: ctx.state.command.clone(), runs, ports, before, after, speedup })
}

#[cfg(test)]
mod tests {
    use super::Stats;

    #[test]
    fn stats() {
        let s = Stats::of(&[3.0, 1.0, 2.0, 10.0]);
        assert_eq!(s.median, 2.5);
        assert_eq!(s.min, 1.0);
        assert_eq!(s.max, 10.0);
        assert_eq!(s.mean, 4.0);
        assert_eq!(Stats::of(&[5.0]).median, 5.0);
    }
}
