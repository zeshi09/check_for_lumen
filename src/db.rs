use std::path::Path;

use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::{params, Connection, Result};

use crate::models::{
    BudgetRecord, Category, DashboardBudget, ReportCategory, ReportMonth, TransactionRecord,
};

pub type DbPool = Pool<SqliteConnectionManager>;

pub fn init_db(path: &Path) -> DbPool {
    let manager = SqliteConnectionManager::file(path);
    let pool = Pool::new(manager).expect("db pool");
    {
        let conn = pool.get().expect("db connection");
        run_migrations(&conn).expect("db migrations");
    }
    pool
}

fn run_migrations(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        PRAGMA foreign_keys = ON;

        CREATE TABLE IF NOT EXISTS categories (
            id INTEGER PRIMARY KEY,
            name TEXT NOT NULL,
            kind TEXT NOT NULL CHECK(kind IN ('income', 'expense'))
        );

        CREATE TABLE IF NOT EXISTS transactions (
            id INTEGER PRIMARY KEY,
            kind TEXT NOT NULL CHECK(kind IN ('income', 'expense')),
            amount_cents INTEGER NOT NULL,
            category_id INTEGER,
            occurred_on TEXT NOT NULL,
            note TEXT,
            FOREIGN KEY(category_id) REFERENCES categories(id)
        );

        CREATE TABLE IF NOT EXISTS budgets (
            id INTEGER PRIMARY KEY,
            category_id INTEGER NOT NULL,
            month TEXT NOT NULL,
            amount_cents INTEGER NOT NULL,
            FOREIGN KEY(category_id) REFERENCES categories(id)
        );
        ",
    )?;
    Ok(())
}

pub fn list_categories(conn: &Connection) -> Result<Vec<Category>> {
    let mut stmt = conn.prepare(
        "
        SELECT id, name, kind
        FROM categories
        ORDER BY kind, name
        ",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(Category {
            id: row.get(0)?,
            name: row.get(1)?,
            kind: row.get(2)?,
        })
    })?;

    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

pub fn insert_category(conn: &Connection, name: &str, kind: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO categories (name, kind) VALUES (?1, ?2)",
        params![name, kind],
    )?;
    Ok(())
}

pub fn list_transactions(conn: &Connection) -> Result<Vec<TransactionRecord>> {
    let mut stmt = conn.prepare(
        "
        SELECT t.id, t.kind, t.amount_cents, t.occurred_on, t.note, c.name
        FROM transactions t
        LEFT JOIN categories c ON t.category_id = c.id
        ORDER BY t.occurred_on DESC, t.id DESC
        LIMIT 100
        ",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(TransactionRecord {
            id: row.get(0)?,
            kind: row.get(1)?,
            amount_cents: row.get(2)?,
            occurred_on: row.get(3)?,
            note: row.get(4)?,
            category_name: row.get(5)?,
        })
    })?;

    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

pub fn insert_transaction(
    conn: &Connection,
    kind: &str,
    amount_cents: i64,
    category_id: Option<i64>,
    occurred_on: &str,
    note: Option<&str>,
) -> Result<()> {
    conn.execute(
        "
        INSERT INTO transactions (kind, amount_cents, category_id, occurred_on, note)
        VALUES (?1, ?2, ?3, ?4, ?5)
        ",
        params![kind, amount_cents, category_id, occurred_on, note],
    )?;
    Ok(())
}

