//! An on-device scan of an Android app (#136): simpleperf records it through adb, its report ranks the
//! app's own methods, and the result goes in .fast/android/scan as on-device (ART) evidence. Ranking
//! only: Android code can't be ported to Rust yet, and ART times are never compared with host ones.

use crate::android::{self, Format, Meta};
use crate::{sha256, util};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use serde_json::{Map, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

/// Where the capture is written on the device while it records.
const ON_DEVICE: &str = "/data/local/tmp/fast-anything-perf.data";
pub const DEFAULT_SECONDS: f64 = 10.0;

pub struct Options {
    pub package: String,
    /// adb's serial; None when exactly one device is connected.
    pub device: Option<String>,
    pub seconds: f64,
}

/// One row of simpleperf report --children --sort symbol.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Method {
    pub symbol: String,
    /// Share of samples with this method on the stack, callees included.
    pub total: f64,
    /// Share of samples in the method's own body.
    #[serde(rename = "self")]
    pub own: f64,
    pub samples: u64,
}

#[derive(Debug, Serialize)]
pub struct ScanOut {
    pub package: String,
    pub device: String,
    pub model: Option<String>,
    pub android_version: Option<String>,
    pub api: Option<u32>,
    pub abi: Option<String>,
    pub build_variant: String,
    pub thermal_status: Option<u8>,
    pub seconds: f64,
    pub samples: u64,
    /// The app's own methods, hottest first.
    pub methods: Vec<Method>,
    /// Where the rest of the time went (framework, ART, native), by own time.
    pub elsewhere: Vec<Method>,
    pub started_app: bool,
    pub report: PathBuf,
    pub raw: PathBuf,
}

/// How the scan talks to the device: adb, or a fake one in the tests.
pub trait Device {
    /// adb shell <args>: what it printed, or an error with it when it failed.
    fn shell(&self, args: &[&str]) -> Result<String>;
    /// adb pull <remote> <local>
    fn pull(&self, remote: &str, local: &Path) -> Result<()>;
    fn serial(&self) -> Result<String>;
}

pub struct Adb {
    exe: PathBuf,
    serial: Option<String>,
}

impl Adb {
    pub fn find(serial: Option<String>) -> Result<Adb> {
        let exe = adb_path().context(
            "adb isn't installed: get the Android SDK platform-tools (https://developer.android.com/tools/adb), put adb on PATH or set ANDROID_HOME",
        )?;
        Ok(Adb { exe, serial })
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(&self.exe);
        if let Some(s) = &self.serial {
            cmd.args(["-s", s]);
        }
        cmd.args(args).stdin(Stdio::null());
        cmd
    }

    fn run(&self, args: &[&str]) -> Result<String> {
        let (out, _) =
            util::output_for(&mut self.command(args), "adb", Some(Duration::from_secs(600)), Duration::ZERO)?;
        let text = util::combined(&out);
        ensure!(out.status.success(), "adb {} failed:\n{}", args.join(" "), util::tail(&text, 20));
        Ok(text)
    }
}

impl Device for Adb {
    fn shell(&self, args: &[&str]) -> Result<String> {
        let mut all = vec!["shell"];
        all.extend_from_slice(args);
        self.run(&all)
    }

    fn pull(&self, remote: &str, local: &Path) -> Result<()> {
        self.run(&["pull", remote, &local.to_string_lossy()]).map(|_| ())
    }

    fn serial(&self) -> Result<String> {
        let state = self.run(&["get-state"]).context("no Android device is ready: connect one with USB debugging on (adb devices lists it), and pick one with --device when there are several")?;
        ensure!(state.trim() == "device", "the device is {}: unlock it and accept USB debugging", state.trim());
        Ok(self.run(&["get-serialno"])?.trim().to_string())
    }
}

