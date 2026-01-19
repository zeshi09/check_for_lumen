#[macro_use]
extern crate rocket;

mod db;
mod models;

use std::path::PathBuf;

use chrono::Local;
use db::DbPool;
use models::{BudgetRecord, DashboardBudget, ReportCategory, ReportMonth, TransactionRecord};
use rocket::form::Form;
use rocket::fs::FileServer;
use rocket::response::Redirect;
use rocket::serde::Serialize;
use rocket::State;
use rocket_dyn_templates::Template;

#[derive(FromForm)]
struct CategoryForm {
    name: String,
    kind: String,
}

#[derive(FromForm)]
struct TransactionForm {
    kind: String,
    amount: String,
    category_id: Option<i64>,
    occurred_on: String,
    note: Option<String>,
}

#[derive(FromForm)]
struct BudgetForm {
    category_id: i64,
    month: String,
    amount: String,
}

#[derive(Serialize)]
struct TransactionView {
    id: i64,
    kind: String,
    amount: String,
    occurred_on: String,
    note: Option<String>,
    category_name: Option<String>,
}

#[derive(Serialize)]
struct BudgetView {
    id: i64,
    category_name: String,
    month: String,
    amount: String,
    spent: String,
    remaining: String,
    percent: i64,
}

#[derive(Serialize)]
struct DashboardBudgetView {
    category_name: String,
    budget: String,
    spent: String,
    remaining: String,
    percent: i64,
}

#[derive(Serialize)]
struct ReportMonthView {
    month: String,
    income: String,
    expense: String,
    net: String,
}

#[derive(Serialize)]
struct ReportCategoryView {
    category_name: String,
    expense: String,
}

fn format_money(cents: i64) -> String {
    let sign = if cents < 0 { "-" } else { "" };
    let abs = cents.abs();
    let whole = abs / 100;
    let frac = abs % 100;
    format!("{sign}{whole}.{frac:02}")
}

fn parse_amount_to_cents(input: &str) -> Option<i64> {
    let mut s = input.trim().to_string();
    if s.is_empty() {
        return None;
    }
    if s.starts_with('-') {
        return None;
    }
    s = s.replace(',', ".");
    let mut parts = s.split('.');
    let whole_str = parts.next()?;
    let frac_str = parts.next();
    if parts.next().is_some() {
        return None;
    }
    let whole: i64 = whole_str.parse().ok()?;
    let frac = match frac_str {
        None => 0,
        Some(frac) => {
            if frac.len() > 2 {
                return None;
            }
            let mut padded = frac.to_string();
            while padded.len() < 2 {
                padded.push('0');
            }
            padded.parse::<i64>().ok()?
        }
    };
    Some(whole * 100 + frac)
}

fn today_ymd() -> String {
    Local::now().date_naive().format("%Y-%m-%d").to_string()
}

fn current_month() -> String {
    Local::now().date_naive().format("%Y-%m").to_string()
}

fn available_months(conn: &rusqlite::Connection) -> Vec<String> {
    let mut months = db::list_months(conn, 24).unwrap_or_default();
    let budget_months = db::list_budget_months(conn, 24).unwrap_or_default();
    months.extend(budget_months);
    months.push(current_month());
    months.sort();
    months.dedup();
    months.reverse();
    months
}

#[get("/?<month>")]
fn dashboard(pool: &State<DbPool>, month: Option<String>) -> Template {
    let selected = month.unwrap_or_else(current_month);
    let conn = pool.get().expect("db connection");
    let (income_cents, expense_cents) =
        db::month_totals(&conn, &selected).unwrap_or((0, 0));
    let budgets = db::dashboard_budgets(&conn, &selected).unwrap_or_default();
    let budget_views = budgets
        .into_iter()
        .map(dashboard_budget_view)
        .collect::<Vec<_>>();
    let months = available_months(&conn);

    let context = serde_json::json!({
        "month": selected,
        "months": months,
        "income": format_money(income_cents),
        "expense": format_money(expense_cents),
        "net": format_money(income_cents - expense_cents),
        "budgets": budget_views,
    });
    Template::render("dashboard", &context)
}

#[get("/transactions?<month>")]
fn transactions(pool: &State<DbPool>, month: Option<String>) -> Template {
    let conn = pool.get().expect("db connection");
    let selected = month.unwrap_or_else(current_month);
    let records = db::list_transactions(&conn, Some(&selected)).unwrap_or_default();
    let categories = db::list_categories(&conn).unwrap_or_default();
    let views = records.into_iter().map(transaction_view).collect::<Vec<_>>();
    let months = available_months(&conn);

    let context = serde_json::json!({
        "month": selected,
        "months": months,
        "today": today_ymd(),
        "transactions": views,
        "categories": categories,
    });
    Template::render("transactions", &context)
}

#[post("/transactions", data = "<form>")]
fn add_transaction(pool: &State<DbPool>, form: Form<TransactionForm>) -> Result<Redirect, rocket::http::Status> {
    let form = form.into_inner();
    let amount_cents = parse_amount_to_cents(&form.amount)
        .ok_or(rocket::http::Status::BadRequest)?;
    let occurred_on = if form.occurred_on.trim().is_empty() {
        today_ymd()
    } else {
        form.occurred_on
    };

    let conn = pool.get().map_err(|_| rocket::http::Status::InternalServerError)?;
    db::insert_transaction(
        &conn,
        &form.kind,
        amount_cents,
        form.category_id,
        &occurred_on,
        form.note.as_deref(),
    )
    .map_err(|_| rocket::http::Status::InternalServerError)?;

    Ok(Redirect::to("/transactions"))
}

