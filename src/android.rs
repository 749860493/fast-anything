//! Evidence measured on an Android device (dumpsys gfxinfo, Perfetto, simpleperf, or a JSON summary in
//! the same schema), kept apart from the host scan and bench: ART timings can't be compared with them (#143).

use crate::{sha256, util};
use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;
use serde_json::{Map, Value, json};
use std::fs;
use std::path::{Component, Path, PathBuf};

pub const SCHEMA_VERSION: u64 = 1;
/// Fewer frames than this make a jank percentage meaningless: one frame is 0% or 100%.
pub const MIN_FRAMES: u64 = 30;
/// Fewer CPU samples than this (at 1 kHz, a tenth of a second) rank noise, not methods.
pub const MIN_CPU_SAMPLES: u64 = 100;
const NOT_COMPARABLE: &str = "ART frame timing is not comparable to native Rust wall time";
const NOT_PARSED: &str = "fast-anything keeps the trace and its hash but doesn't parse or symbolize traces yet";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Gfxinfo,
    Json,
    Perfetto,
    Simpleperf,
}

impl Format {
    pub fn name(self) -> &'static str {
        match self {
            Format::Gfxinfo => "gfxinfo",
            Format::Json => "json",
            Format::Perfetto => "perfetto",
            Format::Simpleperf => "simpleperf",
        }
    }
}

/// What was measured, as given on the command line: never guessed from the capture.
#[derive(Debug, Clone, Default)]
pub struct Meta {
    pub package: Option<String>,
    pub device: Option<String>,
    pub model: Option<String>,
    pub android_version: Option<String>,
    pub api: Option<u32>,
    pub abi: Option<String>,
    pub build_variant: Option<String>,
    pub version_name: Option<String>,
    pub version_code: Option<u64>,
    pub thermal_status: Option<u8>,
    pub runs: Option<u32>,
    pub warmup: Option<u32>,
    pub captured_at: Option<String>,
    pub captured_by: Option<String>,
}

type Field = (&'static str, [&'static str; 2], Option<Value>);

impl Meta {
    /// Each flag, where it goes in the report, and its value.
    fn fields(&self) -> Vec<Field> {
        let s = |v: &Option<String>| v.as_ref().map(|v| json!(v));
        vec![
            ("--device", ["device", "serial"], s(&self.device)),
            ("--model", ["device", "model"], s(&self.model)),
            ("--android-version", ["device", "android_version"], s(&self.android_version)),
            ("--api", ["device", "api_level"], self.api.map(|v| json!(v))),
            ("--abi", ["device", "abi"], s(&self.abi)),
            ("--package", ["build", "package"], s(&self.package)),
            ("--build-variant", ["build", "variant"], s(&self.build_variant)),
            ("--version-name", ["build", "version_name"], s(&self.version_name)),
            ("--version-code", ["build", "version_code"], self.version_code.map(|v| json!(v))),
            ("--thermal-status", ["thermal", "status"], self.thermal_status.map(|v| json!(v))),
            ("--runs", ["samples", "runs"], self.runs.map(|v| json!(v))),
            ("--warmup", ["samples", "warmup_runs"], self.warmup.map(|v| json!(v))),
            ("--captured-by", ["provenance", "captured_by"], s(&self.captured_by)),
        ]
    }
}

/// The metadata a report can't do without, as (flag, place in the report).
const REQUIRED: [(&str, [&str; 2]); 5] = [
    ("--device", ["device", "serial"]),
    ("--package", ["build", "package"]),
    ("--android-version", ["device", "android_version"]),
    ("--abi", ["device", "abi"]),
    ("--build-variant", ["build", "variant"]),
];

pub struct Import {
    pub format: Format,
    pub input: PathBuf,
    pub meta: Meta,
    /// Default .fast/android/<format>.
    pub out: Option<PathBuf>,
    /// Perfetto: the R8/ProGuard mapping.txt; simpleperf: the symbol folder. Recorded, not used yet.
    pub symbols: Option<PathBuf>,
}

#[derive(Debug, Serialize)]
pub struct ImportOut {
    pub format: &'static str,
    pub report: PathBuf,
    pub raw: PathBuf,
    pub evidence: Value,
}

/// What a dumpsys gfxinfo text says about the frames of one process.
#[derive(Debug, Default, PartialEq)]
pub struct Gfxinfo {
    pub package: Option<String>,
    pub frames: u64,
    pub janky: Option<u64>,
    pub janky_percent: Option<f64>,
    pub p50: Option<f64>,
    pub p90: Option<f64>,
    pub p95: Option<f64>,
    pub p99: Option<f64>,
    /// The lowest and highest HISTOGRAM buckets that have frames.
    pub min: Option<f64>,
    pub max: Option<f64>,
}

