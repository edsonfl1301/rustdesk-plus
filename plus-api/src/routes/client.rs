use axum::{
    extract::{ConnectInfo, Path, Query, State},
    http::HeaderMap,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    net::SocketAddr,
};
use uuid::Uuid;

use crate::{
    auth::{issue_token, verify_password, AuthUser},
    error::AppError,
    models::User,
    presence,
    state::AppState,
};

pub fn router() -> Router<AppState> {
    Router::new()
        // Rotas legadas sem tenant (mantidas para compatibilidade)
        .route("/api/login-options", get(login_options))
        .route("/api/login", post(login))
        .route("/api/logout", post(logout))
        .route("/api/currentUser", post(current_user))
        .route("/api/heartbeat", post(heartbeat))
        .route("/api/sysinfo", post(sysinfo))
        .route("/api/sysinfo_ver", post(sysinfo_ver))
        .route("/api/ab/get", post(ab_get))
        .route("/api/ab", get(ab_get).post(ab_set))
        .route("/api/device-group/accessible", get(accessible_device_groups))
        .route("/api/users", get(group_users))
        .route("/api/peers", get(group_peers))
        // Rotas com tenant_id no path — usadas pelo instalador v2
        // O RustDesk cliente faz POST /t/<tenant_id>/api/heartbeat
        .route("/t/:tenant_id/api/login-options", get(login_options))
        .route("/t/:tenant_id/api/login", post(login))
        .route("/t/:tenant_id/api/logout", post(logout))
        .route("/t/:tenant_id/api/currentUser", post(current_user))
        .route("/t/:tenant_id/api/heartbeat", post(heartbeat_tenant_path))
        .route("/t/:tenant_id/api/sysinfo", post(sysinfo_tenant_path))
        .route("/t/:tenant_id/api/sysinfo_ver", post(sysinfo_ver))
        .route("/t/:tenant_id/api/ab/get", post(ab_get))
        .route("/t/:tenant_id/api/ab", get(ab_get).post(ab_set))
        .route(
            "/t/:tenant_id/api/device-group/accessible",
            get(accessible_device_groups),
        )
        .route("/t/:tenant_id/api/users", get(group_users))
        .route("/t/:tenant_id/api/peers", get(group_peers))
}

fn extract_ip(addr: SocketAddr, headers: &HeaderMap) -> String {
    headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.split(',').next())
        .map(|s| s.trim().to_string())
        .or_else(|| {
            headers
                .get("x-real-ip")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| addr.ip().to_string())
}

async fn login_options() -> impl IntoResponse {
    Json(Value::Array(vec![]))
}

#[derive(Debug, Deserialize)]
struct LoginBody {
    username: Option<String>,
    password: Option<String>,
    #[allow(dead_code)]
    id: Option<String>,
    #[allow(dead_code)]
    uuid: Option<String>,
}

async fn login(
    State(state): State<AppState>,
    Json(body): Json<LoginBody>,
) -> Result<Json<Value>, AppError> {
    let username = body.username.as_deref().unwrap_or_default().trim();
    let password = body.password.as_deref().unwrap_or_default();
    if username.is_empty() || password.is_empty() {
        return Err(AppError::Unauthorized);
    }

    // O cliente RustDesk chama este campo de `username`, enquanto o Plus usa
    // e-mail como identificador. Mantemos exatamente a mesma regra do painel,
    // inclusive a proteção contra credenciais ambíguas entre tenants.
    let users = sqlx::query_as::<_, User>(
        "SELECT * FROM users WHERE LOWER(email) = LOWER($1) ORDER BY (tenant_id IS NULL) DESC",
    )
    .bind(username)
    .fetch_all(&state.db)
    .await?;

    let mut matches = users
        .into_iter()
        .filter(|candidate| verify_password(password, &candidate.password_hash));
    let user = matches.next().ok_or(AppError::Unauthorized)?;
    if matches.next().is_some() {
        return Err(AppError::BadRequest(
            "email e senha correspondem a mais de um cliente; use credenciais diferentes"
                .to_string(),
        ));
    }
    if user.tenant_id.is_none() {
        return Err(AppError::BadRequest(
            "use no cliente RustDesk uma conta vinculada a um cliente, não a conta super admin"
                .to_string(),
        ));
    }

    let token = issue_token(user.id, &user.role, user.tenant_id)?;
    Ok(Json(json!({
        "type": "access_token",
        "access_token": token,
        "user": rustdesk_user(&user),
    })))
}

