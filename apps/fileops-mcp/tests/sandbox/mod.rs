//! A disposable file tree and a live server speaking MCP over its stdio.
//!
//! These tests drive the built binary, so they cover the adapter and the protocol surface
//! an agent actually sees: tool names, parameter names, the rendered text and the
//! structured copy beside it. Nothing here touches anything outside its own tempdir.
//!
//! Each test binary compiles this module separately and uses only part of it, so unused
//! helpers here are expected rather than dead.
#![allow(dead_code)]

use std::{
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
};

pub struct Tree(tempfile::TempDir);

impl Tree {
    pub fn new() -> Self {
        Self(tempfile::tempdir().expect("tempdir"))
    }

    pub fn path(&self) -> &Path {
        self.0.path()
    }

    pub fn write(&self, name: &str, body: &str) -> &Self {
        let path = self.path().join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
        self
    }

    /// A file of `lines` numbered lines, for slicing.
    pub fn numbered(&self, name: &str, lines: usize) -> &Self {
        let body: String = (1..=lines).map(|n| format!("line {n}\n")).collect();
        self.write(name, &body)
    }

    /// Start the server with this tree as its working directory, handshake included.
    pub fn server(&self) -> Server {
        let mut child = Command::new(env!("CARGO_BIN_EXE_fileops-mcp"))
            .current_dir(self.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn fileops-mcp");

        let stdout = BufReader::new(child.stdout.take().unwrap());
        let mut server = Server {
            child,
            stdout,
            next_id: 1,
            cwd: self.path().to_path_buf(),
            instructions: String::new(),
        };
        let info = server.request(
            "initialize",
            serde_json::json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "fileops-test", "version": "0"}
            }),
        );
        server.instructions = info["instructions"].as_str().unwrap_or_default().to_owned();
        server.notify("notifications/initialized");
        server
    }
}

pub struct Server {
    child: Child,
    stdout: BufReader<std::process::ChildStdout>,
    next_id: u64,
    cwd: PathBuf,
    pub instructions: String,
}

impl Server {
    fn request(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(serde_json::json!({
            "jsonrpc": "2.0", "id": id, "method": method, "params": params
        }));
        let mut line = String::new();
        self.stdout.read_line(&mut line).expect("read response");
        let response: serde_json::Value =
            serde_json::from_str(&line).unwrap_or_else(|e| panic!("bad response `{line}`: {e}"));
        response["result"].clone()
    }

    fn notify(&mut self, method: &str) {
        self.send(serde_json::json!({"jsonrpc": "2.0", "method": method}));
    }

    fn send(&mut self, value: serde_json::Value) {
        let stdin = self.child.stdin.as_mut().unwrap();
        writeln!(stdin, "{value}").unwrap();
        stdin.flush().unwrap();
    }

    pub fn tools(&mut self) -> serde_json::Value {
        self.request("tools/list", serde_json::json!({}))
    }

    /// Call a tool against the tree.
    pub fn call(&mut self, tool: &str, arguments: serde_json::Value) -> ToolResult {
        ToolResult(self.request(
            "tools/call",
            serde_json::json!({"name": tool, "arguments": arguments}),
        ))
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub struct ToolResult(serde_json::Value);

impl ToolResult {
    pub fn is_error(&self) -> bool {
        self.0["isError"].as_bool().unwrap_or(false)
    }

    pub fn text(&self) -> String {
        self.0["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    }

    pub fn structured(&self) -> &serde_json::Value {
        &self.0["structuredContent"]
    }
}
