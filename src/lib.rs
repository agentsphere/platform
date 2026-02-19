pub mod audit;
pub mod config;
pub mod error;
pub mod store;

// Phase 02 — Identity, Auth & RBAC
pub mod api;
pub mod auth;
pub mod rbac;

// Phase 03 — Git Server
pub mod git;

// Module stubs — populated in later phases
pub mod pipeline {}
pub mod deployer {}
pub mod agent {}
pub mod observe {}
pub mod secrets {}
pub mod notify {}
