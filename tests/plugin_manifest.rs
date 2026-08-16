// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Compliance checks for the shipped plugin and its repo-local marketplace.
//!
//! These mirror the rules the official Codex plugin validator enforces, so a
//! manifest that drifts out of spec fails here — on every platform, on every
//! commit — instead of at install time in someone else's terminal.
//!
//! They also pin two things no external validator can know: the skills stay
//! provider-neutral so Claude Code (or any agent reading the open skill format)
//! can use them unchanged, and the Weave rules that exist to protect a
//! repository — the raw Git prohibition and host-only canonical commits — are
//! actually present in the skills that need them.

use serde_json::Value;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn plugin_root() -> PathBuf {
    repo_root().join(".agents").join("plugins").join("weave")
}

fn read_json(path: &Path) -> Value {
    let text = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    assert!(
        !text.starts_with('\u{feff}'),
        "{} must not start with a byte order mark; the manifest parser rejects it",
        path.display()
    );
    serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("{} is not valid JSON: {e}", path.display()))
}

fn non_empty_string(value: &Value, field: &str) -> String {
    let text = value
        .get(field)
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("`{field}` must be a non-empty string"));
    assert!(
        !text.trim().is_empty(),
        "`{field}` must be a non-empty string"
    );
    text.to_string()
}

fn assert_https(value: &Value, field: &str) {
    if let Some(raw) = value.get(field) {
        let url = raw
            .as_str()
            .unwrap_or_else(|| panic!("`{field}` must be a string"));
        assert!(
            url.starts_with("https://") && url.len() > "https://".len(),
            "`{field}` must be an absolute https:// URL, got {url}"
        );
    }
}

fn keys(value: &Value) -> BTreeSet<String> {
    value
        .as_object()
        .expect("expected a JSON object")
        .keys()
        .cloned()
        .collect()
}

fn assert_no_unknown_keys(value: &Value, allowed: &[&str], what: &str) {
    let allowed: BTreeSet<String> = allowed.iter().map(|s| (*s).to_string()).collect();
    let unknown: Vec<String> = keys(value).difference(&allowed).cloned().collect();
    assert!(
        unknown.is_empty(),
        "{what} has fields the plugin validator rejects: {unknown:?}"
    );
}

fn skill_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(plugin_root().join("skills"))
        .expect("skills directory")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    dirs.sort();
    assert!(!dirs.is_empty(), "the plugin must ship at least one skill");
    dirs
}

/// Split a `SKILL.md` into its frontmatter and body, enforcing the exact
/// delimiter rules the validator applies.
fn split_skill(path: &Path) -> (String, String) {
    let text = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    assert!(
        text.starts_with("---\n"),
        "{} must start with YAML frontmatter",
        path.display()
    );
    let end = text[4..]
        .find("\n---")
        .unwrap_or_else(|| panic!("{} frontmatter is not closed", path.display()));
    (text[4..4 + end].to_string(), text[4 + end..].to_string())
}

