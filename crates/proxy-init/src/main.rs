//! Shell-free iptables init container for transparent proxy mesh.
//!
//! Replaces a shell script with a static Rust binary that executes iptables
//! rules directly. Combined with a distroless image that has no `/bin/sh`,
//! this eliminates shell-based attack vectors even if the init container's
//! `NET_ADMIN` capability were somehow exploited.

use std::env;
use std::process::Command;

fn main() {
    println!("[proxy-init] Setting up transparent mesh iptables rules...");

    let inbound_port = env_or("PROXY_INBOUND_PORT", "15006");
    let outbound_port = env_or("PROXY_OUTBOUND_PORT", "15001");
    let health_port = env_or("PROXY_HEALTH_PORT", "15020");
    let outbound_bind = env_or("PROXY_OUTBOUND_BIND", "127.0.0.6");

    // --- INBOUND: redirect external TCP to proxy inbound listener ---
    ipt(&["-t", "nat", "-N", "PLATFORM_INBOUND"]);
    ipt(&[
        "-t",
        "nat",
        "-A",
        "PLATFORM_INBOUND",
        "-p",
        "tcp",
        "--dport",
        &inbound_port,
        "-j",
        "RETURN",
    ]);
    ipt(&[
        "-t",
        "nat",
        "-A",
        "PLATFORM_INBOUND",
        "-p",
        "tcp",
        "--dport",
        &outbound_port,
        "-j",
        "RETURN",
    ]);
    ipt(&[
        "-t",
        "nat",
        "-A",
        "PLATFORM_INBOUND",
        "-p",
        "tcp",
        "--dport",
        &health_port,
        "-j",
        "RETURN",
    ]);
    ipt(&[
        "-t",
        "nat",
        "-A",
        "PLATFORM_INBOUND",
        "-p",
        "tcp",
        "-j",
        "REDIRECT",
        "--to-ports",
        &inbound_port,
    ]);
    ipt(&[
        "-t",
        "nat",
        "-A",
        "PREROUTING",
        "-p",
        "tcp",
        "-j",
        "PLATFORM_INBOUND",
    ]);

    // --- OUTBOUND: redirect app-originated TCP to proxy outbound listener ---
    let outbound_cidr = format!("{outbound_bind}/32");
    ipt(&["-t", "nat", "-N", "PLATFORM_OUTPUT"]);
    ipt(&[
        "-t",
        "nat",
        "-A",
        "PLATFORM_OUTPUT",
        "-s",
        &outbound_cidr,
        "-j",
        "RETURN",
    ]);
    ipt(&[
        "-t",
        "nat",
        "-A",
        "PLATFORM_OUTPUT",
        "-o",
        "lo",
        "-d",
        "127.0.0.1/32",
        "-j",
        "RETURN",
    ]);
    ipt(&[
        "-t",
        "nat",
        "-A",
        "PLATFORM_OUTPUT",
        "-p",
        "tcp",
        "--dport",
        "53",
        "-j",
        "RETURN",
    ]);
    ipt(&[
        "-t",
        "nat",
        "-A",
        "PLATFORM_OUTPUT",
        "-p",
        "tcp",
        "-j",
        "REDIRECT",
        "--to-ports",
        &outbound_port,
    ]);
    ipt(&[
        "-t",
        "nat",
        "-A",
        "OUTPUT",
        "-p",
        "tcp",
        "-j",
        "PLATFORM_OUTPUT",
    ]);

    println!(
        "[proxy-init] Mesh routing established (inbound:{inbound_port} outbound:{outbound_port})"
    );
}

fn env_or(key: &str, default: &str) -> String {
    env::var(key).unwrap_or_else(|_| default.to_string())
}

fn ipt(args: &[&str]) {
    let status = Command::new("iptables")
        .args(args)
        .status()
        .unwrap_or_else(|e| panic!("[proxy-init] failed to execute iptables: {e}"));

    assert!(
        status.success(),
        "[proxy-init] iptables {args:?} exited with {status}"
    );
}
