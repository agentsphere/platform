//! Platform proxy — process wrapper with mTLS, log capture, and OTLP export.
//!
//! The proxy binary wraps a child process as PID 1, captures stdout/stderr,
//! provides mTLS termination/origination, generates traces for every request,
//! and exports all telemetry to the platform via OTLP protobuf.

#[allow(dead_code)]
pub mod child;
#[allow(dead_code)]
pub mod config;
#[allow(dead_code)]
pub mod gateway;
#[allow(dead_code)]
pub mod health;
#[allow(dead_code)]
pub mod inbound;
#[allow(dead_code)]
pub mod logs;
#[allow(dead_code)]
pub mod metrics;
#[allow(dead_code)]
pub mod otlp;
#[allow(dead_code)]
pub mod outbound;
#[allow(dead_code)]
pub mod scraper;
#[allow(dead_code)]
pub mod tcp_proxy;
#[allow(dead_code)]
pub mod tls;
#[allow(dead_code)]
pub mod traces;
