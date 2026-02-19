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

// Phase 07 — Agent Orchestration
pub mod agent;

// Module stubs — populated in later phases
pub mod deployer {}
pub mod observe {}
pub mod secrets {}
pub mod notify {}