async fn logout() -> impl IntoResponse {
    Json(json!({}))
}

async fn current_user(
    State(state): State<AppState>,
    auth: AuthUser,
) -> Result<Json<Value>, AppError> {
    let user = sqlx::query_as::<_, User>("SELECT * FROM users WHERE id = $1")
        .bind(auth.id)
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::Unauthorized)?;
    Ok(Json(rustdesk_user(&user)))
}

fn rustdesk_user(user: &User) -> Value {
    json!({
        // O RustDesk usa `name` como o identificador exibido da conta.
        "name": user.email,
        "display_name": user.name,
        "email": user.email,
        "note": "",
        "status": 1,
        "is_admin": user.role == "admin" || user.role == "super_admin",
        "info": {},
    })
}

#[derive(Debug, Deserialize)]
struct TidQuery {
    tid: Option<Uuid>,
}

#[derive(Debug, Deserialize)]
struct HeartbeatBody {
    id: String,
    uuid: String,
    /// Embutido diretamente pelo agente; para o cliente RustDesk nativo vem via ?tid= na URL
    tenant_id: Option<Uuid>,
}

async fn heartbeat(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(q): Query<TidQuery>,
    Json(body): Json<HeartbeatBody>,
) -> Result<Json<Value>, AppError> {
    heartbeat_inner(&state, addr, &headers, q.tid, body).await
}

async fn heartbeat_inner(
    state: &AppState,
    addr: SocketAddr,
    headers: &HeaderMap,
    query_tid: Option<Uuid>,
    body: HeartbeatBody,
) -> Result<Json<Value>, AppError> {
    let Some(tenant_id) = body.tenant_id.or(query_tid) else {
        tracing::warn!(
            "heartbeat sem tenant_id descartado: rustdesk_id={}",
            body.id
        );
        return Ok(Json(json!({})));
    };
    let ip = extract_ip(addr, headers);

    // Remove placeholder criado pelo agente com o mesmo rustdesk_id mas UUID diferente
    sqlx::query(
        "DELETE FROM devices WHERE rustdesk_id = $1 AND uuid != $2 AND uuid LIKE 'host-%' AND tenant_id = $3",
    )
    .bind(&body.id)
    .bind(&body.uuid)
    .bind(tenant_id)
    .execute(&state.db)
    .await
    .ok();

    sqlx::query(
        r#"
        INSERT INTO devices (rustdesk_id, uuid, ip_address, last_seen_at, online, online_since, tenant_id)
        VALUES ($1, $2, $3, now(), true, now(), $4)
        ON CONFLICT (tenant_id, uuid) DO UPDATE SET
            rustdesk_id  = EXCLUDED.rustdesk_id,
            ip_address   = EXCLUDED.ip_address,
            last_seen_at = now(),
            online       = true,
            online_since = CASE WHEN devices.online = false THEN now() ELSE devices.online_since END
        WHERE devices.deleted_at IS NULL
        "#,
    )
    .bind(&body.id)
    .bind(&body.uuid)
    .bind(&ip)
    .bind(tenant_id)
    .execute(&state.db)
    .await?;

    // Auto-filial por IP dentro do mesmo tenant
    sqlx::query(
        r#"
        UPDATE devices AS d
        SET branch_id = (
            SELECT branch_id FROM devices other
            WHERE other.ip_address = $2
              AND other.branch_id IS NOT NULL
              AND other.uuid != $1
              AND other.tenant_id = $3
            ORDER BY other.last_seen_at DESC
            LIMIT 1
        )
        WHERE d.uuid = $1 AND d.tenant_id = $3
          AND d.branch_id IS NULL
          AND EXISTS (
              SELECT 1 FROM devices other
              WHERE other.ip_address = $2
                AND other.branch_id IS NOT NULL
                AND other.uuid != $1
                AND other.tenant_id = $3
          )
        "#,
    )
    .bind(&body.uuid)
    .bind(&ip)
    .bind(tenant_id)
    .execute(&state.db)
    .await?;

    Ok(Json(json!({})))
}

