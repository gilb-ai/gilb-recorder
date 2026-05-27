//! Per-app `WalkBudget` tiers. Persisted state + adaptive throttling land in
//! Phase 2; Phase 0 provides only the enum so the schema column has a type.

use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetTier {
    /// Few walks/min — text editors, terminals, simple apps.
    Light,
    /// Default, moderately active app.
    #[default]
    Moderate,
    /// Heavy AX tree (Office, IDEs).
    Heavy,
    /// Pathological — Discord, Slack, web apps. Skip most walks.
    Critical,
}

impl BudgetTier {
    pub fn walks_per_min(self) -> u32 {
        match self {
            BudgetTier::Light => 120,
            BudgetTier::Moderate => 60,
            BudgetTier::Heavy => 20,
            BudgetTier::Critical => 5,
        }
    }
}
