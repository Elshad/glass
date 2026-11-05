//! SCIM 2.0 provisioning (RFC 7643/7644, the parts identity providers use): Okta, Entra ID
//! and others push users and groups here, so the directory decides who may sign in.
//!
//! - A deactivated or deleted user can't sign in (SSO, LDAP), and their guest passes are
//!   revoked at once, so open sessions end within seconds.
//! - Groups feed `group:` access rules (see acl.rs), matched by the user's email
//!   (`userName`, or one of its `emails`).
//! - Users nobody provisioned are judged by the access rules alone.
//!
//! On when `glass config scim-token new` has made a bearer token; the provider sends it.

use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::Result;
use axum::extract::{Path, Query};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const USER_SCHEMA: &str = "urn:ietf:params:scim:schemas:core:2.0:User";
const GROUP_SCHEMA: &str = "urn:ietf:params:scim:schemas:core:2.0:Group";
const LIST_SCHEMA: &str = "urn:ietf:params:scim:api:messages:2.0:ListResponse";
const ERROR_SCHEMA: &str = "urn:ietf:params:scim:api:messages:2.0:Error";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct User {
    pub id: String,
    pub user_name: String,
    #[serde(default = "yes")]
    pub active: bool,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub external_id: Option<String>,
    #[serde(default)]
    pub emails: Vec<String>,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Group {
    pub id: String,
    pub display_name: String,
    #[serde(default)]
    pub external_id: Option<String>,
    /// User ids.
    #[serde(default)]
    pub members: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Directory {
    #[serde(default)]
    pub users: Vec<User>,
    #[serde(default)]
    pub groups: Vec<Group>,
}

/// What the directory says about an account.
#[derive(Debug, Clone, PartialEq)]
pub enum Account {
    /// Not provisioned: the access rules alone decide.
    Unknown,
    /// Deactivated: no access.
    Inactive,
    /// Provisioned and active, in these groups (display names).
    Active(Vec<String>),
}

pub fn path() -> PathBuf {
    crate::config_dir().join("directory.json")
}

static LOCK: Mutex<()> = Mutex::new(());

pub fn load() -> Directory {
    std::fs::read(path())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save(d: &Directory) -> Result<()> {
    let p = path();
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = p.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(d)?)?;
    std::fs::rename(&tmp, &p)?;
    Ok(())
}

impl User {
    fn is(&self, email: &str) -> bool {
        let email = email.to_ascii_lowercase();
        self.user_name.to_ascii_lowercase() == email
            || self.emails.iter().any(|e| e.to_ascii_lowercase() == email)
    }

    /// Every address their guest passes may have been minted for.
    fn accounts(&self) -> Vec<String> {
        std::iter::once(self.user_name.clone())
            .chain(self.emails.iter().cloned())
            .map(|e| e.to_ascii_lowercase())
            .collect()
    }
}

impl Directory {
    pub fn account(&self, email: &str) -> Account {
        match self.users.iter().find(|u| u.is(email)) {
            None => Account::Unknown,
            Some(u) if !u.active => Account::Inactive,
            Some(u) => Account::Active(
                self.groups
                    .iter()
                    .filter(|g| g.members.contains(&u.id))
                    .map(|g| g.display_name.clone())
                    .collect(),
            ),
        }
    }
}

/// The directory's view of `email`.
pub fn account(email: &str) -> Account {
    load().account(email)
}

/// Revokes every pass of a user who just lost access.
fn cut_off(user: &User) {
    let Some(shared) = crate::server::shared() else { return };
    let file = crate::access::file_for(&shared.token_file);
    let mut n = 0;
    for a in user.accounts() {
        n += crate::access::revoke_account(&file, &a).unwrap_or(0);
    }
    tracing::info!("scim: {} deprovisioned ({n} guest pass(es) revoked)", user.user_name);
}

// ---- Pure parts: filters and PATCH (unit tested) ----

/// `userName eq "ana@example.com"` → ("username", "ana@example.com"). Only `eq` is
/// supported (what providers use to look a resource up).
pub fn parse_filter(filter: &str) -> Option<(String, String)> {
    let f = filter.trim();
    let (attr, rest) = f.split_once(char::is_whitespace)?;
    let (op, value) = rest.trim_start().split_once(char::is_whitespace)?;
    if !op.eq_ignore_ascii_case("eq") {
        return None;
    }
    let value = value.trim();
    let value = value.strip_prefix('"')?.strip_suffix('"')?;
    Some((attr.to_ascii_lowercase(), value.replace("\\\"", "\"")))
}

/// SCIM booleans arrive as true/false or, from some providers, "True"/"False".
fn as_bool(v: &Value) -> Option<bool> {
    match v {
        Value::Bool(b) => Some(*b),
        Value::String(s) => match s.to_ascii_lowercase().as_str() {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

fn emails_of(v: &Value) -> Vec<String> {
    match v {
        Value::Array(list) => list
            .iter()
            .filter_map(|e| e.get("value").or(Some(e)).and_then(|v| v.as_str()))
            .map(|s| s.to_string())
            .collect(),
        Value::String(s) => vec![s.clone()],
        _ => Vec::new(),
    }
}

/// Sets user attributes from a SCIM resource (or a PATCH value object).
fn apply_user_fields(u: &mut User, body: &Value) {
    for (k, v) in body.as_object().into_iter().flatten() {
        match k.to_ascii_lowercase().as_str() {
            "username" => {
                if let Some(s) = v.as_str() {
                    u.user_name = s.to_string();
                }
            }
            "active" => {
                if let Some(b) = as_bool(v) {
                    u.active = b;
                }
            }
            "displayname" => u.display_name = v.as_str().map(String::from),
            "externalid" => u.external_id = v.as_str().map(String::from),
            "emails" => u.emails = emails_of(v),
            "name" => {
                if u.display_name.is_none() {
                    u.display_name = v.get("formatted").and_then(|f| f.as_str()).map(String::from);
                }
            }
            _ => {}
        }
    }
}

/// Applies PATCH operations to a user.
pub fn patch_user(u: &mut User, ops: &[Value]) -> Result<(), String> {
    for op in ops {
        let kind = op["op"].as_str().unwrap_or("").to_ascii_lowercase();
        let path = op["path"].as_str().map(|p| p.to_ascii_lowercase());
        let value = &op["value"];
        match (kind.as_str(), path.as_deref()) {
            ("add" | "replace", None) => apply_user_fields(u, value),
            ("add" | "replace", Some(p)) => {
                let field = if p.starts_with("emails") { "emails" } else { p };
                apply_user_fields(u, &json!({ field: value }));
            }
            ("remove", Some(p)) if p.starts_with("emails") => u.emails.clear(),
            ("remove", Some("displayname")) => u.display_name = None,
            ("remove", Some("externalid")) => u.external_id = None,
            ("remove", _) => {}
            _ => return Err(format!("unsupported operation {kind}")),
        }
    }
    Ok(())
}

fn member_ids(v: &Value) -> Vec<String> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| m["value"].as_str().map(String::from))
        .collect()
}

/// `members[value eq "id"]` → the id.
fn member_in_path(path: &str) -> Option<String> {
    let inner = path.strip_prefix("members[")?.strip_suffix(']')?;
    parse_filter(inner).filter(|(a, _)| a == "value").map(|(_, v)| v)
}

/// Applies PATCH operations to a group.
pub fn patch_group(g: &mut Group, ops: &[Value]) -> Result<(), String> {
    for op in ops {
        let kind = op["op"].as_str().unwrap_or("").to_ascii_lowercase();
        let path = op["path"].as_str().unwrap_or("");
        let lower = path.to_ascii_lowercase();
        let value = &op["value"];
        match kind.as_str() {
            "add" if lower == "members" => {
                for id in member_ids(value) {
                    if !g.members.contains(&id) {
                        g.members.push(id);
                    }
                }
            }
            "replace" if lower == "members" => g.members = member_ids(value),
            "remove" if lower == "members" => {
                // A value list removes those; none removes everyone.
                let ids = member_ids(value);
                if ids.is_empty() {
                    g.members.clear();
                } else {
                    g.members.retain(|m| !ids.contains(m));
                }
            }
            "remove" if lower.starts_with("members[") => {
                let id = member_in_path(path).ok_or("bad member filter")?;
                g.members.retain(|m| *m != id);
            }
            "add" | "replace" if lower == "displayname" => {
                if let Some(s) = value.as_str() {
                    g.display_name = s.to_string();
                }
            }
            "add" | "replace" if path.is_empty() => {
                if let Some(s) = value["displayName"].as_str() {
                    g.display_name = s.to_string();
                }
                if value.get("members").is_some() {
                    let ids = member_ids(&value["members"]);
                    if kind == "replace" {
                        g.members = ids;
                    } else {
                        for id in ids {
                            if !g.members.contains(&id) {
                                g.members.push(id);
                            }
                        }
                    }
                }
            }
            _ => return Err(format!("unsupported operation {kind} {path}")),
        }
    }
    Ok(())
}

// ---- HTTP ----

pub fn routes() -> axum::Router {
    axum::Router::new()
        .route("/scim/v2/ServiceProviderConfig", get(service_provider_config))
        .route("/scim/v2/Users", get(list_users).post(create_user))
        .route(
            "/scim/v2/Users/{id}",
            get(get_user).put(put_user).patch(patch_user_route).delete(delete_user),
        )
        .route("/scim/v2/Groups", get(list_groups).post(create_group))
        .route(
            "/scim/v2/Groups/{id}",
            get(get_group).put(put_group).patch(patch_group_route).delete(delete_group),
        )
}

fn scim(status: StatusCode, body: Value) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "application/scim+json")],
        body.to_string(),
    )
        .into_response()
}

