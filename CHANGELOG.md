# Changelog

## 0.1.0

- Profile, port, verify, enable, bench and report for Python (PyO3), Node.js/TypeScript (napi-rs) and Ruby (C ABI + JSON via Fiddle, no headers needed).
- Ports functions in the entry script too, when its main code is under a main guard.
- Ports the outermost hot loop, with the helpers it calls, so the app crosses into Rust once instead of per call.
- Verifies against up to 500 real recorded calls plus derived cases (shrunk inputs answered by the original).
- `go --agent claude|codex|<cmd>`: the whole loop in one command. `doctor` checks your toolchain.
- MCP server (`fast-anything mcp`), Claude Code plugin and agent skill.
- npm (`npm i -g @el4cteo/fast-anything`) and crates.io packages, prebuilt binaries for Windows, macOS and Linux.
