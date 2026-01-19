#[macro_use]
extern crate rocket;

mod db;
mod models;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use chrono::Local;
use db::DbPool;
use models::{BudgetRecord, DashboardBudget, ReportCategory, ReportMonth, TransactionRecord, User};
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier};
use password_hash::SaltString;
use rand_core::OsRng;
use rusqlite::params;
use rocket::form::Form;
use rocket::fs::{FileServer, TempFile};
use rocket::http::{Cookie, CookieJar, SameSite};
use rocket::response::Redirect;
use rocket::serde::Serialize;
use rocket::State;
use rocket_dyn_templates::Template;
use uuid::Uuid;

const MAX_SESSIONS: i64 = 5;

#[derive(FromForm)]
struct CategoryForm {
    name: String,
    kind: String,
}

#[derive(FromForm)]
struct TransactionForm<'r> {
    kind: String,
    amount: String,
    category_id: Option<i64>,
    occurred_on: String,
    note: Option<String>,
    receipt: Option<TempFile<'r>>,
}

#[derive(FromForm)]
struct BudgetForm {
    category_id: i64,
    month: String,
    amount: String,
}

#[derive(FromForm)]
struct LoginForm {
    username: String,
    password: String,
}

#[derive(FromForm)]
struct SetupForm {
    username: String,
    password: String,
    confirm_password: String,
}

#[derive(FromForm)]
struct ChangePasswordForm {
    current_password: String,
    new_password: String,
    confirm_password: String,
}

