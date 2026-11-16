//! Every TOML example in `docs/` must parse and validate against the real
//! config schema, so the reference cannot drift from the code unnoticed.
//!
//! Snippets are completed into a whole config before parsing:
//! - a block with `[canvas]` is taken as is (a scene is added if it has none);
//! - a block of `widget = { ... }` lines becomes one scene with a zone each;
//! - anything else (a `[sources.x]`, `[mqtt]`, `[[output]]` or `[[scene]]`
//!   section) gets a canvas and, if needed, a scene around it.
//!
//! Files the examples name (fonts, images, videos) need not exist: errors
//! about loading files are ignored, everything else fails the test.

use std::path::{Path, PathBuf};

use paneld::config::Config;

const CANVAS: &str = "[canvas]\nwidth = 172\nheight = 172\n";
const SCENE: &str =
    "\n[[scene]]\nname = \"docs\"\n  [[scene.zone]]\n  rect = { x = 0, y = 0, w = 172, h = 172 }\n  widget = { type = \"clock\" }\n";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// `(line number, body)` of every ```toml block.
fn toml_blocks(md: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut cur: Option<(usize, String)> = None;
    for (i, line) in md.lines().enumerate() {
        let t = line.trim_start();
        match &mut cur {
            None if t.starts_with("```toml") => cur = Some((i + 1, String::new())),
            Some(_) if t.starts_with("```") => out.push(cur.take().unwrap()),
            Some((_, body)) => {
                body.push_str(line);
                body.push('\n');
            }
            None => {}
        }
    }
    out
}

fn complete(block: &str) -> String {
    let has_scene = block.contains("[[scene]]");
    let widgets: Vec<&str> = block
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with("widget ="))
        .collect();
    if block.contains("[canvas]") {
        let mut s = block.to_owned();
        if !has_scene {
            s.push_str(SCENE);
        }
        return s;
    }
    if !has_scene
        && !widgets.is_empty()
        && widgets.len()
            == block
                .lines()
                .filter(|l| !l.trim().is_empty() && !l.trim().starts_with('#'))
                .count()
    {
        let mut s = format!("{CANVAS}\n[[scene]]\nname = \"docs\"\n");
        for w in widgets {
            s.push_str("  [[scene.zone]]\n  rect = { x = 0, y = 0, w = 172, h = 172 }\n  ");
            s.push_str(w);
            s.push('\n');
        }
        return s;
    }
    let mut s = format!("{CANVAS}\n{block}");
    if !has_scene {
        s.push_str(SCENE);
    }
    s
}

/// Errors that only say a file the example names is missing.
fn is_file_error(e: &str) -> bool {
    [
        "cannot load",
        "does not exist",
        "No such file",
        "cannot read",
        "not found",
        "is not a directory",
    ]
    .iter()
    .any(|m| e.contains(m))
}

#[test]
fn every_toml_example_in_docs_is_valid() {
    let docs = repo_root().join("docs");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&docs)
        .expect("docs/ exists")
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "md"))
        .collect();
    files.sort();
    assert!(!files.is_empty());

    let mut checked = 0;
    let mut failures = Vec::new();
    for file in &files {
        let md = std::fs::read_to_string(file).unwrap();
        for (line, block) in toml_blocks(&md) {
            checked += 1;
            let text = complete(&block);
            let at = format!("{}:{line}", file.file_name().unwrap().to_string_lossy());
            match Config::parse_in(&text, &repo_root().join("config")) {
                Ok(_) => {}
                Err(e) => {
                    let msg = format!("{e:#}");
                    let real: Vec<&str> = msg
                        .lines()
                        .map(str::trim)
                        .filter(|l| l.starts_with("- "))
                        .filter(|l| !is_file_error(l))
                        .collect();
                    // A parse error (not a validation list) has no "- " lines.
                    if !msg.contains("invalid config:") || !real.is_empty() {
                        failures.push(format!("{at}:\n{msg}\n--- as parsed:\n{text}"));
                    }
                }
            }
        }
    }
    assert!(checked >= 10, "only {checked} examples found");
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}
