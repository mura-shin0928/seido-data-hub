//! 読み取り専用の HTTP API。Lambda でもローカルでも同じ Router を使う。

use axum::extract::rejection::QueryRejection;
use axum::extract::{FromRequestParts, Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, FixedOffset};
use entity::{areas, programs};
use sea_orm::sea_query::Expr;
use sea_orm::{
    ColumnTrait, Condition, DatabaseConnection, DbErr, EntityTrait, Order, QueryFilter, QueryOrder,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

pub fn router(db: DatabaseConnection) -> Router {
    Router::new()
        .route("/v1/areas", get(list_areas))
        .route("/v1/areas/{code}/programs", get(list_programs))
        .route("/v1/programs/{id}", get(get_program))
        .fallback(|| async { ApiError::NotFound("エンドポイントが無い") })
        .with_state(db)
}

/// CC BY 4.0 の出典表記。データを返すレスポンスには必ず付ける。
/// 自治体が発信者だと読めないよう、改変していることと公式サイトで確認することを添える。
#[derive(Debug, Serialize)]
pub struct Attribution {
    pub source: &'static str,
    pub license: &'static str,
    pub license_url: &'static str,
    pub notice: &'static str,
}

pub const ATTRIBUTION: Attribution = Attribution {
    source: "東京デジタル2030ビジョン（こどもDX）子育て支援制度レジストリ（東京都・GovTech東京）を改変して利用",
    license: "CC BY 4.0",
    license_url: "https://creativecommons.org/licenses/by/4.0/deed.ja",
    notice: "各制度の内容は、必ず各自治体の公式サイトで確認してください。",
};

#[derive(Debug, Serialize)]
struct Body<T> {
    data: T,
    attribution: &'static Attribution,
}

fn body<T: Serialize>(data: T) -> Json<Body<T>> {
    Json(Body {
        data,
        attribution: &ATTRIBUTION,
    })
}

#[derive(Debug, Serialize)]
struct AreaView {
    code: String,
    name: String,
    parent_code: Option<String>,
}

impl From<areas::Model> for AreaView {
    fn from(a: areas::Model) -> Self {
        Self {
            code: a.code,
            name: a.name,
            parent_code: a.parent_code,
        }
    }
}

/// 一覧の1件。レジストリの行（registry）は大きいので詳細でだけ返す。
#[derive(Debug, Serialize)]
struct ProgramView {
    id: Uuid,
    um: String,
    area_code: String,
    canonical_name: String,
    short_name: Option<String>,
    source_url: String,
    category_codes: Vec<String>,
    target_codes: Vec<String>,
    content_codes: Vec<String>,
    age_min_months: Option<i32>,
    age_max_months: Option<i32>,
    status: String,
    checked_at: Option<DateTime<FixedOffset>>,
}

#[derive(Debug, Serialize)]
struct ProgramDetailView {
    #[serde(flatten)]
    program: ProgramView,
    psid: String,
    registry: Value,
    imported_at: DateTime<FixedOffset>,
}

fn program_view(p: programs::Model) -> ProgramDetailView {
    ProgramDetailView {
        program: ProgramView {
            id: p.id,
            um: p.um,
            area_code: p.area_code,
            canonical_name: p.canonical_name,
            short_name: p.short_name,
            source_url: p.source_url,
            category_codes: p.category_codes,
            target_codes: p.target_codes,
            content_codes: p.content_codes,
            age_min_months: p.age_min_months,
            age_max_months: p.age_max_months,
            status: p.status,
            checked_at: p.checked_at,
        },
        psid: p.psid,
        registry: p.registry,
        imported_at: p.imported_at,
    }
}

async fn list_areas(State(db): State<DatabaseConnection>) -> Result<impl IntoResponse, ApiError> {
    let areas = areas::Entity::find()
        .order_by_asc(areas::Column::Code)
        .all(&db)
        .await?;
    Ok(body(
        areas.into_iter().map(AreaView::from).collect::<Vec<_>>(),
    ))
}

#[derive(Debug, Deserialize, FromRequestParts)]
#[from_request(via(axum::extract::Query), rejection(ApiError))]
struct ProgramsQuery {
    age_months: Option<i32>,
    category: Option<String>,
}

/// 自治体と、その都道府県の制度を返す。
async fn list_programs(
    State(db): State<DatabaseConnection>,
    Path(code): Path<String>,
    query: ProgramsQuery,
) -> Result<impl IntoResponse, ApiError> {
    let area = areas::Entity::find_by_id(&code)
        .one(&db)
        .await?
        .ok_or(ApiError::NotFound("自治体が無い"))?;

    let mut cond = Condition::all().add(
        programs::Column::AreaCode.is_in([Some(area.code), area.parent_code].into_iter().flatten()),
    );
    if let Some(months) = query.age_months {
        if months < 0 {
            return Err(ApiError::BadRequest("age_months は0以上".into()));
        }
        // 月齢の上下限が無い制度は、どの月齢でも表示する
        cond = cond
            .add(
                Condition::any()
                    .add(programs::Column::AgeMinMonths.is_null())
                    .add(programs::Column::AgeMinMonths.lte(months)),
            )
            .add(
                Condition::any()
                    .add(programs::Column::AgeMaxMonths.is_null())
                    .add(programs::Column::AgeMaxMonths.gt(months)),
            );
    }
    if let Some(category) = query.category {
        cond = cond.add(Expr::cust_with_values(
            "$1 = any(category_codes)",
            [category],
        ));
    }

    // 市区町村のコードは同じ都道府県のコード（3〜5桁目が000）より大きいので、降順で市区町村が先に来る
    let rows = programs::Entity::find()
        .filter(cond)
        .order_by_desc(programs::Column::AreaCode)
        .order_by(Expr::cust("substring(um from 3)::int"), Order::Asc)
        .order_by_asc(programs::Column::Psid)
        .all(&db)
        .await?;
    Ok(body(
        rows.into_iter()
            .map(|p| program_view(p).program)
            .collect::<Vec<_>>(),
    ))
}

async fn get_program(
    State(db): State<DatabaseConnection>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    const NOT_FOUND: ApiError = ApiError::NotFound("制度が無い");
    let id = Uuid::parse_str(&id).map_err(|_| NOT_FOUND)?;
    let program = programs::Entity::find_by_id(id)
        .one(&db)
        .await?
        .ok_or(NOT_FOUND)?;
    Ok(body(program_view(program)))
}

#[derive(Debug)]
pub enum ApiError {
    NotFound(&'static str),
    BadRequest(String),
    Database(DbErr),
}

impl From<DbErr> for ApiError {
    fn from(err: DbErr) -> Self {
        Self::Database(err)
    }
}

impl From<QueryRejection> for ApiError {
    fn from(rejection: QueryRejection) -> Self {
        Self::BadRequest(rejection.body_text())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            Self::NotFound(message) => (StatusCode::NOT_FOUND, message.to_string()),
            Self::BadRequest(message) => (StatusCode::BAD_REQUEST, message),
            Self::Database(err) => {
                // 中身は利用者に見せず、ログ（Lambda では CloudWatch）にだけ出す
                eprintln!("database error: {err}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "サーバーでエラーが起きた".to_string(),
                )
            }
        };
        (
            status,
            Json(serde_json::json!({ "error": { "message": message } })),
        )
            .into_response()
    }
}