fn ms(text: &str) -> Option<f64> {
    text.trim().strip_suffix("ms")?.trim().parse().ok()
}

/// Reads the text of  adb shell dumpsys gfxinfo <package> . Only its frame lines count: "Janky frames
/// (legacy)" and the "gpu percentile" lines are other measures.
pub fn parse_gfxinfo(text: &str) -> Result<Gfxinfo> {
    let mut g = Gfxinfo::default();
    let mut packages = Vec::new();
    let mut sections = 0;
    for line in text.lines().map(str::trim) {
        if let Some(rest) = line.strip_prefix("** Graphics info for pid ") {
            if let Some(pkg) = rest.split_once('[').and_then(|(_, r)| r.split_once(']')).map(|(p, _)| p.trim()) {
                packages.push(pkg.to_string());
            }
        } else if let Some(n) = line.strip_prefix("Total frames rendered:") {
            sections += 1;
            g.frames = n.trim().parse().with_context(|| format!("reading \"{line}\""))?;
        } else if let Some(rest) = line.strip_prefix("Janky frames:") {
            let (count, percent) = rest.split_once('(').unwrap_or((rest, ""));
            g.janky = count.trim().parse().ok();
            g.janky_percent = percent.split_once('%').and_then(|(p, _)| p.trim().parse().ok());
        } else if let Some(v) = line.strip_prefix("50th percentile:") {
            g.p50 = ms(v);
        } else if let Some(v) = line.strip_prefix("90th percentile:") {
            g.p90 = ms(v);
        } else if let Some(v) = line.strip_prefix("95th percentile:") {
            g.p95 = ms(v);
        } else if let Some(v) = line.strip_prefix("99th percentile:") {
            g.p99 = ms(v);
        } else if let Some(buckets) = line.strip_prefix("HISTOGRAM:") {
            let used: Vec<f64> = buckets
                .split_whitespace()
                .filter_map(|b| b.split_once('='))
                .filter(|(_, n)| n.parse::<u64>().is_ok_and(|n| n > 0))
                .filter_map(|(at, _)| ms(at))
                .collect();
            g.min = used.iter().copied().reduce(f64::min);
            g.max = used.iter().copied().reduce(f64::max);
        }
    }
    ensure!(
        sections > 0,
        "no \"Total frames rendered\" line: is this the output of  adb shell dumpsys gfxinfo <package> ?"
    );
    ensure!(
        sections == 1,
        "this has the frames of {sections} processes: capture one with  adb shell dumpsys gfxinfo <package>"
    );
    g.package = packages.into_iter().next();
    Ok(g)
}

/// The text of a capture: adb output redirected in Windows PowerShell 5 is UTF-16.
fn decode(bytes: &[u8]) -> String {
    if let Some(rest) = bytes.strip_prefix(&[0xff, 0xfe]) {
        let units: Vec<u16> = rest.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect();
        return String::from_utf16_lossy(&units);
    }
    let bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes);
    String::from_utf8_lossy(bytes).into_owned()
}

fn get<'a>(doc: &'a Value, path: &[&str]) -> Option<&'a Value> {
    path.iter().try_fold(doc, |v, key| v.get(key)).filter(|v| !v.is_null())
}

fn text(doc: &Value, path: &[&str]) -> Option<String> {
    match get(doc, path)? {
        Value::String(s) if !s.trim().is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn number(doc: &Value, path: &[&str]) -> Option<f64> {
    get(doc, path)?.as_f64()
}

fn object<'a>(doc: &'a mut Value, key: &str) -> Result<&'a mut Map<String, Value>> {
    let map = doc.as_object_mut().context("the report isn't a JSON object")?;
    map.entry(key).or_insert_with(|| json!({})).as_object_mut().with_context(|| format!("{key} isn't an object"))
}

fn text_of(v: &Value) -> String {
    v.as_str().map(String::from).unwrap_or_else(|| v.to_string())
}

/// Puts each given flag in its place, unless the report already says something else there.
pub(crate) fn apply(doc: &mut Value, meta: &Meta) -> Result<()> {
    for (flag, [section, key], value) in meta.fields() {
        let Some(value) = value else { continue };
        let map = object(doc, section)?;
        match map.get(key).filter(|v| !v.is_null()) {
            Some(had) if text_of(had) != text_of(&value) => {
                bail!("{flag} is {} but the report says {section}.{key} is {had}", text_of(&value))
            }
            _ => {
                map.insert(key.into(), value);
            }
        }
    }
    if let Some(at) = &meta.captured_at {
        doc.as_object_mut().context("the report isn't a JSON object")?.insert("captured_at".into(), json!(at));
    }
    Ok(())
}