/// adb from PATH, or from the SDK that ANDROID_HOME / ANDROID_SDK_ROOT (or Android Studio's default) points to.
fn adb_path() -> Option<PathBuf> {
    let name = if cfg!(windows) { "adb.exe" } else { "adb" };
    let works =
        |p: &Path| Command::new(p).arg("version").stdin(Stdio::null()).output().is_ok_and(|o| o.status.success());
    if works(Path::new("adb")) {
        return Some(PathBuf::from("adb"));
    }
    let mut sdks: Vec<PathBuf> =
        ["ANDROID_HOME", "ANDROID_SDK_ROOT"].iter().filter_map(std::env::var_os).map(PathBuf::from).collect();
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        sdks.push(PathBuf::from(local).join("Android").join("Sdk"));
    }
    if let Some(home) = std::env::var_os("HOME") {
        sdks.push(PathBuf::from(&home).join("Library/Android/sdk"));
        sdks.push(PathBuf::from(home).join("Android/Sdk"));
    }
    sdks.into_iter().map(|s| s.join("platform-tools").join(name)).find(|p| works(p))
}

/// Total samples and the rows of  simpleperf report --children --sort symbol -n.
pub fn parse_report(text: &str) -> Result<(u64, Vec<Method>)> {
    let samples = text
        .lines()
        .find_map(|l| l.trim().strip_prefix("Samples:").and_then(|n| n.trim().parse().ok()))
        .context("simpleperf report printed no sample count: did the recording catch the app running?")?;
    let pct = |s: &str| s.strip_suffix('%').and_then(|n| n.parse::<f64>().ok()).map(|p| p / 100.0);
    let rows = text
        .lines()
        .filter_map(|l| {
            let mut parts = l.split_whitespace();
            let total = pct(parts.next()?)?;
            let own = pct(parts.next()?)?;
            let rest: Vec<&str> = parts.collect();
            let (samples, symbol) = match rest.split_first() {
                Some((n, sym)) if !sym.is_empty() && n.parse::<u64>().is_ok() => {
                    (n.parse().unwrap_or(0), sym.join(" "))
                }
                _ => (0, rest.join(" ")),
            };
            (!symbol.is_empty()).then_some(Method { symbol, total, own, samples })
        })
        .collect();
    Ok((samples, rows))
}

/// Whether a symbol is one of the app's own methods: "com.example.app.Foo.bar" or "void com.example.app.Foo.bar(int)".
fn is_app(symbol: &str, prefixes: &[String]) -> bool {
    symbol.split_whitespace().any(|t| prefixes.iter().any(|p| t.starts_with(&format!("{p}.")) || t == p))
}

/// The Java/Kotlin packages the app's code lives in: its package, and every namespace / applicationId
/// in the project's Gradle files (they often differ: a debug suffix, a renamed namespace).
pub fn app_prefixes(root: &Path, package: &str) -> Vec<String> {
    let mut found = vec![package.to_string()];
    if let Some(base) = package.strip_suffix(".debug") {
        found.push(base.to_string());
    }
    let mut stack = vec![(root.to_path_buf(), 0)];
    while let Some((dir, depth)) = stack.pop() {
        for f in ["build.gradle.kts", "build.gradle"] {
            if let Ok(text) = fs::read_to_string(dir.join(f)) {
                found.extend(gradle_ids(&text));
            }
        }
        if depth < 2
            && let Ok(entries) = fs::read_dir(&dir)
        {
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if e.path().is_dir()
                    && !name.starts_with('.')
                    && !["build", "src", "node_modules"].contains(&name.as_str())
                {
                    stack.push((e.path(), depth + 1));
                }
            }
        }
    }
    found.sort();
    found.dedup();
    found
}

/// namespace = "x" / applicationId "x" in a Gradle build file.
fn gradle_ids(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|l| {
            let l = l.trim();
            let rest = l.strip_prefix("namespace").or_else(|| l.strip_prefix("applicationId"))?;
            let value = rest.trim_start_matches([' ', '=', '(']).trim();
            let value = value.strip_prefix('"').or_else(|| value.strip_prefix('\''))?;
            let id = value.split(['"', '\'']).next()?;
            (!id.is_empty() && id.contains('.')).then(|| id.to_string())
        })
        .collect()
}