fn error(status: StatusCode, detail: &str) -> Response {
    scim(
        status,
        json!({ "schemas": [ERROR_SCHEMA], "status": status.as_u16().to_string(), "detail": detail }),
    )
}

/// The provider's bearer token; SCIM is off (404) without one configured.
fn authorized(headers: &HeaderMap) -> Result<(), Response> {
    let Some(token) = crate::settings::scim_token() else {
        return Err(error(StatusCode::NOT_FOUND, "SCIM isn't set up on this computer"));
    };
    let given = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim);
    match given {
        Some(g) if crate::control::token_matches(g, &token) => Ok(()),
        _ => Err(error(StatusCode::UNAUTHORIZED, "bad or missing bearer token")),
    }
}

fn body_json(body: &[u8]) -> Result<Value, Response> {
    serde_json::from_slice(body).map_err(|e| error(StatusCode::BAD_REQUEST, &format!("invalid JSON: {e}")))
}

fn new_id() -> String {
    let mut b = [0u8; 16];
    let _ = crate::control::getrandom(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn user_json(u: &User) -> Value {
    json!({
        "schemas": [USER_SCHEMA],
        "id": u.id,
        "userName": u.user_name,
        "active": u.active,
        "displayName": u.display_name,
        "externalId": u.external_id,
        "emails": u.emails.iter().enumerate().map(|(i, e)| json!({ "value": e, "primary": i == 0 })).collect::<Vec<_>>(),
        "meta": { "resourceType": "User", "location": format!("/scim/v2/Users/{}", u.id) },
    })
}

fn group_json(g: &Group, d: &Directory) -> Value {
    json!({
        "schemas": [GROUP_SCHEMA],
        "id": g.id,
        "displayName": g.display_name,
        "externalId": g.external_id,
        "members": g.members.iter().map(|id| {
            let display = d.users.iter().find(|u| u.id == *id).map(|u| u.user_name.clone());
            json!({ "value": id, "display": display })
        }).collect::<Vec<_>>(),
        "meta": { "resourceType": "Group", "location": format!("/scim/v2/Groups/{}", g.id) },
    })
}

#[derive(Deserialize)]
struct ListQuery {
    filter: Option<String>,
    #[serde(rename = "startIndex")]
    start_index: Option<usize>,
    count: Option<usize>,
}

fn list(items: Vec<Value>, q: &ListQuery) -> Response {
    let total = items.len();
    let start = q.start_index.unwrap_or(1).max(1);
    let page: Vec<Value> = items
        .into_iter()
        .skip(start - 1)
        .take(q.count.unwrap_or(100).min(500))
        .collect();
    scim(
        StatusCode::OK,
        json!({ "schemas": [LIST_SCHEMA], "totalResults": total, "startIndex": start, "itemsPerPage": page.len(), "Resources": page }),
    )
}

async fn service_provider_config(headers: HeaderMap) -> Response {
    if let Err(r) = authorized(&headers) {
        return r;
    }
    scim(
        StatusCode::OK,
        json!({
            "schemas": ["urn:ietf:params:scim:schemas:core:2.0:ServiceProviderConfig"],
            "patch": { "supported": true },
            "bulk": { "supported": false, "maxOperations": 0, "maxPayloadSize": 0 },
            "filter": { "supported": true, "maxResults": 500 },
            "changePassword": { "supported": false },
            "sort": { "supported": false },
            "etag": { "supported": false },
            "authenticationSchemes": [{ "type": "oauthbearertoken", "name": "Bearer token", "description": "glass config scim-token" }],
        }),
    )
}

async fn list_users(headers: HeaderMap, Query(q): Query<ListQuery>) -> Response {
    if let Err(r) = authorized(&headers) {
        return r;
    }
    let d = load();
    let filter = match q.filter.as_deref().map(parse_filter) {
        Some(None) => return error(StatusCode::BAD_REQUEST, "only `attribute eq \"value\"` filters are supported"),
        Some(f) => f,
        None => None,
    };
    let items = d
        .users
        .iter()
        .filter(|u| match &filter {
            None => true,
            Some((a, v)) => match a.as_str() {
                "username" => u.user_name.eq_ignore_ascii_case(v),
                "externalid" => u.external_id.as_deref() == Some(v.as_str()),
                "emails" | "emails.value" => u.emails.iter().any(|e| e.eq_ignore_ascii_case(v)),
                "id" => u.id == *v,
                _ => false,
            },
        })
        .map(user_json)
        .collect();
    list(items, &q)
}

async fn create_user(headers: HeaderMap, body: axum::body::Bytes) -> Response {
    if let Err(r) = authorized(&headers) {
        return r;
    }
    let body = match body_json(&body) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let mut u = User {
        id: new_id(),
        active: true,
        ..User::default()
    };
    apply_user_fields(&mut u, &body);
    if u.user_name.trim().is_empty() {
        return error(StatusCode::BAD_REQUEST, "userName is required");
    }
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut d = load();
    if d.users.iter().any(|x| x.user_name.eq_ignore_ascii_case(&u.user_name)) {
        return error(StatusCode::CONFLICT, "a user with that userName exists");
    }
    d.users.push(u.clone());
    if let Err(e) = save(&d) {
        return error(StatusCode::INTERNAL_SERVER_ERROR, &format!("{e:#}"));
    }
    tracing::info!("scim: provisioned {}", u.user_name);
    if !u.active {
        cut_off(&u);
    }
    scim(StatusCode::CREATED, user_json(&u))
}

async fn get_user(headers: HeaderMap, Path(id): Path<String>) -> Response {
    if let Err(r) = authorized(&headers) {
        return r;
    }
    match load().users.iter().find(|u| u.id == id) {
        Some(u) => scim(StatusCode::OK, user_json(u)),
        None => error(StatusCode::NOT_FOUND, "no such user"),
    }
}

/// Changes one user under the lock; cuts them off if this deactivated them.
fn change_user(id: &str, f: impl FnOnce(&mut User) -> Result<(), String>) -> Response {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut d = load();
    let Some(u) = d.users.iter_mut().find(|u| u.id == id) else {
        return error(StatusCode::NOT_FOUND, "no such user");
    };
    let was_active = u.active;
    if let Err(e) = f(u) {
        return error(StatusCode::BAD_REQUEST, &e);
    }
    let u = u.clone();
    if let Err(e) = save(&d) {
        return error(StatusCode::INTERNAL_SERVER_ERROR, &format!("{e:#}"));
    }
    if was_active && !u.active {
        cut_off(&u);
    }
    scim(StatusCode::OK, user_json(&u))
}

async fn put_user(headers: HeaderMap, Path(id): Path<String>, body: axum::body::Bytes) -> Response {
    if let Err(r) = authorized(&headers) {
        return r;
    }
    let body = match body_json(&body) {
        Ok(b) => b,
        Err(r) => return r,
    };
    change_user(&id, |u| {
        // A PUT replaces the resource: what it leaves out goes back to the defaults.
        let mut fresh = User {
            id: u.id.clone(),
            user_name: u.user_name.clone(),
            active: true,
            ..User::default()
        };
        apply_user_fields(&mut fresh, &body);
        *u = fresh;
        Ok(())
    })
}

async fn patch_user_route(headers: HeaderMap, Path(id): Path<String>, body: axum::body::Bytes) -> Response {
    if let Err(r) = authorized(&headers) {
        return r;
    }
    let body = match body_json(&body) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let ops = body["Operations"].as_array().cloned().unwrap_or_default();
    change_user(&id, |u| patch_user(u, &ops))
}

async fn delete_user(headers: HeaderMap, Path(id): Path<String>) -> Response {
    if let Err(r) = authorized(&headers) {
        return r;
    }
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut d = load();
    let Some(i) = d.users.iter().position(|u| u.id == id) else {
        return error(StatusCode::NOT_FOUND, "no such user");
    };
    let u = d.users.remove(i);
    for g in &mut d.groups {
        g.members.retain(|m| *m != id);
    }
    if let Err(e) = save(&d) {
        return error(StatusCode::INTERNAL_SERVER_ERROR, &format!("{e:#}"));
    }
    cut_off(&u);
    StatusCode::NO_CONTENT.into_response()
}

async fn list_groups(headers: HeaderMap, Query(q): Query<ListQuery>) -> Response {
    if let Err(r) = authorized(&headers) {
        return r;
    }
    let d = load();
    let filter = match q.filter.as_deref().map(parse_filter) {
        Some(None) => return error(StatusCode::BAD_REQUEST, "only `attribute eq \"value\"` filters are supported"),
        Some(f) => f,
        None => None,
    };
    let items = d
        .groups
        .iter()
        .filter(|g| match &filter {
            None => true,
            Some((a, v)) => match a.as_str() {
                "displayname" => g.display_name.eq_ignore_ascii_case(v),
                "externalid" => g.external_id.as_deref() == Some(v.as_str()),
                "id" => g.id == *v,
                _ => false,
            },
        })
        .map(|g| group_json(g, &d))
        .collect();
    list(items, &q)
}

async fn create_group(headers: HeaderMap, body: axum::body::Bytes) -> Response {
    if let Err(r) = authorized(&headers) {
        return r;
    }
    let body = match body_json(&body) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let Some(name) = body["displayName"].as_str().filter(|n| !n.trim().is_empty()) else {
        return error(StatusCode::BAD_REQUEST, "displayName is required");
    };
    let g = Group {
        id: new_id(),
        display_name: name.to_string(),
        external_id: body["externalId"].as_str().map(String::from),
        members: member_ids(&body["members"]),
    };
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut d = load();
    if d.groups.iter().any(|x| x.display_name.eq_ignore_ascii_case(&g.display_name)) {
        return error(StatusCode::CONFLICT, "a group with that displayName exists");
    }
    d.groups.push(g.clone());
    if let Err(e) = save(&d) {
        return error(StatusCode::INTERNAL_SERVER_ERROR, &format!("{e:#}"));
    }
    scim(StatusCode::CREATED, group_json(&g, &d))
}

async fn get_group(headers: HeaderMap, Path(id): Path<String>) -> Response {
    if let Err(r) = authorized(&headers) {
        return r;
    }
    let d = load();
    match d.groups.iter().find(|g| g.id == id) {
        Some(g) => scim(StatusCode::OK, group_json(g, &d)),
        None => error(StatusCode::NOT_FOUND, "no such group"),
    }
}

fn change_group(id: &str, f: impl FnOnce(&mut Group) -> Result<(), String>) -> Response {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut d = load();
    let Some(g) = d.groups.iter_mut().find(|g| g.id == id) else {
        return error(StatusCode::NOT_FOUND, "no such group");
    };
    if let Err(e) = f(g) {
        return error(StatusCode::BAD_REQUEST, &e);
    }
    let g = g.clone();
    if let Err(e) = save(&d) {
        return error(StatusCode::INTERNAL_SERVER_ERROR, &format!("{e:#}"));
    }
    scim(StatusCode::OK, group_json(&g, &d))
}

async fn put_group(headers: HeaderMap, Path(id): Path<String>, body: axum::body::Bytes) -> Response {
    if let Err(r) = authorized(&headers) {
        return r;
    }
    let body = match body_json(&body) {
        Ok(b) => b,
        Err(r) => return r,
    };
    change_group(&id, |g| {
        if let Some(n) = body["displayName"].as_str() {
            g.display_name = n.to_string();
        }
        g.external_id = body["externalId"].as_str().map(String::from);
        g.members = member_ids(&body["members"]);
        Ok(())
    })
}

async fn patch_group_route(headers: HeaderMap, Path(id): Path<String>, body: axum::body::Bytes) -> Response {
    if let Err(r) = authorized(&headers) {
        return r;
    }
    let body = match body_json(&body) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let ops = body["Operations"].as_array().cloned().unwrap_or_default();
    change_group(&id, |g| patch_group(g, &ops))
}

async fn delete_group(headers: HeaderMap, Path(id): Path<String>) -> Response {
    if let Err(r) = authorized(&headers) {
        return r;
    }
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut d = load();
    let before = d.groups.len();
    d.groups.retain(|g| g.id != id);
    if d.groups.len() == before {
        return error(StatusCode::NOT_FOUND, "no such group");
    }
    if let Err(e) = save(&d) {
        return error(StatusCode::INTERNAL_SERVER_ERROR, &format!("{e:#}"));
    }
    StatusCode::NO_CONTENT.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters() {
        assert_eq!(
            parse_filter(r#"userName eq "ana@example.com""#),
            Some(("username".into(), "ana@example.com".into()))
        );
        assert_eq!(
            parse_filter(r#"displayName  EQ  "Eng Team""#),
            Some(("displayname".into(), "Eng Team".into()))
        );
        assert_eq!(parse_filter(r#"userName co "ana""#), None);
        assert_eq!(parse_filter("userName eq ana"), None);
        assert_eq!(parse_filter(""), None);
    }

    #[test]
    fn user_patches_including_provider_quirks() {
        let mut u = User {
            id: "1".into(),
            user_name: "ana@example.com".into(),
            active: true,
            ..User::default()
        };
        // Okta: path + bool.
        patch_user(&mut u, &[json!({"op": "replace", "path": "active", "value": false})]).unwrap();
        assert!(!u.active);
        // Entra ID: no path, value object, "True" as a string, capitalised op.
        patch_user(&mut u, &[json!({"op": "Replace", "value": {"active": "True", "displayName": "Ana"}})]).unwrap();
        assert!(u.active);
        assert_eq!(u.display_name.as_deref(), Some("Ana"));
        patch_user(&mut u, &[json!({"op": "replace", "path": "emails[type eq \"work\"].value", "value": "ana@corp.dev"})]).unwrap();
        assert_eq!(u.emails, vec!["ana@corp.dev"]);
        assert!(patch_user(&mut u, &[json!({"op": "move"})]).is_err());
    }

    #[test]
    fn group_patches() {
        let mut g = Group {
            id: "g".into(),
            display_name: "eng".into(),
            ..Group::default()
        };
        patch_group(&mut g, &[json!({"op": "add", "path": "members", "value": [{"value": "1"}, {"value": "2"}]})]).unwrap();
        assert_eq!(g.members, vec!["1", "2"]);
        patch_group(&mut g, &[json!({"op": "remove", "path": "members[value eq \"1\"]"})]).unwrap();
        assert_eq!(g.members, vec!["2"]);
        patch_group(&mut g, &[json!({"op": "replace", "value": {"displayName": "Engineering", "members": [{"value": "3"}]}})]).unwrap();
        assert_eq!((g.display_name.as_str(), g.members.clone()), ("Engineering", vec!["3".to_string()]));
        patch_group(&mut g, &[json!({"op": "remove", "path": "members"})]).unwrap();
        assert!(g.members.is_empty());
    }

    #[test]
    fn accounts_and_groups() {
        let d = Directory {
            users: vec![
                User { id: "1".into(), user_name: "Ana@example.com".into(), active: true, ..User::default() },
                User { id: "2".into(), user_name: "bob".into(), active: false, emails: vec!["bob@example.com".into()], ..User::default() },
            ],
            groups: vec![Group { id: "g".into(), display_name: "eng".into(), members: vec!["1".into()], ..Group::default() }],
        };
        assert_eq!(d.account("ana@EXAMPLE.com"), Account::Active(vec!["eng".into()]));
        assert_eq!(d.account("bob@example.com"), Account::Inactive);
        assert_eq!(d.account("carol@example.com"), Account::Unknown);
    }
}