/// Lowercase a skill body and collapse every whitespace run to one space, so a
/// phrase assertion does not depend on where Markdown happened to wrap a line.
fn normalized(text: &str) -> String {
    text.to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Read one `key: value` pair out of simple frontmatter.
fn frontmatter_value(frontmatter: &str, key: &str) -> Option<String> {
    for line in frontmatter.lines() {
        if let Some(rest) = line.strip_prefix(&format!("{key}:")) {
            return Some(rest.trim().trim_matches('"').to_string());
        }
    }
    None
}

// ---------------------------------------------------------------------------
// plugin.json
// ---------------------------------------------------------------------------

#[test]
fn plugin_manifest_matches_the_codex_plugin_contract() {
    let root = plugin_root();
    let manifest = read_json(&root.join(".codex-plugin").join("plugin.json"));

    // The outer folder name and the manifest name must agree.
    assert_eq!(
        root.file_name().unwrap().to_str().unwrap(),
        non_empty_string(&manifest, "name"),
        "the plugin folder name must match plugin.json `name`"
    );

    assert_no_unknown_keys(
        &manifest,
        &[
            "id",
            "name",
            "version",
            "description",
            "skills",
            "apps",
            "mcpServers",
            "interface",
            "author",
            "homepage",
            "repository",
            "license",
            "keywords",
        ],
        "plugin.json",
    );

    let name = non_empty_string(&manifest, "name");
    assert!(
        name.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            && name.len() <= 64,
        "plugin name must be lowercase kebab-case and at most 64 characters: {name}"
    );

    // Strict semver.
    let version = non_empty_string(&manifest, "version");
    let core: Vec<&str> = version
        .split(['-', '+'])
        .next()
        .unwrap()
        .split('.')
        .collect();
    assert_eq!(core.len(), 3, "`version` must be strict semver: {version}");
    for part in core {
        assert!(
            !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()),
            "`version` must be strict semver: {version}"
        );
    }

    non_empty_string(&manifest, "description");

    // `author` is required, and its shape is checked.
    let author = manifest.get("author").expect("`author` is required");
    assert_no_unknown_keys(author, &["name", "email", "url"], "plugin.json author");
    non_empty_string(author, "name");
    assert_https(author, "url");

    // `skills` must resolve to the `skills` directory, as a path, not a list.
    let skills = manifest
        .get("skills")
        .and_then(|v| v.as_str())
        .expect("`skills` must be a path string");
    assert_eq!(
        skills.trim_start_matches("./").trim_end_matches('/'),
        "skills",
        "`skills` must resolve to `skills`"
    );
    assert!(root.join("skills").is_dir());

    // Weave ships no MCP server and no app, by design.
    assert!(
        manifest.get("mcpServers").is_none(),
        "Weave must not declare an MCP server"
    );
    assert!(manifest.get("apps").is_none());
    assert!(
        !root.join(".mcp.json").exists(),
        "Weave must not ship .mcp.json"
    );
    assert!(!root.join(".app.json").exists());
    // `hooks` is not accepted by plugin validation.
    assert!(manifest.get("hooks").is_none());

    // ---- interface, which is required ----
    let interface = manifest.get("interface").expect("`interface` is required");
    assert_no_unknown_keys(
        interface,
        &[
            "displayName",
            "shortDescription",
            "longDescription",
            "developerName",
            "category",
            "capabilities",
            "websiteURL",
            "privacyPolicyURL",
            "termsOfServiceURL",
            "brandColor",
            "composerIcon",
            "logo",
            "logoDark",
            "screenshots",
            "defaultPrompt",
            "default_prompt",
        ],
        "plugin.json interface",
    );
    for field in [
        "displayName",
        "shortDescription",
        "longDescription",
        "developerName",
        "category",
    ] {
        non_empty_string(interface, field);
    }

    let capabilities = interface
        .get("capabilities")
        .and_then(|v| v.as_array())
        .expect("`interface.capabilities` must be an array");
    assert!(!capabilities.is_empty());
    for capability in capabilities {
        assert!(capability.as_str().is_some_and(|s| !s.trim().is_empty()));
    }

    let prompts = interface
        .get("defaultPrompt")
        .or_else(|| interface.get("default_prompt"))
        .expect("`interface.defaultPrompt` is required");
    let prompts = prompts
        .as_array()
        .expect("`interface.defaultPrompt` must be an array");
    assert!(
        !prompts.is_empty() && prompts.len() <= 3,
        "at most three starter prompts are allowed"
    );
    for prompt in prompts {
        let text = prompt.as_str().expect("prompts must be strings");
        assert!(!text.trim().is_empty());
        assert!(
            text.chars().count() <= 128,
            "starter prompts are limited to 128 characters: {text}"
        );
    }

    for field in ["websiteURL", "privacyPolicyURL", "termsOfServiceURL"] {
        assert_https(interface, field);
    }
    assert_https(&manifest, "homepage");
    assert_https(&manifest, "repository");

    if let Some(color) = interface.get("brandColor") {
        let color = color.as_str().expect("`brandColor` must be a string");
        assert!(
            color.len() == 7
                && color.starts_with('#')
                && color[1..].chars().all(|c| c.is_ascii_hexdigit()),
            "`interface.brandColor` must use #RRGGBB, got {color}"
        );
    }

    // Asset paths are only valid when the file actually ships.
    for field in ["composerIcon", "logo", "logoDark"] {
        if let Some(asset) = interface.get(field).and_then(|v| v.as_str()) {
            assert_asset_exists(&root, asset, field);
        }
    }
    if let Some(shots) = interface.get("screenshots").and_then(|v| v.as_array()) {
        for (index, shot) in shots.iter().enumerate() {
            let asset = shot.as_str().expect("screenshots must be strings");
            assert_asset_exists(&root, asset, &format!("screenshots[{index}]"));
        }
    }

    // Placeholders must never reach a published manifest.
    let raw = std::fs::read_to_string(root.join(".codex-plugin").join("plugin.json")).unwrap();
    assert!(
        !raw.contains("[TODO:"),
        "plugin.json still contains a placeholder"
    );
}