#[get("/categories")]
fn categories(pool: &State<DbPool>) -> Template {
    let conn = pool.get().expect("db connection");
    let list = db::list_categories(&conn).unwrap_or_default();
    let context = serde_json::json!({
        "categories": list,
    });
    Template::render("categories", &context)
}

#[post("/categories", data = "<form>")]
fn add_category(pool: &State<DbPool>, form: Form<CategoryForm>) -> Result<Redirect, rocket::http::Status> {
    let form = form.into_inner();
    if form.name.trim().is_empty() {
        return Err(rocket::http::Status::BadRequest);
    }
    let conn = pool.get().map_err(|_| rocket::http::Status::InternalServerError)?;
    db::insert_category(&conn, form.name.trim(), &form.kind)
        .map_err(|_| rocket::http::Status::InternalServerError)?;
    Ok(Redirect::to("/categories"))
}

#[get("/budgets?<month>")]
fn budgets(pool: &State<DbPool>, month: Option<String>) -> Template {
    let conn = pool.get().expect("db connection");
    let selected = month.unwrap_or_else(current_month);
    let list = db::list_budgets(&conn, &selected).unwrap_or_default();
    let categories = db::list_categories(&conn).unwrap_or_default();
    let views = list.into_iter().map(budget_view).collect::<Vec<_>>();
    let months = available_months(&conn);

    let context = serde_json::json!({
        "month": selected,
        "months": months,
        "budgets": views,
        "categories": categories,
    });
    Template::render("budgets", &context)
}

#[post("/budgets", data = "<form>")]
fn add_budget(pool: &State<DbPool>, form: Form<BudgetForm>) -> Result<Redirect, rocket::http::Status> {
    let form = form.into_inner();
    let amount_cents = parse_amount_to_cents(&form.amount)
        .ok_or(rocket::http::Status::BadRequest)?;
    let month = if form.month.trim().is_empty() {
        current_month()
    } else {
        form.month
    };

    let conn = pool.get().map_err(|_| rocket::http::Status::InternalServerError)?;
    db::insert_budget(&conn, form.category_id, &month, amount_cents)
        .map_err(|_| rocket::http::Status::InternalServerError)?;
    Ok(Redirect::to("/budgets"))
}

#[get("/reports?<month>")]
fn reports(pool: &State<DbPool>, month: Option<String>) -> Template {
    let conn = pool.get().expect("db connection");
    let selected = month.unwrap_or_else(current_month);
    let months = db::report_months(&conn, 12).unwrap_or_default();
    let categories = db::report_categories(&conn, &selected).unwrap_or_default();
    let month_options = available_months(&conn);

    let month_views = months
        .into_iter()
        .map(report_month_view)
        .collect::<Vec<_>>();
    let category_views = categories
        .into_iter()
        .map(report_category_view)
        .collect::<Vec<_>>();

    let context = serde_json::json!({
        "month": selected,
        "month_options": month_options,
        "months": month_views,
        "categories": category_views,
    });
    Template::render("reports", &context)
}

fn transaction_view(record: TransactionRecord) -> TransactionView {
    TransactionView {
        id: record.id,
        kind: record.kind,
        amount: format_money(record.amount_cents),
        occurred_on: record.occurred_on,
        note: record.note,
        category_name: record.category_name,
    }
}

fn budget_view(record: BudgetRecord) -> BudgetView {
    let remaining = record.amount_cents - record.spent_cents;
    let percent = if record.amount_cents == 0 {
        0
    } else {
        ((record.spent_cents as f64 / record.amount_cents as f64) * 100.0).round() as i64
    };
    BudgetView {
        id: record.id,
        category_name: record.category_name,
        month: record.month,
        amount: format_money(record.amount_cents),
        spent: format_money(record.spent_cents),
        remaining: format_money(remaining),
        percent,
    }
}

fn dashboard_budget_view(record: DashboardBudget) -> DashboardBudgetView {
    let percent = if record.budget_cents == 0 {
        0
    } else {
        ((record.spent_cents as f64 / record.budget_cents as f64) * 100.0).round() as i64
    };
    DashboardBudgetView {
        category_name: record.category_name,
        budget: format_money(record.budget_cents),
        spent: format_money(record.spent_cents),
        remaining: format_money(record.remaining_cents),
        percent,
    }
}

fn report_month_view(record: ReportMonth) -> ReportMonthView {
    ReportMonthView {
        month: record.month,
        income: format_money(record.income_cents),
        expense: format_money(record.expense_cents),
        net: format_money(record.net_cents),
    }
}

fn report_category_view(record: ReportCategory) -> ReportCategoryView {
    ReportCategoryView {
        category_name: record.category_name,
        expense: format_money(record.expense_cents),
    }
}

#[launch]
fn rocket() -> _ {
    let mut db_path = PathBuf::from("data");
    std::fs::create_dir_all(&db_path).expect("create data directory");
    db_path.push("lumen.sqlite");
    let pool = db::init_db(&db_path);

    rocket::build()
        .manage(pool)
        .mount(
            "/",
            routes![
                dashboard,
                transactions,
                add_transaction,
                categories,
                add_category,
                budgets,
                add_budget,
                reports
            ],
        )
        .mount("/static", FileServer::from("static"))
        .attach(Template::fairing())
}