/// Checks a report against schema version 1 and fills in what follows from it (comparability,
/// sample_status). Refuses a report that would let ART numbers pass for host ones.
pub fn validate(mut doc: Value) -> Result<Value> {
    ensure!(doc.is_object(), "the report isn't a JSON object");
    let version = doc.get("schema_version").and_then(Value::as_u64);
    ensure!(
        version == Some(SCHEMA_VERSION),
        "schema_version is {}: fast-anything reads version {SCHEMA_VERSION}",
        doc.get("schema_version").map_or("missing".into(), Value::to_string)
    );
    let measured_on = text(&doc, &["measured_on"]).context("measured_on is required (ART, host, ...)")?;
    ensure!(text(&doc, &["source"]).is_some(), "source is required, e.g. android-gfxinfo");
    let missing: Vec<String> = REQUIRED
        .iter()
        .filter(|(_, path)| text(&doc, path).is_none())
        .map(|(flag, [a, b])| format!("{flag} ({a}.{b})"))
        .collect();
    if !missing.is_empty() {
        bail!(
            "E_ANDROID_METADATA_REQUIRED: missing {}: fast-anything doesn't guess what was measured from the capture",
            missing.join(", ")
        );
    }
    for section in ["thermal", "samples", "provenance"] {
        ensure!(doc.get(section).is_some_and(Value::is_object), "{section} is required and must be an object");
    }
    if let Some(status) = get(&doc, &["thermal", "status"]) {
        ensure!(
            status.as_u64().is_some_and(|s| s <= 6),
            "thermal.status is {status}: Android's thermal status runs from 0 (none) to 6 (shutdown)"
        );
    }
    let sha = text(&doc, &["provenance", "input_sha256"]).unwrap_or_default();
    ensure!(
        sha.len() == 64 && sha.bytes().all(|b| b.is_ascii_hexdigit()),
        "provenance.input_sha256 is required: the SHA-256 of the raw capture, as 64 hex digits"
    );
    let frames = match get(&doc, &["samples", "frames"]) {
        Some(f) => Some(f.as_u64().with_context(|| format!("samples.frames is {f}: a count of frames"))?),
        None => None,
    };
    let art = measured_on == "ART";
    let warmup = number(&doc, &["samples", "warmup_runs"]).is_some_and(|w| w > 0.0);
    // thermal status 0 means the device wasn't throttling when it was captured
    let cool = number(&doc, &["thermal", "status"]) == Some(0.0);
    // an on-device CPU profile (scan --android) counts samples, not frames
    let cpu_samples = number(&doc, &["samples", "cpu_samples"]);
    let c = object(&mut doc, "comparability")?;
    if art {
        ensure!(
            c.get("host_scan") != Some(&json!(true)),
            "comparability.host_scan is true, but ART numbers can't be compared with a host scan"
        );
        c.insert("host_scan".into(), json!(false));
        c.entry("reason").or_insert_with(|| json!(NOT_COMPARABLE));
    }
    c.entry("warmup").or_insert(json!(warmup));
    c.entry("thermal_controlled").or_insert(json!(cool));
    if frames.is_none() && cpu_samples.is_some_and(|n| n >= MIN_CPU_SAMPLES as f64) {
        c.entry("sample_status").or_insert(json!("ok"));
    } else if frames.is_none() && cpu_samples.is_some() {
        c.insert("sample_status".into(), json!("insufficient"));
        c.entry("sample_reason").or_insert(json!(format!("fewer than {MIN_CPU_SAMPLES} CPU samples")));
    } else if frames.is_some_and(|f| f >= MIN_FRAMES) {
        c.entry("sample_status").or_insert(json!("ok"));
    } else {
        c.insert("sample_status".into(), json!("insufficient"));
        let why = match frames {
            Some(f) => format!("{f} frame{}, {MIN_FRAMES} needed for a jank percentage", if f == 1 { "" } else { "s" }),
            None => "no frame count".into(),
        };
        c.entry("sample_reason").or_insert(json!(why));
    }
    Ok(doc)
}

/// A new report for a capture, before the flags are applied.
pub(crate) fn skeleton(format: Format, samples: Map<String, Value>, input: &str, sha: &str) -> Value {
    json!({
        "schema_version": SCHEMA_VERSION,
        "source": format!("android-{}", format.name()),
        "measured_on": "ART",
        "imported_at": util::now_utc(),
        "imported_by": format!("fast-anything {}", env!("CARGO_PKG_VERSION")),
        "device": {},
        "build": {},
        "thermal": { "status": null },
        "samples": samples,
        "provenance": { "input": input, "input_sha256": sha },
    })
}

fn gfxinfo_samples(g: &Gfxinfo) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("frames".into(), json!(g.frames));
    let fields = [
        ("janky_frames", g.janky.map(|j| j as f64)),
        ("janky_percent", g.janky_percent),
        ("p50_ms", g.p50),
        ("p90_ms", g.p90),
        ("p95_ms", g.p95),
        ("p99_ms", g.p99),
        ("min_ms", g.min),
        ("max_ms", g.max),
    ];
    for (key, value) in fields {
        if let Some(v) = value {
            m.insert(key.into(), if v.fract() == 0.0 { json!(v as u64) } else { json!(v) });
        }
    }
    m
}