#[derive(Debug, Deserialize)]
struct SysinfoBody {
    id: String,
    uuid: String,
    hostname: Option<String>,
    os: Option<String>,
    tenant_id: Option<Uuid>,
}

async fn sysinfo(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(q): Query<TidQuery>,
    Json(body): Json<SysinfoBody>,
) -> Result<impl IntoResponse, AppError> {
    sysinfo_inner(&state, addr, &headers, q.tid, body).await
}

async fn sysinfo_inner(
    state: &AppState,
    addr: SocketAddr,
    headers: &HeaderMap,
    query_tid: Option<Uuid>,
    body: SysinfoBody,
) -> Result<impl IntoResponse, AppError> {
    let Some(tenant_id) = body.tenant_id.or(query_tid) else {
        tracing::warn!("sysinfo sem tenant_id descartado: rustdesk_id={}", body.id);
        return Ok("SYSINFO_IGNORED");
    };
    let ip = extract_ip(addr, headers);

    sqlx::query(
        "DELETE FROM devices WHERE rustdesk_id = $1 AND uuid != $2 AND uuid LIKE 'host-%' AND tenant_id = $3",
    )
    .bind(&body.id)
    .bind(&body.uuid)
    .bind(tenant_id)
    .execute(&state.db)
    .await
    .ok();

    sqlx::query(
        r#"
        INSERT INTO devices (rustdesk_id, uuid, hostname, os, ip_address, last_seen_at, online, online_since, tenant_id)
        VALUES ($1, $2, $3, $4, $5, now(), true, now(), $6)
        ON CONFLICT (tenant_id, uuid) DO UPDATE SET
            rustdesk_id  = EXCLUDED.rustdesk_id,
            hostname     = EXCLUDED.hostname,
            os           = EXCLUDED.os,
            ip_address   = EXCLUDED.ip_address,
            last_seen_at = now(),
            online       = true,
            online_since = CASE WHEN devices.online = false THEN now() ELSE devices.online_since END
        WHERE devices.deleted_at IS NULL
        "#,
    )
    .bind(&body.id)
    .bind(&body.uuid)
    .bind(&body.hostname)
    .bind(&body.os)
    .bind(&ip)
    .bind(tenant_id)
    .execute(&state.db)
    .await?;

    Ok("SYSINFO_UPDATED")
}

/// Tenant_id extraído do path: POST /t/:tenant_id/api/heartbeat
async fn heartbeat_tenant_path(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(tenant_id): Path<Uuid>,
    Json(body): Json<HeartbeatBody>,
) -> Result<Json<Value>, AppError> {
    let body = HeartbeatBody {
        tenant_id: Some(body.tenant_id.unwrap_or(tenant_id)),
        ..body
    };
    heartbeat_inner(&state, addr, &headers, None, body).await
}

/// Tenant_id extraído do path: POST /t/:tenant_id/api/sysinfo
async fn sysinfo_tenant_path(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(tenant_id): Path<Uuid>,
    Json(body): Json<SysinfoBody>,
) -> Result<impl IntoResponse, AppError> {
    let body = SysinfoBody {
        tenant_id: Some(body.tenant_id.unwrap_or(tenant_id)),
        ..body
    };
    sysinfo_inner(&state, addr, &headers, None, body).await
}

async fn sysinfo_ver() -> impl IntoResponse {
    "1"
}

