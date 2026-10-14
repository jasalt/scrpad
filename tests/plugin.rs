//! The Claude Code plugin in `plugin/`: its manifests, and its hook run as Claude Code runs it.

use std::path::Path;

fn root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn json(path: &str) -> serde_json::Value {
    serde_json::from_str(&std::fs::read_to_string(root().join(path)).unwrap()).unwrap_or_else(|e| panic!("{path}: {e}"))
}

#[test]
fn the_marketplace_lists_the_plugin_at_the_version_of_the_app() {
    let market = json(".claude-plugin/marketplace.json");
    let entry = &market["plugins"][0];
    assert_eq!(entry["name"], "scrpad");
    let plugin_dir = root().join(entry["source"].as_str().unwrap());
    let plugin = json(plugin_dir.join(".claude-plugin/plugin.json").strip_prefix(root()).unwrap().to_str().unwrap());
    assert_eq!(plugin["name"], "scrpad");
    assert_eq!(plugin["version"], env!("CARGO_PKG_VERSION"), "bump plugin/.claude-plugin/plugin.json with Cargo.toml");
    let hooks = json("plugin/hooks/hooks.json");
    let command = hooks["hooks"]["PreToolUse"][0]["hooks"][0]["command"].as_str().unwrap();
    assert!(command.contains("${CLAUDE_PLUGIN_ROOT}/hooks/handoff-files.sh"));
}

#[test]
fn the_skill_has_what_agents_need_to_load_it() {
    let skill = scrpad::cli::SKILL;
    let front = skill.strip_prefix("---\n").and_then(|s| s.split_once("\n---\n")).expect("YAML front matter").0;
    assert!(front.lines().any(|l| l == "name: scrpad"));
    let description = front.lines().find_map(|l| l.strip_prefix("description: ")).expect("a description");
    assert!(description.len() <= 1024, "the Agent Skills limit");
    for command in ["--append", "--read", "--run", "--write"] {
        assert!(skill.contains(&format!("scrpad {command}")), "{command}");
    }
}

/// Runs the hook on a tool call, as Claude Code does: the call as JSON on stdin; exit 2 blocks.
#[cfg(unix)]
fn hook(call: serde_json::Value) -> i32 {
    use std::io::Write;
    let mut child = std::process::Command::new("sh")
        .arg(root().join("plugin/hooks/handoff-files.sh"))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(call.to_string().as_bytes()).unwrap();
    child.wait().unwrap().code().unwrap()
}

#[cfg(unix)]
#[test]
fn the_hook_blocks_handoff_files_but_not_the_round_trip() {
    use serde_json::json;
    let bash = |command: &str| json!({ "tool_name": "Bash", "tool_input": { "command": command } });
    let read = |path: &str| json!({ "tool_name": "Read", "tool_input": { "file_path": path } });
    // Exit 1 would be jq missing (`mise install`), which Claude Code shows but does not block on.
    for blocked in [
        read("/p/app/.env"),
        read("/p/app/.env.local"),
        json!({ "tool_name": "Grep", "tool_input": { "pattern": "KEY", "path": ".env" } }),
        bash("cat .env"),
        bash("cd /p && grep KEY ./.env.production"),
        bash("scrpad --read .env"),
        bash("scrpad --write .env . && cat .env"),
        bash("scrpad --run .env -- cat .env"),
        // A title that runs code is not a title: what it runs is checked like the rest.
        bash("scrpad --append \"$(cat .env)\" -"),
        bash("scrpad --run \"$(cat .env)\" -- true"),
        bash("scrpad --write \"$(cat .env)\" /tmp"),
        bash("scrpad --append \"$(cp .env /tmp/x)\" -"),
        bash("scrpad --append $(cat<.env) -"),
        bash("cat<.env"),
        bash("scrpad --append x `cat .env`"),
        bash("scrpad --append \"`cat .env`\" -"),
        bash("scrpad --append \"\\$(cat .env)\" -"),
    ] {
        assert_eq!(hook(blocked.clone()), 2, "{blocked}");
    }
    for allowed in [
        read("/p/app/.env.example"),
        read("/p/app/src/env.rs"),
        bash("printf '%s\\n' 'API_KEY=' | scrpad --append .env -"),
        bash("scrpad --append \".env\" -"),
        bash("scrpad --write .env ."),
        bash("scrpad --run '.env' -- npm run dev"),
        // Single quotes run nothing.
        bash("scrpad --append '$(cat .env)' -"),
    ] {
        assert_eq!(hook(allowed.clone()), 0, "{allowed}");
    }
}
