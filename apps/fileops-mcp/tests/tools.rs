//! End-to-end: the tools as an MCP client sees them.

mod sandbox;

use sandbox::Tree;

#[test]
fn every_tool_is_advertised_with_its_batch_parameters() {
    let tree = Tree::new();
    let mut server = tree.server();
    let tools = server.tools();
    let names: Vec<&str> = tools["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(
        sorted,
        vec![
            "extract", "find", "grep", "inspect", "outline", "read", "survey"
        ]
    );

    let read = tools["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "read")
        .unwrap();
    let schema = read["inputSchema"].to_string();
    for parameter in [
        "specs",
        "lines",
        "head",
        "tail",
        "from",
        "to",
        "max_lines",
        "max_bytes",
    ] {
        assert!(
            schema.contains(parameter),
            "`{parameter}` missing from {schema}"
        );
    }
    assert!(
        server.instructions.contains("Prefer these tools over"),
        "the server tells the client what it is for: {}",
        server.instructions
    );
}

#[test]
fn one_read_call_replaces_a_chain_of_shell_reads() {
    let tree = Tree::new();
    tree.write(".specify/extensions.yml", "extensions: []\n")
        .write(".specify/feature.json", "{\"id\": \"010\"}\n")
        .numbered("specs/010-x/spec.md", 300)
        .write(
            "specs/010-x/tasks.md",
            "- [x] done\n- [ ] open one\nnoise\n- [ ] open two\n",
        );

    let mut server = tree.server();
    let result = server.call(
        "read",
        serde_json::json!({
            "specs": [
                {"path": ".specify/extensions.yml"},
                {"path": ".specify/feature.json"},
                {"path": "specs/010-x/spec.md", "lines": "1-2"},
                {"path": "specs/010-x/tasks.md", "grep": "^- \\[ \\]"}
            ]
        }),
    );

    assert!(!result.is_error(), "{}", result.text());
    assert_eq!(
        result.text(),
        "\
#1 .specify/extensions.yml 1/1
1: extensions: []
#2 .specify/feature.json 1/1
1: {\"id\": \"010\"}
#3 specs/010-x/spec.md 1-2/300
1: line 1
2: line 2
#4 specs/010-x/tasks.md 2,4/4
2: - [ ] open one
4: - [ ] open two
[4 files, 6 lines, 212 chars]
"
    );
    assert_eq!(result.structured()["files"].as_array().unwrap().len(), 4);
    assert_eq!(result.structured()["lines_shown"], 6);
}

#[test]
fn a_bad_request_comes_back_as_a_readable_tool_error() {
    let tree = Tree::new();
    tree.numbered("a.txt", 5);
    let mut server = tree.server();

    let result = server.call("read", serde_json::json!({"specs": []}));
    assert!(result.is_error());
    assert!(
        result.text().contains("at least one spec"),
        "{}",
        result.text()
    );

    let result = server.call(
        "read",
        serde_json::json!({"specs": [{"path": "a.txt", "lines": "40-12"}]}),
    );
    assert!(result.is_error());
    assert!(
        result.text().contains("not a line range"),
        "{}",
        result.text()
    );
}

#[test]
fn grep_locates_before_reading() {
    let tree = Tree::new();
    tree.write("src/a.rs", "fn one() {}\nlet x = 1;\n")
        .write("src/b.rs", "fn two() {}\n")
        .write("target/generated.rs", "fn three() {}\n")
        .write(".gitignore", "target\n");

    let mut server = tree.server();
    let files = server.call(
        "grep",
        serde_json::json!({"patterns": ["^fn "], "mode": "files"}),
    );
    assert_eq!(
        files.text(),
        "src/a.rs\nsrc/b.rs\n[2 matches in 2 files, 2 searched]\n"
    );

    let lines = server.call(
        "grep",
        serde_json::json!({"patterns": ["^fn "], "glob": ["a.rs"]}),
    );
    assert_eq!(
        lines.text(),
        "src/a.rs (1)\n1: fn one() {}\n[1 match in 1 file, 1 searched]\n"
    );
    assert_eq!(lines.structured()["matches"], 1);
}

#[test]
fn find_and_inspect_answer_what_is_here_and_how_big_it_is() {
    let tree = Tree::new();
    tree.write("src/lib.rs", "a\n")
        .write("src/main.rs", "b\n")
        .numbered("docs/guide.md", 40);

    let mut server = tree.server();
    let listing = server.call("find", serde_json::json!({"depth": 2, "kind": "file"}));
    assert_eq!(
        listing.text(),
        "docs/\n  guide.md\nsrc/\n  lib.rs main.rs\n[3 of 3 entries (0 dirs)]\n"
    );

    let described = server.call(
        "inspect",
        serde_json::json!({"paths": ["docs/*.md", "nope"]}),
    );
    let text = described.text();
    assert!(text.contains("docs/guide.md 311B 40L"), "{text}");
    assert!(text.contains("nope (missing)"), "{text}");
    assert_eq!(described.structured()["missing"], 1);
}

#[test]
fn every_response_is_bounded_and_says_what_it_left_out() {
    let tree = Tree::new();
    tree.numbered("huge.txt", 5000);
    let mut server = tree.server();

    let result = server.call(
        "read",
        serde_json::json!({"specs": [{"path": "huge.txt"}], "max_bytes": 200}),
    );
    let text = result.text();
    assert!(
        text.len() < 400,
        "a budget of 200 held: {} bytes",
        text.len()
    );
    assert!(text.contains("lines cut"), "{text}");
    assert_eq!(result.structured()["truncated"], 1);
}

#[test]
fn an_outline_locates_what_a_following_read_should_name() {
    let tree = Tree::new();
    tree.write(
        "docs/guide.md",
        "# Guide\n\nprose\n\n## Install\n\nmore prose\n\n## Usage\n\nstill more\n",
    )
    .write(
        "src/lib.rs",
        "pub struct Spec {\n    pub path: String,\n}\n\npub fn run() {}\n",
    );
    let mut server = tree.server();

    let outline = server.call(
        "outline",
        serde_json::json!({"paths": ["docs/guide.md", "src/lib.rs"]}),
    );
    assert!(!outline.is_error(), "{}", outline.text());
    assert_eq!(
        outline.text(),
        "docs/guide.md (3)\n\
         1: # Guide\n\
         5: ## Install\n\
         9: ## Usage\n\
         src/lib.rs (2)\n\
         1: pub struct Spec\n\
         5: pub fn run()\n\
         [5 symbols in 2 files, 2 read]\n"
    );

    // The outline said where "## Install" starts; the read names that section without
    // anyone counting lines.
    let read = server.call(
        "read",
        serde_json::json!({"specs": [{"path": "docs/guide.md", "from": "^## Install", "to": "^## "}]}),
    );
    assert!(!read.is_error(), "{}", read.text());
    assert_eq!(
        read.text(),
        "#1 docs/guide.md 5-9/11\n\
         5: ## Install\n\
         6: \n\
         7: more prose\n\
         8: \n\
         9: ## Usage\n\
         [1 file, 5 lines, 72 chars]\n"
    );
}

#[test]
fn extract_reads_one_value_instead_of_a_whole_config() {
    let tree = Tree::new();
    tree.write(
        "Cargo.toml",
        "[package]\nname = \"thing\"\nversion = \"0.3.1\"\n\n[dependencies]\nserde = \"1\"\n",
    )
    .write(
        "ci.yml",
        "jobs:\n  build:\n    steps:\n      - run: make check\n      - run: make install\n",
    );
    let mut server = tree.server();

    let values = server.call(
        "extract",
        serde_json::json!({"specs": [
            {"path": "Cargo.toml", "query": "package.version"},
            {"path": "ci.yml", "query": "jobs.build.steps[].run"}
        ]}),
    );
    assert!(!values.is_error(), "{}", values.text());
    assert_eq!(
        values.text(),
        "#1 Cargo.toml package.version (1)\n\
         package.version = 0.3.1\n\
         #2 ci.yml jobs.build.steps[].run (2)\n\
         jobs.build.steps[0].run = make check\n\
         jobs.build.steps[1].run = make install\n\
         [2 documents, 3 values]\n"
    );
}

#[test]
fn a_survey_sizes_up_a_tree_before_anything_is_read() {
    let tree = Tree::new();
    tree.write("src/lib.rs", "a\nb\nc\n")
        .write("src/main.rs", "d\n")
        .write("README.md", "# Thing\n");
    let mut server = tree.server();

    let shape = server.call("survey", serde_json::json!({"top": 2}));
    assert!(!shape.is_error(), "{}", shape.text());
    assert_eq!(
        shape.text(),
        "md 1 file 8B 1L\n\
         rs 2 files 8B 4L\n\
         largest:\n\
         README.md 8B 1L\n\
         src/lib.rs 6B 3L\n\
         [3 files in 2 dirs, 16B, 5L]\n"
    );
}

#[test]
fn every_response_carries_its_text_in_both_halves() {
    // Clients disagree about which half of a response they display: Claude Code shows
    // `structuredContent` and drops the text content, other hosts do the reverse. So the
    // rendered text ships in both, and the duplication is deliberate. It was once removed
    // to save bytes on the wire — the result was a server whose every tool answered with
    // metadata and no content. The wire bytes are nobody's context; the text is.
    let tree = Tree::new();
    tree.write("src/lib.rs", "pub fn run() {}\n")
        .write("Cargo.toml", "[package]\nname = \"thing\"\n");
    let mut server = tree.server();

    for (tool, arguments) in [
        (
            "read",
            serde_json::json!({"specs": [{"path": "src/lib.rs"}]}),
        ),
        ("grep", serde_json::json!({"patterns": ["fn"]})),
        ("find", serde_json::json!({"roots": ["."]})),
        ("inspect", serde_json::json!({"paths": ["src/lib.rs"]})),
        ("outline", serde_json::json!({"paths": ["src/lib.rs"]})),
        (
            "extract",
            serde_json::json!({"specs": [{"path": "Cargo.toml"}]}),
        ),
        ("survey", serde_json::json!({})),
    ] {
        let result = server.call(tool, arguments);
        assert!(!result.is_error(), "{tool}: {}", result.text());
        assert!(!result.text().is_empty(), "{tool} rendered nothing");
        let structured = result.structured();
        let carried = structured
            .get("text")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_else(|| {
                panic!("{tool} left its text out of the structured copy: {structured}")
            });
        assert_eq!(
            carried,
            result.text(),
            "{tool}: the two halves of the response disagree"
        );
    }
}
