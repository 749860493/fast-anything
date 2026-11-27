<p align="center">
  <img src="https://raw.githubusercontent.com/EL4CTEO/fast-anything/main/docs/assets/wordmark.png" alt="fast-anything" width="520">
</p>

<p align="center"><b>Don't rewrite it in Rust. Rewrite the 5% that's slow.</b></p>

<p align="center">
  <a href="https://trendshift.io/repositories/371252" target="_blank" rel="noopener noreferrer"><img src="https://trendshift.io/api/badge/trendshift/repositories/371252/daily?language=Rust" alt="EL4CTEO/fast-anything: Rust repository of the day on Trendshift" width="250" height="55"></a>
</p>

<p align="center">
  <a href="https://www.npmjs.com/package/@el4cteo/fast-anything"><img src="https://img.shields.io/npm/v/@el4cteo/fast-anything?color=f74c00&label=npm" alt="npm"></a>
  <a href="https://crates.io/crates/fast-anything"><img src="https://img.shields.io/crates/v/fast-anything?color=f74c00" alt="crates.io"></a>
  <a href="https://github.com/EL4CTEO/fast-anything/actions/workflows/ci.yml"><img src="https://github.com/EL4CTEO/fast-anything/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <img src="https://img.shields.io/badge/Python%20%C2%B7%20Node%20%C2%B7%20TypeScript%20%C2%B7%20Ruby%20%C2%B7%20Java%20%C2%B7%20Kotlin%20%C2%B7%20Go%20%C2%B7%20PHP%20%C2%B7%20C%20%C2%B7%20C%2B%2B%20%C2%B7%20C%23%20%C2%B7%20R-2563eb" alt="Python, Node, TypeScript, Ruby, Java, Kotlin, Go, PHP, C, C++, C#, R">
  <img src="https://img.shields.io/badge/Claude%20Code-plugin-d97757" alt="Claude Code plugin">
  <img src="https://img.shields.io/badge/MCP-server-555" alt="MCP server">
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-MIT-555" alt="MIT"></a>
</p>

Rewriting a whole app in Rust costs months, minute-long compiles and gigabytes of `target/`. Most of the runtime sits in a few functions anyway.

**fast-anything** finds those functions, has your coding agent port *only them* to Rust, proves the port gives the same results on real recorded calls, and swaps it in. One command, fully reversible.

<p align="center"><img src="https://raw.githubusercontent.com/EL4CTEO/fast-anything/main/docs/assets/demo.gif" alt="fast-anything go --agent claude -- python app.py" width="820"></p>

## Install

```sh
npm i -g @el4cteo/fast-anything      # or: cargo install fast-anything
```

