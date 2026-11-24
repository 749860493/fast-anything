// The agent plugin manifests are bumped by hand next to Cargo.toml; keep them in step.

use serde_json::Value;

fn read(path: &str) -> Value {
    let full = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(path);
    let text = std::fs::read_to_string(&full).unwrap_or_else(|e| panic!("{path}: {e}"));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{path}: {e}"))
}

#[test]
fn plugin_versions_match_the_crate() {
    for path in [".claude-plugin/plugin.json", ".codex-plugin/plugin.json"] {
        assert_eq!(read(path)["version"], env!("CARGO_PKG_VERSION"), "{path}");
    }
}

#[test]
fn marketplaces_list_the_plugin() {
    let name = read(".codex-plugin/plugin.json")["name"].clone();
    assert_eq!(read(".claude-plugin/plugin.json")["name"], name);
    for path in [".agents/plugins/marketplace.json", ".claude-plugin/marketplace.json"] {
        let plugins = read(path)["plugins"].as_array().cloned().unwrap_or_default();
        assert!(plugins.iter().any(|p| p["name"] == name), "{path}");
    }
}
