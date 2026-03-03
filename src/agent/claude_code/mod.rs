pub mod adapter;
pub mod pod;
#[allow(dead_code)] // Pending removal in Step 6 (dead code cleanup)
pub mod progress;

pub use adapter::ClaudeCodeProvider;
