use std::sync::Arc;

pub const MOCK_DRAFT_AUTHORIZATION_CONTRACT_VERSION: u16 = 1;
pub const MOCK_DRAFT_CREATE_TOOL: &str = "create_mock_transfer_draft";
pub const MOCK_DRAFT_CREATE_ACTION: &str = "CREATE_MOCK_DRAFT";
pub const MOCK_DRAFT_TOOL_SCHEMA_VERSION: u32 = 1;
pub const MOCK_DRAFT_LIFECYCLE_CONTRACT_VERSION: u16 = 2;
pub const MOCK_DRAFT_MODIFY_TOOL: &str = "modify_mock_transfer_draft";
pub const MOCK_DRAFT_MODIFY_ACTION: &str = "MODIFY_MOCK_DRAFT";
pub const MOCK_DRAFT_CANCEL_TOOL: &str = "cancel_mock_transfer_draft";
pub const MOCK_DRAFT_CANCEL_ACTION: &str = "CANCEL_MOCK_DRAFT";

/// Closed mock-storage whitelist. No bank submission action can be represented.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MockDraftAction {
    Create,
    Modify,
    Cancel,
}

impl MockDraftAction {
    pub fn tool_name(self) -> &'static str {
        match self {
            Self::Create => MOCK_DRAFT_CREATE_TOOL,
            Self::Modify => MOCK_DRAFT_MODIFY_TOOL,
            Self::Cancel => MOCK_DRAFT_CANCEL_TOOL,
        }
    }

    pub fn action(self) -> &'static str {
        match self {
            Self::Create => MOCK_DRAFT_CREATE_ACTION,
            Self::Modify => MOCK_DRAFT_MODIFY_ACTION,
            Self::Cancel => MOCK_DRAFT_CANCEL_ACTION,
        }
    }

    pub fn contract_version(self) -> u16 {
        match self {
            Self::Create => MOCK_DRAFT_AUTHORIZATION_CONTRACT_VERSION,
            Self::Modify | Self::Cancel => MOCK_DRAFT_LIFECYCLE_CONTRACT_VERSION,
        }
    }

    pub fn for_tool(tool: &str) -> Option<Self> {
        [Self::Create, Self::Modify, Self::Cancel]
            .into_iter()
            .find(|action| action.tool_name() == tool)
    }
}

/// Process-local object capability, never JSON or a production ExecutionPermit.
/// Construction alone grants nothing: the trusted authority must register this
/// exact instance and verify all bindings before Runtime and Tool use.
#[derive(Clone)]
pub struct MockDraftPermit(Arc<()>);

impl MockDraftPermit {
    pub fn unregistered() -> Self {
        Self(Arc::new(()))
    }

    pub fn same_instance(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl std::fmt::Debug for MockDraftPermit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MockDraftPermit(<opaque>)")
    }
}