pub fn list_budgets(conn: &Connection, month: &str) -> Result<Vec<BudgetRecord>> {
    let like_month = format!("{}-%", month);
    let mut stmt = conn.prepare(
        "
        SELECT b.id, b.category_id, c.name, b.month, b.amount_cents,
               COALESCE(SUM(t.amount_cents), 0) AS spent_cents
        FROM budgets b
        JOIN categories c ON b.category_id = c.id
        LEFT JOIN transactions t
            ON t.category_id = b.category_id
           AND t.kind = 'expense'
           AND t.occurred_on LIKE ?1
        WHERE b.month = ?2
        GROUP BY b.id, b.category_id, c.name, b.month, b.amount_cents
        ORDER BY c.name
        ",
    )?;
    let rows = stmt.query_map(params![like_month, month], |row| {
        Ok(BudgetRecord {
            id: row.get(0)?,
            category_id: row.get(1)?,
            category_name: row.get(2)?,
            month: row.get(3)?,
            amount_cents: row.get(4)?,
            spent_cents: row.get(5)?,
        })
    })?;

    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

pub fn insert_budget(
    conn: &Connection,
    category_id: i64,
    month: &str,
    amount_cents: i64,
) -> Result<()> {
    conn.execute(
        "INSERT INTO budgets (category_id, month, amount_cents) VALUES (?1, ?2, ?3)",
        params![category_id, month, amount_cents],
    )?;
    Ok(())
}

pub fn month_totals(conn: &Connection, month: &str) -> Result<(i64, i64)> {
    let like_month = format!("{}-%", month);
    let income: i64 = conn.query_row(
        "
        SELECT COALESCE(SUM(amount_cents), 0)
        FROM transactions
        WHERE kind = 'income' AND occurred_on LIKE ?1
        ",
        params![like_month],
        |row| row.get(0),
    )?;
    let expense: i64 = conn.query_row(
        "
        SELECT COALESCE(SUM(amount_cents), 0)
        FROM transactions
        WHERE kind = 'expense' AND occurred_on LIKE ?1
        ",
        params![like_month],
        |row| row.get(0),
    )?;
    Ok((income, expense))
}

pub fn dashboard_budgets(conn: &Connection, month: &str) -> Result<Vec<DashboardBudget>> {
    let like_month = format!("{}-%", month);
    let mut stmt = conn.prepare(
        "
        SELECT c.name, b.amount_cents,
               COALESCE(SUM(t.amount_cents), 0) AS spent_cents
        FROM budgets b
        JOIN categories c ON b.category_id = c.id
        LEFT JOIN transactions t
            ON t.category_id = b.category_id
           AND t.kind = 'expense'
           AND t.occurred_on LIKE ?1
        WHERE b.month = ?2
        GROUP BY c.name, b.amount_cents
        ORDER BY c.name
        ",
    )?;
    let rows = stmt.query_map(params![like_month, month], |row| {
        let budget_cents: i64 = row.get(1)?;
        let spent_cents: i64 = row.get(2)?;
        Ok(DashboardBudget {
            category_name: row.get(0)?,
            budget_cents,
            spent_cents,
            remaining_cents: budget_cents - spent_cents,
        })
    })?;

    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

pub fn report_months(conn: &Connection, limit: i64) -> Result<Vec<ReportMonth>> {
    let mut stmt = conn.prepare(
        "
        SELECT substr(occurred_on, 1, 7) AS month,
               COALESCE(SUM(CASE WHEN kind = 'income' THEN amount_cents END), 0) AS income_cents,
               COALESCE(SUM(CASE WHEN kind = 'expense' THEN amount_cents END), 0) AS expense_cents
        FROM transactions
        GROUP BY month
        ORDER BY month DESC
        LIMIT ?1
        ",
    )?;
    let rows = stmt.query_map(params![limit], |row| {
        let income: i64 = row.get(1)?;
        let expense: i64 = row.get(2)?;
        Ok(ReportMonth {
            month: row.get(0)?,
            income_cents: income,
            expense_cents: expense,
            net_cents: income - expense,
        })
    })?;

    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

pub fn report_categories(conn: &Connection, month: &str) -> Result<Vec<ReportCategory>> {
    let like_month = format!("{}-%", month);
    let mut stmt = conn.prepare(
        "
        SELECT c.name, COALESCE(SUM(t.amount_cents), 0) AS expense_cents
        FROM transactions t
        JOIN categories c ON t.category_id = c.id
        WHERE t.kind = 'expense' AND t.occurred_on LIKE ?1
        GROUP BY c.name
        ORDER BY expense_cents DESC
        ",
    )?;
    let rows = stmt.query_map(params![like_month], |row| {
        Ok(ReportCategory {
            category_name: row.get(0)?,
            expense_cents: row.get(1)?,
        })
    })?;

    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}