/// The checked report for an import, without writing anything.
pub fn build(imp: &Import, input: &Path, stored_as: &str) -> Result<Value> {
    let bytes = fs::read(input).with_context(|| format!("reading {}", input.display()))?;
    ensure!(!bytes.is_empty(), "{} is empty", input.display());
    let sha = sha256::hex(&bytes);
    let mut doc = match imp.format {
        Format::Gfxinfo => {
            let g = parse_gfxinfo(&decode(&bytes))?;
            if let (Some(found), Some(given)) = (&g.package, &imp.meta.package) {
                ensure!(found == given, "the capture is of {found}, not --package {given}");
            }
            skeleton(imp.format, gfxinfo_samples(&g), stored_as, &sha)
        }
        Format::Perfetto | Format::Simpleperf => {
            let mut doc = skeleton(imp.format, Map::new(), stored_as, &sha);
            doc["symbolization"] = json!("not attempted");
            doc["symbolization_reason"] = json!(NOT_PARSED);
            if let Some(path) = &imp.symbols {
                ensure!(path.exists(), "{} doesn't exist", path.display());
                let key = if imp.format == Format::Perfetto { "mapping" } else { "symfs" };
                let mut entry = json!({ "path": util::forward_slashes(&util::absolute(path)) });
                if path.is_file() {
                    entry["sha256"] = json!(sha256::file(path)?);
                }
                doc["provenance"][key] = entry;
            }
            doc
        }
        Format::Json => {
            let mut doc: Value =
                serde_json::from_str(&decode(&bytes)).with_context(|| format!("{} isn't JSON", input.display()))?;
            let p = object(&mut doc, "provenance")?;
            if !p.contains_key("input_sha256") {
                p.insert("input".into(), json!(stored_as));
                p.insert("input_sha256".into(), json!(sha));
            }
            doc
        }
    };
    apply(&mut doc, &imp.meta)?;
    validate(doc)
}

/// Where an import goes: .fast/android/<format>, or --out when that's inside .fast/android.
fn out_dir(fast: &Path, imp: &Import) -> Result<PathBuf> {
    let android = fast.join("android");
    let Some(out) = &imp.out else { return Ok(android.join(imp.format.name())) };
    let out = util::absolute(out);
    ensure!(
        out.starts_with(&android) && out != android && !out.components().any(|c| c == Component::ParentDir),
        "--out has to be a folder inside {}, where report looks for on-device evidence",
        android.display()
    );
    Ok(out)
}

/// Imports a capture: checks it and its metadata first, so a refused import writes nothing, then keeps
/// the raw file next to its report.json. Never touches state.json, the scan or the bench.
pub fn import(root: &Path, imp: &Import) -> Result<ImportOut> {
    let root = util::absolute(root);
    ensure!(root.is_dir(), "{} isn't a folder", root.display());
    let input = util::absolute(&imp.input);
    ensure!(input.is_file(), "{} isn't a file", imp.input.display());
    let out = out_dir(&root.join(".fast"), imp)?;
    let name = input.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "input".into());
    let name = if name == "report.json" { "input-report.json".to_string() } else { name };
    let doc = build(imp, &input, &name)?;
    crate::state::Ctx::open(&root)?;
    fs::create_dir_all(&out).with_context(|| format!("creating {}", out.display()))?;
    let raw = out.join(&name);
    if raw != input {
        fs::copy(&input, &raw).with_context(|| format!("copying {} to {}", input.display(), raw.display()))?;
    }
    let (report, tmp) = (out.join("report.json"), out.join("report.json.tmp"));
    fs::write(&tmp, serde_json::to_string_pretty(&doc)?)?;
    fs::rename(&tmp, &report).with_context(|| format!("saving {}", report.display()))?;
    Ok(ImportOut { format: imp.format.name(), report, raw, evidence: doc })
}

/// An imported report, as found under .fast/android.
pub struct Evidence {
    pub path: PathBuf,
    pub doc: Value,
}

/// Every .fast/android/*/report.json, by folder name.
pub fn load(fast: &Path) -> Result<Vec<Evidence>> {
    let Ok(dirs) = fs::read_dir(fast.join("android")) else { return Ok(vec![]) };
    let mut paths: Vec<PathBuf> =
        dirs.filter_map(|d| d.ok()).map(|d| d.path().join("report.json")).filter(|p| p.is_file()).collect();
    paths.sort();
    paths
        .into_iter()
        .map(|path| {
            let doc = fs::read_to_string(&path)
                .ok()
                .and_then(|t| serde_json::from_str(&t).ok())
                .with_context(|| format!("reading {}", path.display()))?;
            Ok(Evidence { path, doc })
        })
        .collect()
}