async fn ab_get(State(state): State<AppState>, auth: AuthUser) -> Result<Json<Value>, AppError> {
    let tenant_id = auth.tenant_id.ok_or(AppError::Forbidden)?;
    // The native client treats `password` on a shared address-book peer as the
    // unattended-access password. Only roles allowed to operate devices receive
    // it; inventory-only users keep the catalog without the credential.
    let shared_password = if can_connect_remotely(&auth.role) {
        crate::config::load_tenant_password(&state.db, tenant_id)
            .await
            .unwrap_or_default()
    } else {
        String::new()
    };

    let device_rows = sqlx::query(
        r#"
        SELECT d.id, d.rustdesk_id, d.hostname, d.os, d.alias, d.description,
               b.name AS branch_name
        FROM devices d
        LEFT JOIN branches b ON b.id = d.branch_id AND b.tenant_id = d.tenant_id
        WHERE d.tenant_id = $1 AND d.deleted_at IS NULL
        ORDER BY d.favorite DESC, d.online DESC,
                 COALESCE(NULLIF(d.alias, ''), NULLIF(d.hostname, ''), d.rustdesk_id)
        "#,
    )
    .bind(tenant_id)
    .fetch_all(&state.db)
    .await?;

    let tag_rows = sqlx::query(
        r#"
        SELECT dt.device_id, t.name, t.color
        FROM device_tags dt
        JOIN devices d ON d.id = dt.device_id
        JOIN tags t ON t.id = dt.tag_id AND t.tenant_id = d.tenant_id
        WHERE d.tenant_id = $1 AND d.deleted_at IS NULL
        ORDER BY t.name
        "#,
    )
    .bind(tenant_id)
    .fetch_all(&state.db)
    .await?;

    let mut device_tags: HashMap<Uuid, Vec<String>> = HashMap::new();
    let mut tag_names = BTreeSet::new();
    let mut tag_colors = BTreeMap::new();
    for row in tag_rows {
        let device_id: Uuid = row.try_get("device_id")?;
        let name: String = row.try_get("name")?;
        let color: String = row.try_get("color")?;
        device_tags.entry(device_id).or_default().push(name.clone());
        tag_names.insert(name.clone());
        tag_colors.insert(name, flutter_color(&color));
    }

    let mut peers = Vec::with_capacity(device_rows.len());
    for row in device_rows {
        let device_id: Uuid = row.try_get("id")?;
        let rustdesk_id: String = row.try_get("rustdesk_id")?;
        let hostname: Option<String> = row.try_get("hostname")?;
        let os: Option<String> = row.try_get("os")?;
        let alias: Option<String> = row.try_get("alias")?;
        let description: Option<String> = row.try_get("description")?;
        let branch_name: Option<String> = row.try_get("branch_name")?;
        let mut tags = device_tags.remove(&device_id).unwrap_or_default();

        if let Some(branch) = branch_name.filter(|s| !s.trim().is_empty()) {
            let branch_tag = format!("Filial: {branch}");
            if !tags.contains(&branch_tag) {
                tags.push(branch_tag.clone());
            }
            tag_names.insert(branch_tag.clone());
            tag_colors.entry(branch_tag).or_insert(0xFF64748B);
        }

        peers.push(json!({
            "id": rustdesk_id,
            "password": shared_password,
            "hostname": hostname.clone().unwrap_or_default(),
            "platform": rustdesk_platform(os.as_deref()),
            "alias": alias.filter(|s| !s.trim().is_empty())
                .or(hostname)
                .unwrap_or_default(),
            "tags": tags,
            "note": description.unwrap_or_default(),
            "same_server": true,
        }));
    }

    // O endpoint legado transporta o catálogo interno como uma string JSON.
    // Ele é mantido somente para leitura: o painel Plus é a fonte do inventário.
    let tag_colors_json = serde_json::to_string(&tag_colors).map_err(anyhow::Error::new)?;
    let data = serde_json::to_string(&json!({
        "peers": peers,
        "tags": tag_names.into_iter().collect::<Vec<_>>(),
        "tag_colors": tag_colors_json,
    }))
    .map_err(anyhow::Error::new)?;
    Ok(Json(json!({ "data": data })))
}

