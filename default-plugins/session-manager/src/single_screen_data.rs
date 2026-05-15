use std::time::Duration;

#[derive(Debug, Clone)]
pub enum DeleteTarget {
    Active(String),
    Resurrectable(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum UnifiedSearchResult {
    ActiveSession {
        score: i64,
        indices: Vec<usize>,
        session_name: String,
        connected_users: usize,
        tab_count: usize,
        pane_count: usize,
        is_current_session: bool,
        creation_time: Duration,
    },
    ResurrectableSession {
        score: i64,
        indices: Vec<usize>,
        session_name: String,
        ctime: Duration,
    },
}

impl UnifiedSearchResult {
    pub fn session_name(&self) -> &str {
        match self {
            UnifiedSearchResult::ActiveSession { session_name, .. } => session_name.as_str(),
            UnifiedSearchResult::ResurrectableSession { session_name, .. } => session_name.as_str(),
        }
    }

    pub fn score(&self) -> i64 {
        match self {
            UnifiedSearchResult::ActiveSession { score, .. } => *score,
            UnifiedSearchResult::ResurrectableSession { score, .. } => *score,
        }
    }

    pub fn as_delete_target(&self) -> DeleteTarget {
        match self {
            UnifiedSearchResult::ActiveSession { session_name, .. } => {
                DeleteTarget::Active(session_name.clone())
            },
            UnifiedSearchResult::ResurrectableSession { session_name, .. } => {
                DeleteTarget::Resurrectable(session_name.clone())
            },
        }
    }

    pub fn cmp_by_type_then_recency(&self, other: &Self) -> std::cmp::Ordering {
        match (self, other) {
            (
                UnifiedSearchResult::ActiveSession {
                    creation_time: ct_a,
                    ..
                },
                UnifiedSearchResult::ActiveSession {
                    creation_time: ct_b,
                    ..
                },
            ) => ct_a.cmp(ct_b),
            (
                UnifiedSearchResult::ResurrectableSession { ctime: ct_a, .. },
                UnifiedSearchResult::ResurrectableSession { ctime: ct_b, .. },
            ) => ct_a.cmp(ct_b),
            (
                UnifiedSearchResult::ActiveSession { .. },
                UnifiedSearchResult::ResurrectableSession { .. },
            ) => std::cmp::Ordering::Less,
            (
                UnifiedSearchResult::ResurrectableSession { .. },
                UnifiedSearchResult::ActiveSession { .. },
            ) => std::cmp::Ordering::Greater,
        }
    }
}