/// versionName, versionCode and whether the build is debuggable, from  dumpsys package <pkg>.
fn parse_package(text: &str) -> (Option<String>, Option<u64>, bool) {
    let field = |key: &str| {
        text.split_whitespace()
            .find_map(|t| t.strip_prefix(key))
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    let debuggable = text.lines().any(|l| l.contains("flags=[") && l.contains("DEBUGGABLE"));
    (field("versionName="), field("versionCode=").and_then(|v| v.parse().ok()), debuggable)
}

/// The device's thermal status (0 none .. 6 shutdown), from  dumpsys thermalservice.
fn parse_thermal(text: &str) -> Option<u8> {
    text.lines().find_map(|l| l.trim().strip_prefix("Thermal Status:")?.trim().parse().ok()).filter(|&s| s <= 6)
}

fn prop(dev: &dyn Device, name: &str) -> Option<String> {
    dev.shell(&["getprop", name]).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

pub fn scan(root: &Path, opts: &Options) -> Result<ScanOut> {
    ensure!(opts.seconds.is_finite() && opts.seconds > 0.0, "--duration takes a number of seconds, e.g. --duration 10");
    let adb = Adb::find(opts.device.clone())?;
    scan_on(&adb, root, opts)
}

pub fn scan_on(dev: &dyn Device, root: &Path, opts: &Options) -> Result<ScanOut> {
    let root = util::absolute(root);
    ensure!(root.is_dir(), "{} isn't a folder", root.display());
    let pkg = opts.package.as_str();
    ensure!(
        !pkg.is_empty()
            && pkg.split('.').all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')),
        "--android takes the app's package, e.g. com.example.app"
    );
    let serial = dev.serial()?;
    let installed = dev.shell(&["pm", "path", pkg]).unwrap_or_default();
    ensure!(
        installed.contains("package:"),
        "{pkg} isn't installed on {serial}: install a debuggable build first (./gradlew installDebug)"
    );
    let (version_name, version_code, debuggable) =
        parse_package(&dev.shell(&["dumpsys", "package", pkg]).unwrap_or_default());
    let thermal = dev.shell(&["dumpsys", "thermalservice"]).ok().and_then(|t| parse_thermal(&t));
    let started_app = dev.shell(&["pidof", pkg]).map(|p| p.trim().is_empty()).unwrap_or(true);
    if started_app {
        dev.shell(&["monkey", "-p", pkg, "-c", "android.intent.category.LAUNCHER", "1"])
            .with_context(|| format!("starting {pkg}"))?;
        std::thread::sleep(Duration::from_secs(2));
    }
    let secs = format!("{}", opts.seconds);
    dev.shell(&["simpleperf", "record", "--app", pkg, "-g", "-e", "cpu-clock", "-f", "1000", "--duration", &secs, "-o", ON_DEVICE])
        .map_err(|e| {
            e.context(format!(
                "simpleperf couldn't record {pkg}: it profiles debuggable builds, or release builds with <profileable android:shell=\"true\"/> in the manifest (Android 10+)"
            ))
        })?;
    let report = dev.shell(&[
        "simpleperf",
        "report",
        "-i",
        ON_DEVICE,
        "--children",
        "--sort",
        "symbol",
        "-n",
        "--percent-limit",
        "0.1",
    ])?;
    let (samples, rows) = parse_report(&report)?;
    ensure!(samples > 0, "simpleperf recorded no samples of {pkg}: use the app while it records (--duration {secs})");

    let out = root.join(".fast").join("android").join("scan");
    crate::state::Ctx::open(&root)?;
    fs::create_dir_all(&out).with_context(|| format!("creating {}", out.display()))?;
    let raw = out.join("perf.data");
    dev.pull(ON_DEVICE, &raw)?;
    let _ = dev.shell(&["rm", "-f", ON_DEVICE]);
    fs::write(out.join("report.txt"), &report)?;

    let prefixes = app_prefixes(&root, pkg);
    let (mut methods, mut elsewhere): (Vec<Method>, Vec<Method>) =
        rows.into_iter().partition(|m| is_app(&m.symbol, &prefixes));
    methods.sort_by(|a, b| b.total.total_cmp(&a.total));
    elsewhere.sort_by(|a, b| b.own.total_cmp(&a.own));
    elsewhere.truncate(5);

    let api = prop(dev, "ro.build.version.sdk").and_then(|v| v.parse().ok());
    let variant = if debuggable { "debug" } else { "release" };
    let meta = Meta {
        package: Some(pkg.into()),
        device: Some(serial.clone()),
        model: prop(dev, "ro.product.model"),
        android_version: prop(dev, "ro.build.version.release"),
        api,
        abi: prop(dev, "ro.product.cpu.abi"),
        build_variant: Some(variant.into()),
        version_name,
        version_code,
        thermal_status: thermal,
        captured_at: util::now_utc(),
        captured_by: Some(format!("fast-anything {} scan --android", env!("CARGO_PKG_VERSION"))),
        ..Meta::default()
    };
    let mut samples_map = Map::new();
    samples_map.insert("cpu_samples".into(), json!(samples));
    samples_map.insert("duration_s".into(), json!(opts.seconds));
    let bytes = fs::read(&raw).with_context(|| format!("reading {}", raw.display()))?;
    let mut doc = android::skeleton(Format::Simpleperf, samples_map, "perf.data", &sha256::hex(&bytes));
    doc["symbolization"] = json!("simpleperf report on the device");
    doc["hot_methods"] = json!(methods.iter().take(20).collect::<Vec<_>>());
    doc["elsewhere"] = json!(elsewhere);
    android::apply(&mut doc, &meta)?;
    let doc = android::validate(doc)?;
    let (path, tmp) = (out.join("report.json"), out.join("report.json.tmp"));
    fs::write(&tmp, serde_json::to_string_pretty(&doc)?)?;
    fs::rename(&tmp, &path).with_context(|| format!("saving {}", path.display()))?;

    let text = |k: &str| doc["device"][k].as_str().map(String::from);
    Ok(ScanOut {
        package: pkg.into(),
        device: serial,
        model: text("model"),
        android_version: text("android_version"),
        api,
        abi: text("abi"),
        build_variant: variant.into(),
        thermal_status: thermal,
        seconds: opts.seconds,
        samples,
        methods,
        elsewhere,
        started_app,
        report: path,
        raw,
    })
}

/// What an Android scan can't do, for scans that look like Android ones.
pub fn hint(package: Option<&str>) -> String {
    format!(
        "to rank an Android app's methods on a device:  fast-anything scan --android {} --duration 10",
        package.unwrap_or("<package>")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    const REPORT: &str = "Cmdline: /system/bin/simpleperf record --app com.example.app -g -e cpu-clock
Arch: arm64
Event: cpu-clock (type 1, config 0)
Samples: 9966
Event count: 9966000000

Children  Self    Sample  Symbol
93.21%    0.00%   0       __start_thread
41.50%    1.20%   120     com.example.app.Schedule.parse
30.10%    30.10%  3000    void com.example.app.json.Lexer.next(int)
12.00%    12.00%  1196    art::gc::Heap::CollectGarbageInternal
 5.00%     5.00%  498     androidx.compose.runtime.Recomposer.run
";

    struct Fake {
        calls: RefCell<Vec<String>>,
        running: bool,
    }

    impl Device for Fake {
        fn shell(&self, args: &[&str]) -> Result<String> {
            let line = args.join(" ");
            self.calls.borrow_mut().push(line.clone());
            Ok(match args[0] {
                "pm" => "package:/data/app/base.apk\n".into(),
                "dumpsys" if args[1] == "package" => {
                    "    versionCode=18 minSdk=24 targetSdk=36\n    versionName=5.1.3\n    flags=[ DEBUGGABLE HAS_CODE ALLOW_CLEAR_USER_DATA ]\n".into()
                }
                "dumpsys" => "IsStatusOverride: false\nThermal Status: 0\n".into(),
                "pidof" => if self.running { "4321\n".into() } else { String::new() },
                "getprop" => match args[1] {
                    "ro.product.model" => "22041216G\n",
                    "ro.build.version.release" => "14\n",
                    "ro.build.version.sdk" => "34\n",
                    "ro.product.cpu.abi" => "arm64-v8a\n",
                    _ => "\n",
                }
                .into(),
                "simpleperf" if args[1] == "report" => REPORT.into(),
                _ => String::new(),
            })
        }

        fn pull(&self, _remote: &str, local: &Path) -> Result<()> {
            fs::write(local, b"PERFILE2 fake").map_err(Into::into)
        }

        fn serial(&self) -> Result<String> {
            Ok("DM9X8".into())
        }
    }

    #[test]
    fn report_rows_are_read() {
        let (n, rows) = parse_report(REPORT).unwrap();
        assert_eq!(n, 9966);
        assert_eq!(rows.len(), 5);
        assert_eq!(
            rows[2],
            Method {
                symbol: "void com.example.app.json.Lexer.next(int)".into(),
                total: 0.301,
                own: 0.301,
                samples: 3000
            }
        );
        assert!(parse_report("Children Self Symbol\n").is_err(), "no sample count");
        let (_, no_counts) = parse_report("Samples: 10\n50.00%  10.00%  com.x.A.b\n").unwrap();
        assert_eq!((no_counts[0].samples, no_counts[0].symbol.as_str()), (0, "com.x.A.b"));
    }

    #[test]
    fn package_thermal_and_gradle_ids() {
        let (name, code, debug) =
            parse_package("versionCode=18 minSdk=24\n versionName=5.1.3\n flags=[ DEBUGGABLE HAS_CODE ]\n");
        assert_eq!((name.as_deref(), code, debug), (Some("5.1.3"), Some(18), true));
        assert!(!parse_package("flags=[ HAS_CODE ]\n").2);
        assert_eq!(parse_thermal("Thermal Status: 2\n"), Some(2));
        assert_eq!(parse_thermal("nothing\n"), None);
        let ids = gradle_ids(
            "android {\n    namespace = \"com.aistudio.fitness\"\n    defaultConfig {\n        applicationId \"com.aistudio.fitness.ironpulse\"\n    }\n}\n",
        );
        assert_eq!(ids, ["com.aistudio.fitness", "com.aistudio.fitness.ironpulse"]);
        assert!(is_app("void com.x.app.A.b(int)", &["com.x.app".into()]));
        assert!(!is_app("com.x.application.A.b", &["com.x.app".into()]));
    }

    #[test]
    fn a_device_scan_ranks_the_apps_methods_and_keeps_the_evidence() {
        let dir = std::env::temp_dir().join(format!("fa-device-scan-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("app")).unwrap();
        fs::write(dir.join("app/build.gradle.kts"), "android {\n    namespace = \"com.example.app\"\n}\n").unwrap();
        let fake = Fake { calls: RefCell::new(vec![]), running: false };
        let opts = Options { package: "com.example.app".into(), device: None, seconds: 5.0 };
        let out = scan_on(&fake, &dir, &opts).unwrap();
        let calls = fake.calls.borrow().join("\n");
        assert!(calls.contains("monkey -p com.example.app"), "the app wasn't running, so it's started: {calls}");
        assert!(
            calls.contains("simpleperf record --app com.example.app -g -e cpu-clock -f 1000 --duration 5 -o"),
            "{calls}"
        );
        assert!(out.started_app && out.samples == 9966 && out.build_variant == "debug");
        let ids: Vec<&str> = out.methods.iter().map(|m| m.symbol.as_str()).collect();
        assert_eq!(ids, ["com.example.app.Schedule.parse", "void com.example.app.json.Lexer.next(int)"]);
        assert_eq!(out.elsewhere[0].symbol, "art::gc::Heap::CollectGarbageInternal", "by own time");

        let doc: serde_json::Value = serde_json::from_str(&fs::read_to_string(&out.report).unwrap()).unwrap();
        assert_eq!((&doc["measured_on"], &doc["source"]), (&json!("ART"), &json!("android-simpleperf")));
        assert_eq!(
            doc["device"],
            json!({ "serial": "DM9X8", "model": "22041216G", "android_version": "14", "api_level": 34, "abi": "arm64-v8a" })
        );
        assert_eq!(
            doc["build"],
            json!({ "package": "com.example.app", "variant": "debug", "version_name": "5.1.3", "version_code": 18 })
        );
        assert_eq!(doc["comparability"]["sample_status"], json!("ok"));
        assert_eq!(doc["comparability"]["host_scan"], json!(false));
        let line = android::samples_line(&doc);
        assert!(
            line.starts_with(
                "**9966** CPU samples over 5s; hottest app methods: `com.example.app.Schedule.parse` 41.5%"
            ),
            "{line}"
        );
        assert_eq!(android::load(&dir.join(".fast")).unwrap().len(), 1, "report picks it up");

        let running = Fake { calls: RefCell::new(vec![]), running: true };
        assert!(!scan_on(&running, &dir, &opts).unwrap().started_app);
        assert!(!running.calls.borrow().iter().any(|c| c.starts_with("monkey")));
        let bad = Options { package: "com.x;rm -rf /".into(), ..opts };
        assert!(scan_on(&running, &dir, &bad).unwrap_err().to_string().contains("--android takes the app's package"));
        let _ = fs::remove_dir_all(&dir);
    }
}
