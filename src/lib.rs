pub mod audit;
pub mod config;
pub mod error;
pub mod store;
pub mod validation;

// Phase 02 — Identity, Auth & RBAC
pub mod api;
pub mod auth;
pub mod rbac;

// Phase 03 — Git Server
pub mod git;

// Phase 05 — Build Engine
pub mod pipeline;

// Module stubs — populated in later phases
pub mod deployer {}
pub mod agent {}
pub mod observe {}

// Phase 09 — Secrets Engine & Notifications
pub mod notify;
pub mod secrets;