fn thermal_name(status: u64) -> &'static str {
    ["none", "light", "moderate", "severe", "critical", "emergency", "shutdown"]
        .get(status as usize)
        .copied()
        .unwrap_or("?")
}

/// One line on what the samples say. No jank percentage when there are too few frames for one.
pub fn samples_line(doc: &Value) -> String {
    let n = |key: &str| number(doc, &["samples", key]);
    let runs = match (n("runs"), n("warmup_runs")) {
        (Some(r), Some(w)) if w > 0.0 => format!(" over {r} runs after {w} warm-up"),
        (Some(r), _) => format!(" over {r} runs"),
        _ => String::new(),
    };
    let why = text(doc, &["comparability", "sample_reason"]).unwrap_or_else(|| "too few frames".into());
    if let Some(cpu) = n("cpu_samples") {
        let over = n("duration_s").map(|d| format!(" over {d}s")).unwrap_or_default();
        if text(doc, &["comparability", "sample_status"]).as_deref() != Some("ok") {
            return format!("{cpu} CPU samples{over}: insufficient ({why})");
        }
        let top: Vec<String> = get(doc, &["hot_methods"])
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .take(5)
                    .filter_map(|m| Some(format!("`{}` {:.1}%", m["symbol"].as_str()?, m["total"].as_f64()? * 100.0)))
                    .collect()
            })
            .unwrap_or_default();
        let top = if top.is_empty() {
            "none of the app's own methods showed up".into()
        } else {
            format!("hottest app methods: {}", top.join(", "))
        };
        return format!("**{cpu}** CPU samples{over}; {top}");
    }
    let Some(frames) = n("frames") else { return format!("samples{runs}: insufficient ({why})") };
    let s = if frames == 1.0 { "" } else { "s" };
    if text(doc, &["comparability", "sample_status"]).as_deref() != Some("ok") {
        return format!("{frames} frame{s}{runs}: insufficient for a jank percentage ({why})");
    }
    let mut parts = vec![format!("**{frames}** frame{s}{runs}")];
    if let (Some(j), Some(p)) = (n("janky_frames"), n("janky_percent")) {
        parts.push(format!("**{p:.2}%** janky ({j})"));
    }
    let pct: Vec<String> = ["p50", "p90", "p95", "p99"]
        .iter()
        .filter_map(|p| n(&format!("{p}_ms")).map(|v| format!("{p} {v}ms")))
        .collect();
    if !pct.is_empty() {
        parts.push(pct.join(", "));
    }
    if let (Some(lo), Some(hi)) = (n("min_ms"), n("max_ms")) {
        parts.push(format!("frames from {lo}ms to {hi}ms"));
    }
    parts.join("; ")
}

/// The report's "On-device (ART)" section. It never sets these numbers against the host ones.
pub fn markdown(list: &[Evidence], after_host: bool) -> String {
    let mut md = String::from("\n## On-device (ART)\n\n");
    md.push_str(if after_host {
        "Measured on the device and imported with `fast-anything import`, not by fast-anything's bench. ART timings can't be compared with the host numbers above, so no speed-up is computed across the two.\n"
    } else {
        "Measured on the device and imported with `fast-anything import`, not by fast-anything's bench. ART timings can't be compared with host numbers, so no speed-up is computed from them.\n"
    });
    for e in list {
        let t = |path: &[&str]| text(&e.doc, path);
        let with = |out: &mut String, path: &[&str], fmt: &dyn Fn(String) -> String| {
            if let Some(v) = t(path) {
                out.push_str(&fmt(v));
            }
        };
        let mut build = t(&["build", "package"]).unwrap_or_default();
        with(&mut build, &["build", "version_name"], &|v| format!(" {v}"));
        with(&mut build, &["build", "version_code"], &|v| format!(" ({v})"));
        with(&mut build, &["build", "variant"], &|v| format!(", {v}"));
        let source = t(&["source"]).unwrap_or_else(|| "evidence".into());
        md.push_str(&format!("\n### {source}: {build}\n\n"));
        let mut device = t(&["device", "serial"]).unwrap_or_default();
        with(&mut device, &["device", "model"], &|v| format!(" ({v})"));
        with(&mut device, &["device", "android_version"], &|v| format!(", Android {v}"));
        with(&mut device, &["device", "api_level"], &|v| format!(" (API {v})"));
        with(&mut device, &["device", "abi"], &|v| format!(", {v}"));
        md.push_str(&format!("- measured on {}: {device}\n", t(&["measured_on"]).unwrap_or_default()));
        let thermal = match get(&e.doc, &["thermal", "status"]).and_then(Value::as_u64) {
            Some(s) => format!("{s} ({})", thermal_name(s)),
            None => "not recorded".into(),
        };
        md.push_str(&format!("- thermal status at capture: {thermal}\n- {}\n", samples_line(&e.doc)));
        if let Some(s) = t(&["symbolization"]) {
            let why = t(&["symbolization_reason"]).map(|r| format!(": {r}")).unwrap_or_default();
            md.push_str(&format!("- symbolization: {s}{why}\n"));
        }
        let mut prov = format!(
            "- raw input: `{}`, sha256 `{}`",
            t(&["provenance", "input"]).unwrap_or_else(|| "?".into()),
            t(&["provenance", "input_sha256"]).unwrap_or_default()
        );
        with(&mut prov, &["provenance", "captured_by"], &|v| format!(", captured by {v}"));
        with(&mut prov, &["captured_at"], &|v| format!(" at {v}"));
        md.push_str(&prov);
        md.push('\n');
    }
    md
}