#[derive(Serialize)]
struct TransactionView {
    id: i64,
    kind: String,
    amount: String,
    occurred_on: String,
    note: Option<String>,
    category_name: Option<String>,
    receipt_url: Option<String>,
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

fn selected_month(month: Option<String>) -> String {
    month
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(current_month)
}

fn is_receipt_category(name: &str) -> bool {
    name.trim().to_lowercase() == "жкх"
}

fn receipts_dir() -> PathBuf {
    let mut dir = PathBuf::from("data");
    dir.push("receipts");
    dir
}

fn allowed_extension(name: &str) -> Option<String> {
    let ext = Path::new(name).extension()?.to_str()?.to_lowercase();
    match ext.as_str() {
        "jpg" | "jpeg" | "png" | "webp" | "heic" => Some(ext),
        _ => None,
    }
}

async fn persist_receipt(
    receipt: Option<TempFile<'_>>,
    category_name: Option<&str>,
    kind: &str,
) -> Result<Option<String>, rocket::http::Status> {
    let Some(mut receipt) = receipt else {
        return Ok(None);
    };
    let Some(category_name) = category_name else {
        return Ok(None);
    };
    if kind != "expense" || !is_receipt_category(category_name) {
        return Ok(None);
    }

    let ext = receipt
        .name()
        .and_then(allowed_extension)
        .unwrap_or_else(|| "jpg".to_string());
    let filename = format!("receipt-{}.{}", Local::now().timestamp_millis(), ext);
    let dir = receipts_dir();
    std::fs::create_dir_all(&dir).map_err(|_| rocket::http::Status::InternalServerError)?;
    let path = dir.join(&filename);
    receipt
        .persist_to(&path)
        .await
        .map_err(|_| rocket::http::Status::InternalServerError)?;
    Ok(Some(filename))
}

fn available_months(conn: &rusqlite::Connection) -> Vec<String> {
    let mut set = BTreeSet::new();
    for month in db::list_months(conn, 24).unwrap_or_default() {
        set.insert(month);
    }
    for month in db::list_budget_months(conn, 24).unwrap_or_default() {
        set.insert(month);
    }
    set.insert(current_month());
    set.into_iter().rev().collect()
}

fn hash_password(password: &str) -> Result<String, rocket::http::Status> {
    let salt = SaltString::generate(&mut OsRng);
    let argon2 = Argon2::default();
    let hash = argon2
        .hash_password(password.as_bytes(), &salt)
        .map_err(|_| rocket::http::Status::InternalServerError)?;
    Ok(hash.to_string())
}

fn verify_password(hash: &str, password: &str) -> bool {
    let parsed = match PasswordHash::new(hash) {
        Ok(parsed) => parsed,
        Err(_) => return false,
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

fn require_user(pool: &State<DbPool>, cookies: &CookieJar<'_>) -> Result<User, Redirect> {
    let conn = pool.get().map_err(|_| Redirect::to("/login"))?;
    if !db::has_users(&conn).unwrap_or(false) {
        return Err(Redirect::to("/setup"));
    }
    if let Some(cookie) = cookies.get("session") {
        if let Ok(Some(user)) = db::user_by_session(&conn, cookie.value()) {
            return Ok(user);
        }
    }
    Err(Redirect::to("/login"))
}

fn current_user(pool: &State<DbPool>, cookies: &CookieJar<'_>) -> Option<User> {
    let conn = pool.get().ok()?;
    let token = cookies.get("session")?.value().to_string();
    db::user_by_session(&conn, &token).ok().flatten()
}

fn render_login(error: Option<&str>) -> Template {
    Template::render(
        "login",
        serde_json::json!({
            "error": error,
        }),
    )
}

fn render_setup(error: Option<&str>) -> Template {
    Template::render(
        "setup",
        serde_json::json!({
            "error": error,
        }),
    )
}

fn render_settings(username: &str, sessions: i64, error: Option<&str>, notice: Option<&str>) -> Template {
    Template::render(
        "settings",
        serde_json::json!({
            "username": username,
            "active_sessions": sessions,
            "error": error,
            "notice": notice,
        }),
    )
}

#[get("/setup")]
fn setup(pool: &State<DbPool>, cookies: &CookieJar<'_>) -> Result<Template, Redirect> {
    let conn = pool.get().map_err(|_| Redirect::to("/login"))?;
    if db::has_users(&conn).unwrap_or(false) {
        if current_user(pool, cookies).is_some() {
            return Err(Redirect::to("/"));
        }
        return Err(Redirect::to("/login"));
    }
    Ok(render_setup(None))
}

#[post("/setup", data = "<form>")]
fn setup_post(
    pool: &State<DbPool>,
    cookies: &CookieJar<'_>,
    form: Form<SetupForm>,
) -> Result<Redirect, Template> {
    let conn = pool.get().map_err(|_| render_setup(Some("Ошибка подключения к базе")))?;
    if db::has_users(&conn).unwrap_or(false) {
        return Ok(Redirect::to("/login"));
    }

    let form = form.into_inner();
    let username = form.username.trim();
    if username.is_empty() {
        return Err(render_setup(Some("Введите логин")));
    }
    if form.password.len() < 6 {
        return Err(render_setup(Some("Пароль должен быть не короче 6 символов")));
    }
    if form.password != form.confirm_password {
        return Err(render_setup(Some("Пароли не совпадают")));
    }

    let password_hash = hash_password(&form.password)
        .map_err(|_| render_setup(Some("Не удалось сохранить пароль")))?;
    let created_at = Local::now().to_rfc3339();
    let user_id = db::insert_user(&conn, username, &password_hash, &created_at)
        .map_err(|_| render_setup(Some("Такой логин уже существует")))?;

    let token = Uuid::new_v4().to_string();
    db::create_session(&conn, user_id, &token, &created_at)
        .map_err(|_| render_setup(Some("Не удалось создать сессию")))?;
    db::prune_sessions(&conn, user_id, MAX_SESSIONS)
        .map_err(|_| render_setup(Some("Не удалось обновить сессии")))?;

    let mut cookie = Cookie::new("session", token);
    cookie.set_path("/");
    cookie.set_http_only(true);
    cookie.set_same_site(SameSite::Lax);
    cookies.add(cookie);

    Ok(Redirect::to("/"))
}

#[get("/login")]
fn login(pool: &State<DbPool>, cookies: &CookieJar<'_>) -> Result<Template, Redirect> {
    let conn = pool.get().map_err(|_| Redirect::to("/login"))?;
    if !db::has_users(&conn).unwrap_or(false) {
        return Err(Redirect::to("/setup"));
    }
    if current_user(pool, cookies).is_some() {
        return Err(Redirect::to("/"));
    }
    Ok(render_login(None))
}

#[post("/login", data = "<form>")]
fn login_post(
    pool: &State<DbPool>,
    cookies: &CookieJar<'_>,
    form: Form<LoginForm>,
) -> Result<Redirect, Template> {
    let conn = pool.get().map_err(|_| render_login(Some("Ошибка подключения к базе")))?;
    if !db::has_users(&conn).unwrap_or(false) {
        return Ok(Redirect::to("/setup"));
    }
    let form = form.into_inner();
    let username = form.username.trim();
    if username.is_empty() || form.password.is_empty() {
        return Err(render_login(Some("Введите логин и пароль")));
    }

    let creds = db::user_credentials(&conn, username)
        .map_err(|_| render_login(Some("Ошибка поиска пользователя")))?;
    let Some((user_id, hash)) = creds else {
        return Err(render_login(Some("Неверный логин или пароль")));
    };
    if !verify_password(&hash, &form.password) {
        return Err(render_login(Some("Неверный логин или пароль")));
    }

    let token = Uuid::new_v4().to_string();
    let created_at = Local::now().to_rfc3339();
    db::create_session(&conn, user_id, &token, &created_at)
        .map_err(|_| render_login(Some("Не удалось создать сессию")))?;
    db::prune_sessions(&conn, user_id, MAX_SESSIONS)
        .map_err(|_| render_login(Some("Не удалось обновить сессии")))?;

    let mut cookie = Cookie::new("session", token);
    cookie.set_path("/");
    cookie.set_http_only(true);
    cookie.set_same_site(SameSite::Lax);
    cookies.add(cookie);

    Ok(Redirect::to("/"))
}

#[get("/settings")]
fn settings(pool: &State<DbPool>, cookies: &CookieJar<'_>) -> Result<Template, Redirect> {
    let user = require_user(pool, cookies)?;
    let conn = pool.get().map_err(|_| Redirect::to("/login"))?;
    let sessions = db::session_count(&conn, user.id).unwrap_or(1);
    Ok(render_settings(&user.username, sessions, None, None))
}

#[post("/settings/password", data = "<form>")]
fn settings_password(
    pool: &State<DbPool>,
    cookies: &CookieJar<'_>,
    form: Form<ChangePasswordForm>,
) -> Result<Template, Redirect> {
    let user = require_user(pool, cookies)?;
    let conn = pool.get().map_err(|_| Redirect::to("/login"))?;
    let sessions = db::session_count(&conn, user.id).unwrap_or(1);
    let form = form.into_inner();

    if form.new_password.len() < 6 {
        return Ok(render_settings(
            &user.username,
            sessions,
            Some("Новый пароль должен быть не короче 6 символов"),
            None,
        ));
    }
    if form.new_password != form.confirm_password {
        return Ok(render_settings(
            &user.username,
            sessions,
            Some("Пароли не совпадают"),
            None,
        ));
    }

    let creds = db::user_credentials(&conn, &user.username)
        .map_err(|_| Redirect::to("/login"))?;
    let Some((_user_id, hash)) = creds else {
        return Ok(render_settings(
            &user.username,
            sessions,
            Some("Пользователь не найден"),
            None,
        ));
    };
    if !verify_password(&hash, &form.current_password) {
        return Ok(render_settings(
            &user.username,
            sessions,
            Some("Текущий пароль неверный"),
            None,
        ));
    }

    let new_hash = hash_password(&form.new_password).map_err(|_| Redirect::to("/login"))?;
    conn.execute(
        "UPDATE users SET password_hash = ?1 WHERE id = ?2",
        params![new_hash, user.id],
    )
    .map_err(|_| Redirect::to("/login"))?;
    Ok(render_settings(
        &user.username,
        sessions,
        None,
        Some("Пароль обновлен"),
    ))
}

#[post("/settings/logout_all")]
fn settings_logout_all(pool: &State<DbPool>, cookies: &CookieJar<'_>) -> Redirect {
    if let Ok(conn) = pool.get() {
        if let Some(user) = current_user(pool, cookies) {
            let _ = db::delete_sessions_for_user(&conn, user.id);
        }
    }
    let mut cookie = Cookie::named("session");
    cookie.set_path("/");
    cookies.remove(cookie);
    Redirect::to("/login")
}

#[get("/logout")]
fn logout(pool: &State<DbPool>, cookies: &CookieJar<'_>) -> Redirect {
    if let Some(cookie) = cookies.get("session") {
        if let Ok(conn) = pool.get() {
            let _ = db::delete_session(&conn, cookie.value());
        }
    }
    let mut cookie = Cookie::named("session");
    cookie.set_path("/");
    cookies.remove(cookie);
    Redirect::to("/login")
}

#[get("/?<month>")]
fn dashboard(
    pool: &State<DbPool>,
    cookies: &CookieJar<'_>,
    month: Option<String>,
) -> Result<Template, Redirect> {
    let user = require_user(pool, cookies)?;
    let selected = selected_month(month);
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
        "username": user.username,
        "income": format_money(income_cents),
        "expense": format_money(expense_cents),
        "net": format_money(income_cents - expense_cents),
        "budgets": budget_views,
    });
    Ok(Template::render("dashboard", &context))
}

#[get("/transactions?<month>")]
fn transactions(
    pool: &State<DbPool>,
    cookies: &CookieJar<'_>,
    month: Option<String>,
) -> Result<Template, Redirect> {
    let user = require_user(pool, cookies)?;
    let conn = pool.get().expect("db connection");
    let selected = selected_month(month);
    let records = db::list_transactions(&conn, Some(&selected)).unwrap_or_default();
    let categories = db::list_categories(&conn).unwrap_or_default();
    let views = records.into_iter().map(transaction_view).collect::<Vec<_>>();
    let months = available_months(&conn);

    let context = serde_json::json!({
        "month": selected,
        "months": months,
        "username": user.username,
        "today": today_ymd(),
        "transactions": views,
        "categories": categories,
    });
    Ok(Template::render("transactions", &context))
}

#[post("/transactions", data = "<form>")]
async fn add_transaction(
    pool: &State<DbPool>,
    cookies: &CookieJar<'_>,
    form: Form<TransactionForm<'_>>,
) -> Result<Redirect, rocket::http::Status> {
    if let Err(redirect) = require_user(pool, cookies) {
        return Ok(redirect);
    }
    let mut form = form.into_inner();
    let amount_cents = parse_amount_to_cents(&form.amount)
        .ok_or(rocket::http::Status::BadRequest)?;
    let occurred_on = if form.occurred_on.trim().is_empty() {
        today_ymd()
    } else {
        form.occurred_on
    };

    let conn = pool.get().map_err(|_| rocket::http::Status::InternalServerError)?;
    let category_name = if let Some(category_id) = form.category_id {
        db::category_name_by_id(&conn, category_id)
            .map_err(|_| rocket::http::Status::InternalServerError)?
    } else {
        None
    };
    drop(conn);
    let receipt_path =
        persist_receipt(form.receipt.take(), category_name.as_deref(), &form.kind).await?;

    let conn = pool.get().map_err(|_| rocket::http::Status::InternalServerError)?;
    db::insert_transaction(
        &conn,
        &form.kind,
        amount_cents,
        form.category_id,
        &occurred_on,
        form.note.as_deref(),
        receipt_path.as_deref(),
    )
    .map_err(|_| rocket::http::Status::InternalServerError)?;

    Ok(Redirect::to("/transactions"))
}

#[get("/categories")]
fn categories(pool: &State<DbPool>, cookies: &CookieJar<'_>) -> Result<Template, Redirect> {
    let user = require_user(pool, cookies)?;
    let conn = pool.get().expect("db connection");
    let list = db::list_categories(&conn).unwrap_or_default();
    let context = serde_json::json!({
        "username": user.username,
        "categories": list,
    });
    Ok(Template::render("categories", &context))
}

#[post("/categories", data = "<form>")]
fn add_category(
    pool: &State<DbPool>,
    cookies: &CookieJar<'_>,
    form: Form<CategoryForm>,
) -> Result<Redirect, rocket::http::Status> {
    if let Err(redirect) = require_user(pool, cookies) {
        return Ok(redirect);
    }
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
fn budgets(
    pool: &State<DbPool>,
    cookies: &CookieJar<'_>,
    month: Option<String>,
) -> Result<Template, Redirect> {
    let user = require_user(pool, cookies)?;
    let conn = pool.get().expect("db connection");
    let selected = selected_month(month);
    let list = db::list_budgets(&conn, &selected).unwrap_or_default();
    let categories = db::list_categories(&conn).unwrap_or_default();
    let views = list.into_iter().map(budget_view).collect::<Vec<_>>();
    let months = available_months(&conn);

    let context = serde_json::json!({
        "month": selected,
        "months": months,
        "username": user.username,
        "budgets": views,
        "categories": categories,
    });
    Ok(Template::render("budgets", &context))
}

#[post("/budgets", data = "<form>")]
fn add_budget(
    pool: &State<DbPool>,
    cookies: &CookieJar<'_>,
    form: Form<BudgetForm>,
) -> Result<Redirect, rocket::http::Status> {
    if let Err(redirect) = require_user(pool, cookies) {
        return Ok(redirect);
    }
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
fn reports(
    pool: &State<DbPool>,
    cookies: &CookieJar<'_>,
    month: Option<String>,
) -> Result<Template, Redirect> {
    let user = require_user(pool, cookies)?;
    let conn = pool.get().expect("db connection");
    let selected = selected_month(month);
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
        "username": user.username,
        "months": month_views,
        "categories": category_views,
    });
    Ok(Template::render("reports", &context))
}

fn transaction_view(record: TransactionRecord) -> TransactionView {
    TransactionView {
        id: record.id,
        kind: record.kind,
        amount: format_money(record.amount_cents),
        occurred_on: record.occurred_on,
        note: record.note,
        category_name: record.category_name,
        receipt_url: record
            .receipt_path
            .map(|name| format!("/receipts/{name}")),
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
    let receipts = receipts_dir();
    std::fs::create_dir_all(&receipts).expect("create receipts directory");

    rocket::build()
        .manage(pool)
        .mount(
            "/",
            routes![
                setup,
                setup_post,
                login,
                login_post,
                logout,
                settings,
                settings_password,
                settings_logout_all,
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
        .mount("/receipts", FileServer::from(receipts))
        .attach(Template::fairing())
}
