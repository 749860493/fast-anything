//! fast-anything doctor: is everything a port needs installed?

use crate::ui::{self, bold, dim, green, red, yellow};
use anyhow::{Result, ensure};
use std::process::Command;

fn version(program: &str, args: &[&str]) -> Option<String> {
    let run = |p: &str| Command::new(p).args(args).output().ok().filter(|o| o.status.success());
    let out = run(program).or_else(|| if cfg!(windows) { run(&format!("{program}.cmd")) } else { None })?;
    let text = String::from_utf8_lossy(if out.stdout.is_empty() { &out.stderr } else { &out.stdout }).into_owned();
    Some(text.lines().next().unwrap_or("").trim().to_string())
}

struct Check {
    name: &'static str,
    found: Option<String>,
    need: &'static str,
    hint: &'static str,
}

pub fn run() -> Result<()> {
    let python = version("python", &["--version"]).or_else(|| version("python3", &["--version"]));
    let checks = [
        Check { name: "cargo", found: version("cargo", &["--version"]), need: "required", hint: "https://rustup.rs" },
        Check { name: "python", found: python, need: "for Python apps (3.10+)", hint: "https://python.org" },
        Check {
            name: "node",
            found: version("node", &["--version"]),
            need: "for JS/TS apps (22+)",
            hint: "https://nodejs.org",
        },
        Check {
            name: "ruby",
            found: version("ruby", &["--version"]),
            need: "for Ruby apps (3.3+)",
            hint: "https://ruby-lang.org",
        },
        Check {
            name: "java",
            found: version("java", &["--version"]),
            need: "for Java apps (22+)",
            hint: "https://adoptium.net",
        },
        Check {
            name: "go",
            found: version("go", &["version"]),
            need: "for Go apps (1.22+)",
            hint: "https://go.dev/dl",
        },
        Check {
            name: "php",
            found: version("php", &["--version"]),
            need: "for PHP apps (8.1+, extension=ffi)",
            hint: "https://php.net",
        },
        Check {
            name: "claude",
            found: version("claude", &["--version"]),
            need: "--agent claude",
            hint: "https://claude.com/claude-code",
        },
        Check {
            name: "codex",
            found: version("codex", &["--version"]),
            need: "--agent codex",
            hint: "npm i -g @openai/codex",
        },
    ];
    for c in &checks {
        match &c.found {
            Some(v) => ui::line(format!("  {} {:<8} {}", green("✓"), bold(c.name), dim(v))),
            None if c.need == "required" => {
                ui::line(format!("  {} {:<8} missing, {}: {}", red("✗"), bold(c.name), c.need, c.hint))
            }
            None => ui::line(format!(
                "  {} {:<8} {}",
                yellow("·"),
                c.name,
                dim(format!("not found ({}): {}", c.need, c.hint))
            )),
        }
    }
    ensure!(checks[0].found.is_some(), "Rust is needed to build ports");
    ensure!(checks[1..7].iter().any(|c| c.found.is_some()), "install Python, Node.js, Ruby, Java, Go or PHP");
    ui::ok("ready");
    Ok(())
}