fn assert_asset_exists(root: &Path, asset: &str, field: &str) {
    let relative = asset.trim_start_matches("./");
    assert!(
        !asset.starts_with('/')
            && !relative
                .split('/')
                .any(|part| part == ".." || part.is_empty()),
        "`{field}` must be a relative path inside the plugin: {asset}"
    );
    assert!(
        root.join(relative).is_file(),
        "`{field}` points at a file that does not ship: {asset}"
    );
}

// ---------------------------------------------------------------------------
// SKILL.md
// ---------------------------------------------------------------------------

#[test]
fn every_skill_matches_the_open_skill_format() {
    for dir in skill_dirs() {
        let folder = dir.file_name().unwrap().to_str().unwrap().to_string();
        let skill_md = dir.join("SKILL.md");
        assert!(skill_md.is_file(), "skill `{folder}` is missing SKILL.md");

        let (frontmatter, body) = split_skill(&skill_md);

        let name = frontmatter_value(&frontmatter, "name")
            .unwrap_or_else(|| panic!("skill `{folder}` frontmatter needs `name`"));
        assert_eq!(name, folder, "skill `name` must match its folder name");
        assert!(
            name.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
                && name.len() <= 64,
            "skill name must be lowercase kebab-case, at most 64 characters: {name}"
        );

        let description = frontmatter_value(&frontmatter, "description")
            .unwrap_or_else(|| panic!("skill `{folder}` frontmatter needs `description`"));
        assert!(
            !description.trim().is_empty(),
            "skill `{folder}` description must be non-empty"
        );
        // The description is the trigger: it must say when to use the skill,
        // because the body is only loaded after it fires.
        assert!(
            description.contains("Use ") || description.contains("use when"),
            "skill `{folder}` description must say when to use it: {description}"
        );
        assert!(
            description.chars().count() <= 1024,
            "skill `{folder}` description is too long for portable use"
        );

        // Must stay implicitly invocable.
        for key in ["disable-model-invocation", "disable_model_invocation"] {
            if let Some(value) = frontmatter_value(&frontmatter, key) {
                assert_eq!(value, "false", "skill `{folder}` must stay model-invocable");
            }
        }

        assert!(
            !body.trim().is_empty(),
            "skill `{folder}` has no instructions"
        );

        // The optional Codex sidecar, when present, must itself be valid.
        let sidecar = dir.join("agents").join("openai.yaml");
        if sidecar.is_file() {
            let text = std::fs::read_to_string(&sidecar).unwrap();
            for field in ["display_name", "short_description"] {
                let line = format!("  {field}:");
                assert!(
                    text.lines()
                        .any(|l| l.starts_with(&line) && l.len() > line.len()),
                    "skill `{folder}` agents/openai.yaml needs a non-empty `interface.{field}`"
                );
            }
            assert!(
                text.starts_with("interface:"),
                "skill `{folder}` agents/openai.yaml must define `interface`"
            );
        }
    }
}

