//! Workflow Execution Runtime (Layer 7) - V2 Architecture
//!
//! This module provides a high-level runtime for executing distributed workflows
//! built on the generic Raft infrastructure.

pub mod error;
pub mod event;
pub mod state_machine;
pub mod runtime;
pub mod registry;
pub mod ownership;
pub mod context;
pub mod replicated_var;

pub use error::{WorkflowError, ReplicatedVarError, WorkflowStatus};
pub use event::WorkflowEvent;
pub use state_machine::{WorkflowCommand, WorkflowStateMachine};
pub use runtime::{WorkflowRuntime, WorkflowSubscription};
pub use registry::WorkflowRegistry;
pub use ownership::WorkflowOwnershipMap;
pub use context::{WorkflowContext, WorkflowRun, TypedWorkflowRun};
pub use replicated_var::ReplicatedVar;
