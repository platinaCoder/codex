use crate::ThreadId;
use crate::protocol::ThreadSource;

/// Canonical persisted source prefix for an AI Lab SIDE conversation.
pub const AI_LAB_SIDE_THREAD_SOURCE_PREFIX: &str = "ai_lab_side";

/// Server-owned conversation role derived from persisted identity and lifecycle metadata.
///
/// `RestrictedSide` deliberately preserves the restrictive security posture for malformed or
/// conflicting SIDE-like identities. Callers must never reinterpret it as `Standard`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConversationRole {
    Standard,
    Side { parent_main_id: ThreadId },
    RestrictedSide { reason: InvalidSideRoleReason },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InvalidSideRoleReason {
    MissingParent,
    MalformedParent,
    SelfParent,
    ConflictingParent,
}

impl ConversationRole {
    pub fn is_side_like(self) -> bool {
        !matches!(self, Self::Standard)
    }

    pub fn requires_strict_read_only(self) -> bool {
        self.is_side_like()
    }

    pub fn paired_main_id(self) -> Option<ThreadId> {
        match self {
            Self::Side { parent_main_id } => Some(parent_main_id),
            Self::Standard | Self::RestrictedSide { .. } => None,
        }
    }
}

/// Resolve a role from durable thread identity and server-owned lifecycle metadata.
///
/// Only the exact `ThreadSource::Feature("ai_lab_side:<UUID>")` encoding grants the paired SIDE
/// role. Any SIDE-like spelling that is missing, malformed, self-referential, or conflicts with a
/// known control parent fails closed as `RestrictedSide`.
pub fn resolve_conversation_role(
    thread_source: Option<&ThreadSource>,
    thread_id: Option<ThreadId>,
    control_parent_id: Option<ThreadId>,
) -> ConversationRole {
    let feature = thread_source.and_then(|source| match source {
        ThreadSource::Feature(feature) => Some(feature.as_str()),
        _ => None,
    });
    resolve_conversation_role_feature(feature, thread_id, control_parent_id)
}

/// Resolve a role from the feature portion of any transport's thread-source type.
///
/// App-server protocol types intentionally mirror the core protocol without sharing the same Rust
/// enum. Keeping the parser here prevents those transport adapters from growing an independent
/// SIDE identity parser.
pub fn resolve_conversation_role_feature(
    feature: Option<&str>,
    thread_id: Option<ThreadId>,
    control_parent_id: Option<ThreadId>,
) -> ConversationRole {
    let Some(source) = feature else {
        return ConversationRole::Standard;
    };
    let Some(suffix) = source.strip_prefix(AI_LAB_SIDE_THREAD_SOURCE_PREFIX) else {
        return ConversationRole::Standard;
    };
    let Some(parent_text) = suffix.strip_prefix(':') else {
        return ConversationRole::RestrictedSide {
            reason: InvalidSideRoleReason::MissingParent,
        };
    };
    if parent_text.is_empty() {
        return ConversationRole::RestrictedSide {
            reason: InvalidSideRoleReason::MissingParent,
        };
    }
    let Ok(parent_main_id) = ThreadId::from_string(parent_text) else {
        return ConversationRole::RestrictedSide {
            reason: InvalidSideRoleReason::MalformedParent,
        };
    };
    if thread_id == Some(parent_main_id) {
        return ConversationRole::RestrictedSide {
            reason: InvalidSideRoleReason::SelfParent,
        };
    }
    if control_parent_id.is_some_and(|control_parent| control_parent != parent_main_id) {
        return ConversationRole::RestrictedSide {
            reason: InvalidSideRoleReason::ConflictingParent,
        };
    }
    ConversationRole::Side { parent_main_id }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(value: u128) -> ThreadId {
        ThreadId::from_u128(value)
    }

    #[test]
    fn exact_side_source_resolves_with_matching_parent() {
        let parent = id(1);
        let source = ThreadSource::Feature(format!("{AI_LAB_SIDE_THREAD_SOURCE_PREFIX}:{parent}"));

        assert_eq!(
            resolve_conversation_role(Some(&source), Some(id(2)), Some(parent)),
            ConversationRole::Side {
                parent_main_id: parent
            }
        );
    }

    #[test]
    fn ordinary_feature_is_standard() {
        let source = ThreadSource::Feature("another_feature".to_string());
        assert_eq!(
            resolve_conversation_role(Some(&source), Some(id(2)), None),
            ConversationRole::Standard
        );
    }

    #[test]
    fn malformed_side_like_sources_fail_closed() {
        for source in [
            "ai_lab_side",
            "ai_lab_side:",
            "ai_lab_side:not-a-uuid",
            "ai_lab_side_extra",
        ] {
            assert!(matches!(
                resolve_conversation_role(
                    Some(&ThreadSource::Feature(source.to_string())),
                    Some(id(2)),
                    None,
                ),
                ConversationRole::RestrictedSide { .. }
            ));
        }
    }

    #[test]
    fn self_and_conflicting_parents_fail_closed() {
        let parent = id(1);
        let source = ThreadSource::Feature(format!("{AI_LAB_SIDE_THREAD_SOURCE_PREFIX}:{parent}"));
        assert_eq!(
            resolve_conversation_role(Some(&source), Some(parent), Some(parent)),
            ConversationRole::RestrictedSide {
                reason: InvalidSideRoleReason::SelfParent
            }
        );
        assert_eq!(
            resolve_conversation_role(Some(&source), Some(id(2)), Some(id(3))),
            ConversationRole::RestrictedSide {
                reason: InvalidSideRoleReason::ConflictingParent
            }
        );
    }
}
