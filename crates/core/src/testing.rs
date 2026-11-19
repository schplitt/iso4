//! Test and bench side door (feature `test-support`): picked internals for
//! other crates' tests and `benches/`. Never on in a build.

pub use crate::blob::{deserialize_value, serialize_value};
pub use crate::policy::{pick_victim, watermark_action, PassOutcome, PressureFacts, VictimFact};
pub use crate::snapshot::{boot_context, create_params};
#[path = "testval.rs"]
pub mod testval;
/// The default lazy V8 start; `Engine::start` refuses after it.
pub use crate::v8::init_platform;

use crate::engine::Engine;

/// An engine without `Engine::start`: V8 starts lazily via [`init_platform`].
pub fn engine(warm_budget_bytes: u64, admission_line_bytes: u64) -> Engine {
    Engine::new(warm_budget_bytes, admission_line_bytes)
}