Ports are compiled with your Rust toolchain ([rustup](https://rustup.rs)).

## Use

```sh
fast-anything go --agent claude -- python app.py
```

After `--` goes whatever starts your app: `node app.ts`, `ruby app.rb`, `java App.java`, `gradle run`, `go run .`, `php app.php`, `clang -O2 main.c`, `dotnet run -c Release`, `Rscript app.R`.

Or step by step:

| command | what it does |
|---|---|
| `scan -- <cmd>` | profiles the app, ranks functions by time × how portable they are |
| `port [id]` | records up to 500 real calls, creates a one-function Rust crate + `TASK.md` |
| `verify` | release-builds the port and replays every recorded call |
| `enable` / `disable` | swaps the Rust in or out (one marked block, removed byte for byte) |
| `bench` | times the whole app, original against Rust, with an A/A control for noise; wall and CPU time |
| `report` | markdown + SVG card for your PR |
| `import` | attaches measurements from an Android device; the report keeps them apart from the host numbers |
| `doctor` | checks Rust, your runtimes and your agents |

`--agent` takes `claude`, `codex`, or any command that reads a prompt on stdin. Without it, hand `TASK.md` to whatever you use.

## Proof, not vibes

<p align="center"><img src="https://raw.githubusercontent.com/EL4CTEO/fast-anything/main/docs/assets/verify.gif" alt="verify catches a wrong port" width="820"></p>

That port looked right but compared bytes instead of characters. 148 of 500 real calls caught it.

Nothing is enabled until every case matches:

- **Recorded calls**: up to 500 real calls from your app.
- **Derived cases**, answered by the original: edge values (NaN, ±infinity, -0.0, non-ASCII text, empty and up to 16× bigger inputs) and shrunk inputs.
- **Held-out calls**: every 5th recorded call stays out of the folder the agent works in, so a port fitted to the calls it was shown still fails. It's a guard, not a sandbox: an agent that goes looking can read them in `.fast/holdout`.

Floats must match within 1e-9 of their size (or of the largest value in their list), -0.0 isn't 0.0, and errors must come where the original errors. A port that is right but slower per call isn't enabled either, nor one whose original is too cheap to time per call (C, C++, Go, Java, Kotlin, PHP, C#). `FAST_ANYTHING_DISABLE=1` runs the original any time.

## Results

Real runs on an i5-10500, Windows 11 (C and C++: a 4-core GitHub Actions Linux runner). Reproduce them from [`examples/`](examples).

| app | before | after |
|---|---|---|
| Python fuzzy dedupe (700 names) | 4.91s | **42ms, 116×** |
| TypeScript fuzzy dedupe (2,000 names) | 3.11s | **155ms, 20×** |
| PHP fuzzy dedupe (600 names) | 2.94s | **53ms, 55×** |
| Go fuzzy dedupe (3,000 names) | 3.02s | **144ms, 21×** |
| Ruby fuzzy dedupe (400 names) | 3.13s | **228ms, 14×** |
| C# fuzzy dedupe (4,000 names) | 4.03s | **312ms, 13×** |
| Java fuzzy dedupe (4,000 names) | 3.50s | **1.18s, 3×** (11× on the function; the rest is JVM startup) |
| C mandelbrot (8 zoom levels) | 431ms | **127ms, 3.4×** |
| C++ fuzzy dedupe (6,000 names) | 3.02s | **1.09s, 2.7×** |

fast-anything ports the outermost loop it can, not the tiny function called inside it, so the app crosses into Rust once instead of millions of times. Porting only the inner `levenshtein` gave 35× and 1.9×.

## Why not rewrite it all?

- **Compile time**: each port is a one-function crate. Seconds, not minutes.
- **Cache**: every port shares one `.fast/target`. Delete it whenever you like (`fast-anything clean`).
- **Risk**: your code stays as it is. Rust is opt-in per function, checked against real behavior.

## Agents

**Plugin**: the repo is an agent plugin marketplace ([`.agents/plugins/marketplace.json`](.agents/plugins/marketplace.json)) that bundles the skill and the MCP server. Install the CLI first, then:

- **Codex**: `codex plugin marketplace add EL4CTEO/fast-anything`, then install `fast-anything` from `/plugins`
- **Claude Code**: `/plugin marketplace add EL4CTEO/fast-anything`, then `/plugin install fast-anything@fast-anything` and `/fast-anything:fast python app.py`

Or wire it up by hand:

- **MCP server** (Claude Code, Codex, Cursor…), built into the binary: `claude mcp add fast-anything -- fast-anything mcp`. 8 tools: `fast_scan`, `fast_port`, `fast_verify`, `fast_enable`, `fast_disable`, `fast_bench`, `fast_report`, `fast_status`.
- **Skill**: [`skills/fast-anything`](skills/fast-anything/SKILL.md) teaches any agent the loop.

## Languages

| language | run it with | bridge |
|---|---|---|
| Python 3.10+ | `python app.py` | PyO3 |
| JavaScript / TypeScript (Node 22+, Bun, Deno 2+) | `node app.ts`, `bun app.ts`, `deno run -A app.ts` | napi-rs |
| Ruby 3.3+ | `ruby app.rb` | C bridge, through Fiddle |
| Java 22+ | `java App.java`, Maven, Gradle | C bridge, through the FFM API |
| Kotlin/JVM (JDK 22+) | Gradle, sources in `src/main/kotlin` | the Java bridge |
| Go 1.22+ | `go run .` | C bridge, through the Windows loader or dlopen (cgo) |
| PHP 8.1+ | `php app.php` | C bridge, through FFI (`extension=ffi`) |
| C and C++ (gcc, clang, Intel icx/icpx) | `clang -O2 main.c lib.c` | C bridge; C passes its arguments as they are, no JSON |
| C# / .NET 8+ | `dotnet run -c Release` | C bridge, through NativeLibrary |
| R 4.0+ | `Rscript app.R` | C bridge, through base R's `.C()` |

Servers and GUIs that don't exit: `scan --duration 30` stops the app after 30 seconds and ranks what ran (PHP needs `pcntl`; R: not on Windows).

It ports top-level, deterministic functions that take plain data (numbers, strings, lists, dicts, typed arrays). `scan` says why anything else can't move yet.

- **Python**: module-level functions, `@staticmethod`s, and `@classmethod`s that don't use `cls`. Dataclasses, NamedTuples and numeric NumPy arrays count as plain data.
- **Java, C#, PHP**: static methods (and PHP functions). **Kotlin**: top-level functions, `object` and companion members, extension functions.
- **Go**: plain functions. **C and C++**: free functions and static methods.
- **Entry scripts**: their functions count when the main code sits under `if __name__ == "__main__":` or its equivalent (`if __FILE__ == $0`, `import.meta.main`, `require.main === module`).

Outside Python and Node, a port is a plain Rust library with one C entry point taking and returning JSON: no headers, bindings or extra tools, and any language that can call C can use it.

Intel oneAPI on Windows: run fast-anything from a shell where `setvars.bat` ran. SYCL kernels aren't ported; with `-fsycl`, only host functions are.

## Sponsors

fast-anything is free and open source. If it saves you time, sponsoring keeps it maintained.

<p>
  <a href="https://github.com/sponsors/EL4CTEO"><img src="https://img.shields.io/badge/Become_a_sponsor-%E2%9D%A4-db61a2?logo=githubsponsors&logoColor=white" alt="Become a sponsor"></a>
</p>

Company sponsors get a logo and a link here, nothing more: no paid recommendations.

## License

MIT