#[cfg(test)]
mod tests {
    use super::*;

    const GFXINFO: &str = "Applications Graphics Acceleration Info:
Uptime: 5251932 Realtime: 5251932

** Graphics info for pid 4321 [com.example.app] **

Stats since: 5241383187417ns
Total frames rendered: 412
Janky frames: 91 (22.09%)
Janky frames (legacy): 80 (19.42%)
50th percentile: 7ms
90th percentile: 22ms
95th percentile: 26ms
99th percentile: 34ms
Number Missed Vsync: 10
HISTOGRAM: 5ms=120 6ms=80 7ms=50 8ms=0 22ms=40 34ms=10 101ms=2 150ms=0
50th gpu percentile: 3ms
99th gpu percentile: 12ms
GPU HISTOGRAM: 1ms=300 2ms=100 13ms=12
";

    const ONE_FRAME: &str = "** Graphics info for pid 99 [com.example.app] **
Total frames rendered: 1
Janky frames: 1 (100.00%)
50th percentile: 150ms
90th percentile: 150ms
95th percentile: 150ms
99th percentile: 150ms
";

    fn meta() -> Meta {
        Meta {
            package: Some("com.example.app".into()),
            device: Some("DM9X8".into()),
            android_version: Some("14".into()),
            api: Some(34),
            abi: Some("arm64-v8a".into()),
            build_variant: Some("debug".into()),
            thermal_status: Some(0),
            runs: Some(5),
            warmup: Some(1),
            ..Meta::default()
        }
    }