fn can_connect_remotely(role: &str) -> bool {
    matches!(role, "admin" | "operator")
}

async fn ab_set() -> impl IntoResponse {
    Json(json!({}))
}

#[derive(Debug, Deserialize)]
struct GroupPage {
    current: Option<i64>,
    #[serde(rename = "pageSize")]
    page_size: Option<i64>,
}

impl GroupPage {
    fn limit_offset(&self) -> (i64, i64) {
        let limit = self.page_size.unwrap_or(100).clamp(1, 500);
        let current = self.current.unwrap_or(1).max(1);
        (limit, (current - 1) * limit)
    }
}

async fn accessible_device_groups(
    State(state): State<AppState>,
    auth: AuthUser,
    Query(page): Query<GroupPage>,
) -> Result<Json<Value>, AppError> {
    let tenant_id = auth.tenant_id.ok_or(AppError::Forbidden)?;
    let (limit, offset) = page.limit_offset();
    let total: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM branches WHERE tenant_id = $1",
    )
    .bind(tenant_id)
    .fetch_one(&state.db)
    .await?;
    let rows = sqlx::query(
        "SELECT name FROM branches WHERE tenant_id = $1 ORDER BY name LIMIT $2 OFFSET $3",
    )
    .bind(tenant_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.db)
    .await?;
    let data = rows
        .into_iter()
        .map(|row| json!({ "name": row.get::<String, _>("name") }))
        .collect::<Vec<_>>();
    Ok(Json(json!({ "total": total, "data": data })))
}

async fn group_users(
    State(state): State<AppState>,
    auth: AuthUser,
    Query(page): Query<GroupPage>,
) -> Result<Json<Value>, AppError> {
    let tenant_id = auth.tenant_id.ok_or(AppError::Forbidden)?;
    let (limit, offset) = page.limit_offset();
    let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE tenant_id = $1")
        .bind(tenant_id)
        .fetch_one(&state.db)
        .await?;
    let users = sqlx::query_as::<_, User>(
        "SELECT * FROM users WHERE tenant_id = $1 ORDER BY name, email LIMIT $2 OFFSET $3",
    )
    .bind(tenant_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.db)
    .await?;
    let data = users.iter().map(rustdesk_user).collect::<Vec<_>>();
    Ok(Json(json!({ "total": total, "data": data })))
}

