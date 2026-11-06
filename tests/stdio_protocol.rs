use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Write},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
};

struct Server {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Server {
    fn start() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_librelink-mcp-rs"))
            .arg("serve")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("start MCP server");
        let stdin = child.stdin.take().expect("stdin pipe");
        let stdout = BufReader::new(child.stdout.take().expect("stdout pipe"));
        Self {
            child,
            stdin,
            stdout,
        }
    }

    fn send(&mut self, request: Value) {
        writeln!(self.stdin, "{request}").expect("write MCP request");
        self.stdin.flush().expect("flush MCP request");
    }

    fn receive(&mut self) -> Value {
        let mut line = String::new();
        self.stdout.read_line(&mut line).expect("read MCP response");
        assert!(!line.is_empty(), "MCP server closed before responding");
        serde_json::from_str(&line).expect("parse MCP response")
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn stdio_lists_eight_tools_and_rejects_secret_input() {
    let mut server = Server::start();
    server.send(json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": { "name": "synthetic-test", "version": "0.0.0" }
        }
    }));
    let initialized = server.receive();
    assert_eq!(initialized["id"], 1);
    assert!(initialized.get("result").is_some());
    server.send(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));

    server.send(json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }));
    let list = server.receive();
    let mut names: Vec<&str> = list["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .map(|tool| tool["name"].as_str().expect("tool name"))
        .collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "configure_credentials",
            "configure_ranges",
            "get_current_glucose",
            "get_glucose_history",
            "get_glucose_stats",
            "get_glucose_trends",
            "get_sensor_info",
            "validate_connection",
        ]
    );

    server.send(json!({
        "jsonrpc": "2.0", "id": 3, "method": "tools/call",
        "params": { "name": "configure_credentials", "arguments": {} }
    }));
    let setup = server.receive();
    assert_eq!(
        setup["result"]["structuredContent"]["credential_input_supported"],
        false
    );
    assert_eq!(setup["result"]["isError"], false);

    let synthetic_secret = "synthetic-password-must-not-echo";
    server.send(json!({
        "jsonrpc": "2.0", "id": 4, "method": "tools/call",
        "params": {
            "name": "configure_credentials",
            "arguments": { "password": synthetic_secret }
        }
    }));
    let rejected = server.receive();
    assert_eq!(rejected["id"], 4);
    assert!(!rejected.to_string().contains(synthetic_secret));
    assert!(rejected.get("error").is_some() || rejected["result"]["isError"] == true);
}