    /// A project folder with a scan and a bench in it.
    fn project(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("fa-android-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join(".fast")).unwrap();
        fs::write(dir.join(".fast/state.json"), r#"{"version": 1, "wall": 2.5, "bench": {"speedup": 8.0}}"#).unwrap();
        fs::write(dir.join(".fast/profile.json"), "{\"functions\": []}").unwrap();
        dir
    }

    fn input(dir: &Path, name: &str, text: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, text).unwrap();
        path
    }

    fn import_of(format: Format, input: PathBuf, meta: Meta) -> Import {
        Import { format, input, meta, out: None, symbols: None }
    }

    #[test]
    fn gfxinfo_text_is_read() {
        let g = parse_gfxinfo(GFXINFO).unwrap();
        assert_eq!(
            g,
            Gfxinfo {
                package: Some("com.example.app".into()),
                frames: 412,
                janky: Some(91),
                janky_percent: Some(22.09),
                p50: Some(7.0),
                p90: Some(22.0),
                p95: Some(26.0),
                p99: Some(34.0),
                min: Some(5.0),
                max: Some(101.0),
            },
            "legacy jank, gpu percentiles and the gpu histogram are other measures"
        );
        assert!(parse_gfxinfo("Uptime: 1\n").unwrap_err().to_string().contains("dumpsys gfxinfo"));
        let two = format!("{ONE_FRAME}{ONE_FRAME}");
        assert!(parse_gfxinfo(&two).unwrap_err().to_string().contains("2 processes"));
        let utf16: Vec<u8> =
            [0xff, 0xfe].into_iter().chain(ONE_FRAME.encode_utf16().flat_map(|u| u.to_le_bytes())).collect();
        assert_eq!(parse_gfxinfo(&decode(&utf16)).unwrap().frames, 1, "PowerShell 5 writes UTF-16");
    }

    #[test]
    fn gfxinfo_import_keeps_scan_and_bench_untouched() {
        let dir = project("gfxinfo");
        let (state, profile) =
            (fs::read(dir.join(".fast/state.json")).unwrap(), fs::read(dir.join(".fast/profile.json")).unwrap());
        let raw = input(&dir, "gfxinfo.txt", GFXINFO);
        let out = import(&dir, &import_of(Format::Gfxinfo, raw.clone(), meta())).unwrap();
        assert_eq!(out.report, dir.join(".fast/android/gfxinfo/report.json"));
        assert_eq!(fs::read(&out.raw).unwrap(), GFXINFO.as_bytes(), "the raw input is kept next to the report");
        assert_eq!(fs::read(dir.join(".fast/state.json")).unwrap(), state);
        assert_eq!(fs::read(dir.join(".fast/profile.json")).unwrap(), profile);

        let doc: Value = serde_json::from_str(&fs::read_to_string(&out.report).unwrap()).unwrap();
        assert_eq!(
            (&doc["schema_version"], &doc["measured_on"], &doc["source"]),
            (&json!(1), &json!("ART"), &json!("android-gfxinfo"))
        );
        assert_eq!(
            doc["device"],
            json!({ "serial": "DM9X8", "android_version": "14", "api_level": 34, "abi": "arm64-v8a" })
        );
        assert_eq!(doc["build"], json!({ "package": "com.example.app", "variant": "debug" }));
        assert_eq!(doc["thermal"], json!({ "status": 0 }));
        assert_eq!(
            doc["samples"],
            json!({ "runs": 5, "warmup_runs": 1, "frames": 412, "janky_frames": 91, "janky_percent": 22.09,
                    "p50_ms": 7, "p90_ms": 22, "p95_ms": 26, "p99_ms": 34, "min_ms": 5, "max_ms": 101 })
        );
        assert_eq!(
            doc["provenance"],
            json!({ "input": "gfxinfo.txt", "input_sha256": sha256::hex(GFXINFO.as_bytes()) })
        );
        assert_eq!(
            doc["comparability"],
            json!({ "host_scan": false, "reason": NOT_COMPARABLE, "warmup": true, "thermal_controlled": true, "sample_status": "ok" })
        );
        let loaded = load(&dir.join(".fast")).unwrap();
        assert_eq!((loaded.len(), &loaded[0].doc), (1, &doc));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_metadata_fails_and_writes_nothing() {
        let dir = std::env::temp_dir().join(format!("fa-android-missing-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let raw = input(&dir, "gfxinfo.txt", GFXINFO);
        for (flag, strip) in [
            ("--device", (|m: &mut Meta| m.device = None) as fn(&mut Meta)),
            ("--package", |m| m.package = None),
            ("--android-version", |m| m.android_version = None),
            ("--abi", |m| m.abi = None),
            ("--build-variant", |m| m.build_variant = None),
        ] {
            let mut m = meta();
            strip(&mut m);
            let err = import(&dir, &import_of(Format::Gfxinfo, raw.clone(), m)).unwrap_err().to_string();
            assert!(err.starts_with("E_ANDROID_METADATA_REQUIRED: missing ") && err.contains(flag), "{err}");
            assert!(!dir.join(".fast").exists(), "nothing written");
        }
        let wrong = Meta { package: Some("com.other".into()), ..meta() };
        let err = import(&dir, &import_of(Format::Gfxinfo, raw.clone(), wrong)).unwrap_err().to_string();
        assert!(err.contains("the capture is of com.example.app"), "{err}");
        let outside = Import { out: Some(dir.join(".fast")), ..import_of(Format::Gfxinfo, raw, meta()) };
        assert!(import(&dir, &outside).unwrap_err().to_string().contains("--out has to be a folder inside"));
        assert!(!dir.join(".fast").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn one_frame_is_insufficient_not_a_headline() {
        let dir = project("one-frame");
        let out = import(&dir, &import_of(Format::Gfxinfo, input(&dir, "one.txt", ONE_FRAME), meta())).unwrap();
        let c = &out.evidence["comparability"];
        assert_eq!(
            (&c["sample_status"], &c["sample_reason"]),
            (&json!("insufficient"), &json!("1 frame, 30 needed for a jank percentage"))
        );
        let md = markdown(&load(&dir.join(".fast")).unwrap(), true);
        assert!(md.contains("1 frame over 5 runs after 1 warm-up: insufficient for a jank percentage"), "{md}");
        assert!(!md.contains("100") && !md.contains("150ms"), "no percentage or percentile headline: {md}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn traces_are_kept_but_not_parsed() {
        let dir = project("perfetto");
        let trace = input(&dir, "trace.perfetto-trace", "\x0a\x05trace bytes");
        let mapping = input(&dir, "mapping.txt", "com.example.A -> a:\n");
        let imp = Import { symbols: Some(mapping), ..import_of(Format::Perfetto, trace, meta()) };
        let out = import(&dir, &imp).unwrap();
        assert_eq!(out.report, dir.join(".fast/android/perfetto/report.json"));
        assert_eq!(fs::read(&out.raw).unwrap(), b"\x0a\x05trace bytes");
        let doc = &out.evidence;
        assert_eq!((&doc["source"], &doc["symbolization"]), (&json!("android-perfetto"), &json!("not attempted")));
        assert_eq!(doc["provenance"]["input_sha256"], json!(sha256::hex(b"\x0a\x05trace bytes")));
        assert_eq!(doc["provenance"]["mapping"]["sha256"], json!(sha256::hex(b"com.example.A -> a:\n")));
        assert_eq!(doc["comparability"]["sample_status"], json!("insufficient"), "a trace has no frame count here");
        assert!(markdown(&load(&dir.join(".fast")).unwrap(), true).contains("- symbolization: not attempted: "));

        let perf = input(&dir, "perf.data", "PERFILE2");
        let out = import(&dir, &import_of(Format::Simpleperf, perf, meta())).unwrap();
        assert_eq!(out.report, dir.join(".fast/android/simpleperf/report.json"));
        assert_eq!(load(&dir.join(".fast")).unwrap().len(), 2);
        let empty = input(&dir, "empty.data", "");
        assert!(
            import(&dir, &import_of(Format::Simpleperf, empty, meta())).unwrap_err().to_string().contains("is empty")
        );
        let _ = fs::remove_dir_all(&dir);
    }

    fn summary() -> Value {
        json!({
            "schema_version": 1,
            "source": "android-macrobenchmark",
            "measured_on": "ART",
            "device": { "serial": "DM9X8", "android_version": 14, "abi": "arm64-v8a" },
            "build": { "package": "com.example.app", "variant": "benchmark" },
            "thermal": { "status": 1 },
            "samples": { "frames": 600, "janky_percent": 3.5 },
            "provenance": { "input": "run.json", "input_sha256": "ab".repeat(32) },
            "comparability": { "host_scan": false, "sample_status": "ok" }
        })
    }

    #[test]
    fn json_reports_are_checked() {
        let ok = validate(summary()).unwrap();
        assert_eq!(ok["comparability"]["reason"], json!(NOT_COMPARABLE));
        assert_eq!(ok["comparability"]["thermal_controlled"], json!(false), "status 1 is throttling");

        let broken = |change: fn(&mut Value)| {
            let mut doc = summary();
            change(&mut doc);
            validate(doc).unwrap_err().to_string()
        };
        assert!(broken(|d| d["schema_version"] = json!(2)).contains("reads version 1"));
        assert!(
            broken(|d| {
                d.as_object_mut().unwrap().remove("measured_on");
            })
            .contains("measured_on is required")
        );
        assert!(broken(|d| d["comparability"]["host_scan"] = json!(true)).contains("can't be compared"));
        assert!(broken(|d| d["provenance"]["input_sha256"] = json!("abc")).contains("input_sha256 is required"));
        assert!(broken(|d| d["thermal"]["status"] = json!(7)).contains("0 (none) to 6"));
        assert!(
            broken(|d| {
                d.as_object_mut().unwrap().remove("thermal");
            })
            .contains("thermal is required")
        );
        assert!(broken(|d| d["build"] = json!({})).contains(
            "E_ANDROID_METADATA_REQUIRED: missing --package (build.package), --build-variant (build.variant)"
        ));

        let mut few = summary();
        few["samples"]["frames"] = json!(12);
        assert_eq!(
            validate(few).unwrap()["comparability"]["sample_status"],
            json!("insufficient"),
            "12 frames aren't ok"
        );
    }

    #[test]
    fn json_import_fills_in_flags_and_refuses_conflicts() {
        let dir = project("json");
        let mut doc = summary();
        doc["build"].as_object_mut().unwrap().remove("variant");
        doc["provenance"].as_object_mut().unwrap().remove("input_sha256");
        let text = doc.to_string();
        let file = input(&dir, "report.json", &text);
        let missing = import(&dir, &import_of(Format::Json, file.clone(), Meta::default())).unwrap_err();
        assert!(missing.to_string().contains("--build-variant (build.variant)"), "{missing}");
        let flags =
            Meta { build_variant: Some("benchmark".into()), android_version: Some("14".into()), ..Meta::default() };
        let out = import(&dir, &import_of(Format::Json, file.clone(), flags)).unwrap();
        assert_eq!(out.report, dir.join(".fast/android/json/report.json"));
        assert_eq!(
            out.raw,
            dir.join(".fast/android/json/input-report.json"),
            "the raw input doesn't overwrite the report"
        );
        assert_eq!(out.evidence["build"]["variant"], json!("benchmark"));
        assert_eq!(out.evidence["provenance"]["input_sha256"], json!(sha256::hex(text.as_bytes())));
        let conflict = Meta { device: Some("OTHER".into()), ..Meta::default() };
        let err = import(&dir, &import_of(Format::Json, file, conflict)).unwrap_err().to_string();
        assert!(err.contains("--device is OTHER but the report says device.serial is \"DM9X8\""), "{err}");
        let _ = fs::remove_dir_all(&dir);
    }
}