#[test]
fn skills_stay_provider_neutral() {
    // The skills must be reusable by any agent that reads the open skill
    // format, so nothing in a SKILL.md may name a vendor or assume one runtime.
    // Vendor-specific packaging lives in plugin.json and agents/openai.yaml.
    const VENDORS: &[&str] = &[
        "Codex",
        "OpenAI",
        "ChatGPT",
        "Claude",
        "Anthropic",
        "Cursor",
        "Copilot",
        "Gemini",
    ];
    for dir in skill_dirs() {
        let folder = dir.file_name().unwrap().to_str().unwrap().to_string();
        let text = std::fs::read_to_string(dir.join("SKILL.md")).unwrap();
        for vendor in VENDORS {
            assert!(
                !text.contains(vendor),
                "skill `{folder}` names `{vendor}`; skills must stay provider-neutral"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// The rules that protect the repository
// ---------------------------------------------------------------------------

#[test]
fn every_skill_states_the_raw_git_prohibition_explicitly() {
    // Specification section 165: the rule must be explicit, not implied, and it
    // must apply to host and participant agents alike.
    const FORBIDDEN: &[&str] = &[
        "git add",
        "git commit",
        "git pull",
        "git push",
        "git merge",
        "git rebase",
        "git cherry-pick",
        "git reset",
        "git checkout",
        "git switch",
        "git stash",
    ];
    for dir in skill_dirs() {
        let folder = dir.file_name().unwrap().to_str().unwrap().to_string();
        let text = normalized(&std::fs::read_to_string(dir.join("SKILL.md")).unwrap());
        for command in FORBIDDEN {
            assert!(
                text.contains(command),
                "skill `{folder}` must name `{command}` in the raw Git prohibition"
            );
        }
        assert!(
            text.contains("host agents and participant agents alike"),
            "skill `{folder}` must state that the Git rule applies to hosts too"
        );
        for allowed in ["git status", "git diff", "git log", "git show"] {
            assert!(
                text.contains(allowed),
                "skill `{folder}` should still permit `{allowed}`"
            );
        }
    }
}

#[test]
fn the_host_only_commit_rule_is_visible_where_it_matters() {
    // Specification section 169: a non-host may request a Weave commit, but the
    // participant machine does not create the canonical Git commit.
    for skill in ["weave-collaboration", "weave-commit"] {
        let text =
            std::fs::read_to_string(plugin_root().join("skills").join(skill).join("SKILL.md"))
                .unwrap_or_else(|e| panic!("cannot read skill `{skill}`: {e}"));
        let lower = normalized(&text);
        assert!(
            lower.contains("only the host") || lower.contains("host coordinator"),
            "skill `{skill}` must say that only the host builds the canonical commit"
        );
        assert!(
            lower.contains("canonical"),
            "skill `{skill}` must name the commit the host builds as canonical"
        );
        for duty in ["branch", "push"] {
            assert!(
                lower.contains(duty),
                "skill `{skill}` must describe the host's `{duty}` responsibility"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// marketplace.json
// ---------------------------------------------------------------------------

#[test]
fn the_repo_local_marketplace_resolves_to_the_shipped_plugin() {
    let marketplace_dir = repo_root().join(".agents").join("plugins");
    let marketplace = read_json(&marketplace_dir.join("marketplace.json"));

    assert_no_unknown_keys(
        &marketplace,
        &["name", "interface", "plugins"],
        "marketplace.json",
    );
    non_empty_string(&marketplace, "name");
    if let Some(interface) = marketplace.get("interface") {
        assert_no_unknown_keys(interface, &["displayName"], "marketplace.json interface");
        non_empty_string(interface, "displayName");
    }

    let plugins = marketplace
        .get("plugins")
        .and_then(|v| v.as_array())
        .expect("`plugins` must be an array");
    assert!(!plugins.is_empty(), "the marketplace lists no plugins");

    for entry in plugins {
        assert_no_unknown_keys(
            entry,
            &["name", "source", "policy", "category"],
            "marketplace plugin entry",
        );
        let name = non_empty_string(entry, "name");
        non_empty_string(entry, "category");

        let source = entry.get("source").expect("`source` is required");
        assert_no_unknown_keys(source, &["source", "path"], "marketplace plugin source");
        assert_eq!(
            source.get("source").and_then(|v| v.as_str()),
            Some("local"),
            "the repo-local marketplace must use a local source"
        );
        let relative = non_empty_string(source, "path");
        let resolved = marketplace_dir.join(relative.trim_start_matches("./"));
        assert!(
            resolved.join(".codex-plugin").join("plugin.json").is_file(),
            "`source.path` does not resolve to a plugin: {}",
            resolved.display()
        );

        // The entry name, the folder and the manifest must all agree.
        let manifest = read_json(&resolved.join(".codex-plugin").join("plugin.json"));
        assert_eq!(non_empty_string(&manifest, "name"), name);
        assert_eq!(resolved.file_name().unwrap().to_str().unwrap(), name);

        let policy = entry.get("policy").expect("`policy` is required");
        assert_no_unknown_keys(
            policy,
            &["installation", "authentication", "products"],
            "marketplace plugin policy",
        );
        let installation = non_empty_string(policy, "installation");
        assert!(
            ["NOT_AVAILABLE", "AVAILABLE", "INSTALLED_BY_DEFAULT"].contains(&installation.as_str()),
            "unknown installation policy: {installation}"
        );
        let authentication = non_empty_string(policy, "authentication");
        assert!(
            ["ON_INSTALL", "ON_USE"].contains(&authentication.as_str()),
            "unknown authentication policy: {authentication}"
        );
    }
}