async fn group_peers(
    State(state): State<AppState>,
    auth: AuthUser,
    Query(page): Query<GroupPage>,
) -> Result<Json<Value>, AppError> {
    let tenant_id = auth.tenant_id.ok_or(AppError::Forbidden)?;
    let (limit, offset) = page.limit_offset();
    let total: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM devices WHERE tenant_id = $1 AND deleted_at IS NULL",
    )
    .bind(tenant_id)
    .fetch_one(&state.db)
    .await?;
    let rows = sqlx::query(
        r#"
        SELECT d.rustdesk_id, d.hostname, d.os, d.alias, d.description, d.online,
               b.name AS branch_name, u.email AS owner_email, u.name AS owner_name
        FROM devices d
        LEFT JOIN branches b ON b.id = d.branch_id AND b.tenant_id = d.tenant_id
        LEFT JOIN users u ON u.id = d.owner_user_id AND u.tenant_id = d.tenant_id
        WHERE d.tenant_id = $1 AND d.deleted_at IS NULL
        ORDER BY d.favorite DESC, d.online DESC,
                 COALESCE(NULLIF(d.alias, ''), NULLIF(d.hostname, ''), d.rustdesk_id)
        LIMIT $2 OFFSET $3
        "#,
    )
    .bind(tenant_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.db)
    .await?;

    let peer_ids = rows
        .iter()
        .map(|row| row.get::<String, _>("rustdesk_id"))
        .collect::<Vec<_>>();
    let live_states = match std::env::var("HBBS_PRESENCE_ADDR") {
        Ok(address) if !address.trim().is_empty() => {
            match tokio::time::timeout(
                std::time::Duration::from_secs(5),
                presence::query(&address, &peer_ids),
            )
            .await
            {
                Ok(Ok(states)) => Some(states),
                Ok(Err(error)) => {
                    tracing::warn!("group presence query failed: {error:#}");
                    None
                }
                Err(_) => {
                    tracing::warn!("group presence query timed out");
                    None
                }
            }
        }
        _ => None,
    };

    let data = rows
        .into_iter()
        .enumerate()
        .map(|(index, row)| {
            let hostname: Option<String> = row.get("hostname");
            let alias: Option<String> = row.get("alias");
            let device_name = alias
                .filter(|value| !value.trim().is_empty())
                .or(hostname)
                .unwrap_or_default();
            json!({
                "id": row.get::<String, _>("rustdesk_id"),
                "info": {
                    "device_name": device_name,
                    "os": group_platform(row.get::<Option<String>, _>("os").as_deref()),
                    "username": "",
                },
                "status": if live_states
                    .as_ref()
                    .and_then(|states| states.get(index))
                    .copied()
                    .unwrap_or_else(|| row.get::<bool, _>("online"))
                { 1 } else { 0 },
                "user": row.get::<Option<String>, _>("owner_email").unwrap_or_default(),
                "user_name": row.get::<Option<String>, _>("owner_name").unwrap_or_default(),
                "device_group_name": row.get::<Option<String>, _>("branch_name").unwrap_or_default(),
                "note": row.get::<Option<String>, _>("description").unwrap_or_default(),
            })
        })
        .collect::<Vec<_>>();
    Ok(Json(json!({ "total": total, "data": data })))
}

fn rustdesk_platform(os: Option<&str>) -> &'static str {
    let os = os.unwrap_or_default().to_ascii_lowercase();
    if os.contains("windows") {
        "Windows"
    } else if os.contains("mac") || os.contains("darwin") {
        "Mac OS"
    } else if os.contains("android") {
        "Android"
    } else if os.contains("ios") {
        "iOS"
    } else if os.contains("linux") {
        "Linux"
    } else {
        ""
    }
}

fn group_platform(os: Option<&str>) -> &'static str {
    match rustdesk_platform(os) {
        "Mac OS" => "macOS",
        value => value,
    }
}

fn flutter_color(color: &str) -> u32 {
    let hex = color.trim().trim_start_matches('#');
    match hex.len() {
        6 => u32::from_str_radix(hex, 16)
            .map(|rgb| 0xFF00_0000 | rgb)
            .unwrap_or(0xFF3B_82F6),
        8 => u32::from_str_radix(hex, 16).unwrap_or(0xFF3B_82F6),
        _ => 0xFF3B_82F6,
    }
}

#[cfg(test)]
mod tests {
    use super::{can_connect_remotely, flutter_color, rustdesk_platform};

    #[test]
    fn shares_remote_password_only_with_operating_roles() {
        assert!(can_connect_remotely("admin"));
        assert!(can_connect_remotely("operator"));
        assert!(!can_connect_remotely("viewer"));
        assert!(!can_connect_remotely("super_admin"));
    }

    #[test]
    fn maps_platform_names_used_by_the_client() {
        assert_eq!(rustdesk_platform(Some("Windows 11")), "Windows");
        assert_eq!(rustdesk_platform(Some("Ubuntu Linux")), "Linux");
        assert_eq!(rustdesk_platform(Some("Darwin")), "Mac OS");
    }

    #[test]
    fn converts_css_colors_to_flutter_argb() {
        assert_eq!(flutter_color("#3b82f6"), 0xFF3B82F6);
        assert_eq!(flutter_color("#8044CC22"), 0x8044CC22);
        assert_eq!(flutter_color("invalid"), 0xFF3B82F6);
    }
}
