use serde::Serialize;

#[derive(Serialize)]
pub struct Category {
    pub id: i64,
    pub name: String,
    pub kind: String,
}

#[derive(Serialize)]
pub struct TransactionRecord {
    pub id: i64,
    pub kind: String,
    pub amount_cents: i64,
    pub occurred_on: String,
    pub note: Option<String>,
    pub category_name: Option<String>,
}

#[derive(Serialize)]
pub struct BudgetRecord {
    pub id: i64,
    pub category_id: i64,
    pub category_name: String,
    pub month: String,
    pub amount_cents: i64,
    pub spent_cents: i64,
}

#[derive(Serialize)]
pub struct ReportMonth {
    pub month: String,
    pub income_cents: i64,
    pub expense_cents: i64,
    pub net_cents: i64,
}

#[derive(Serialize)]
pub struct ReportCategory {
    pub category_name: String,
    pub expense_cents: i64,
}

#[derive(Serialize)]
pub struct DashboardBudget {
    pub category_name: String,
    pub budget_cents: i64,
    pub spent_cents: i64,
    pub remaining_cents: i64,
}
