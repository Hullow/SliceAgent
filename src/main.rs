mod agent;
mod auth;
mod mesh;
mod pack;
mod prusa;

use axum::{
    Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, Multipart, Path as UrlPath, State},
    http::{HeaderMap,StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use chrono::Utc;
use auth::AppAuth;
use pack::{Part, Plate};
use prusa::{Preset, SliceMetrics};
use rusqlite::{Connection, params};
use serde::{Deserialize,Serialize};
use serde_json::{Value, json};
use std::{collections::HashMap, fs::{self,OpenOptions}, io::{Cursor,Write}, path::{Path, PathBuf}, sync::{Arc, Mutex}};
use tokio::task;
use tower_http::services::ServeDir;
use uuid::Uuid;

#[derive(Clone)]
struct AppState {
    db: Arc<Mutex<Connection>>,
    data_dir: PathBuf,
    slicer: Option<PathBuf>,
    client: reqwest::Client,
    auth: Arc<AppAuth>,
}

type ApiResult = Result<Json<Value>, (StatusCode, String)>;
fn bad(message: impl ToString) -> (StatusCode, String) { (StatusCode::BAD_REQUEST, message.to_string()) }
fn internal(message: impl ToString) -> (StatusCode, String) { (StatusCode::INTERNAL_SERVER_ERROR, message.to_string()) }
fn not_found(message: impl ToString) -> (StatusCode, String) { (StatusCode::NOT_FOUND, message.to_string()) }
fn id() -> String { Uuid::new_v4().to_string() }
fn now() -> String { Utc::now().to_rfc3339() }

fn default_provider()->String {"openai".into()}
fn default_model(provider:&str)->&'static str {match provider {"anthropic"=>"claude-sonnet-4-6","openrouter"=>"anthropic/claude-sonnet-4.6",_=>"gpt-6-astra"}}
fn provider_env_key(provider:&str)->&'static str {match provider {"anthropic"=>"ANTHROPIC_API_KEY","openrouter"=>"OPENROUTER_API_KEY",_=>"OPENAI_API_KEY"}}
fn provider_env_model(provider:&str)->&'static str {match provider {"anthropic"=>"ANTHROPIC_MODEL","openrouter"=>"OPENROUTER_MODEL",_=>"OPENAI_MODEL"}}
fn provider_label(provider:&str)->&'static str {match provider {"anthropic"=>"Anthropic","openrouter"=>"OpenRouter",_=>"OpenAI"}}
fn validate_provider(provider:&str)->Result<(),String> {if matches!(provider,"openai"|"anthropic"|"openrouter") {Ok(())}else{Err("Choose OpenAI, Anthropic, or OpenRouter".into())}}

#[derive(Clone,Serialize,Deserialize)]
struct StoredLlm { #[serde(default="default_provider")] provider:String, model:String, api_key:Option<String> }

fn llm_file(data_dir:&Path)->PathBuf {data_dir.join("llm_config.json")}

fn saved_llm(data_dir:&Path)->Result<Option<StoredLlm>,String> {
    match fs::read(llm_file(data_dir)) {
        Ok(bytes)=>serde_json::from_slice(&bytes).map(Some).map_err(|_|"Saved LLM settings could not be read".into()),
        Err(error) if error.kind()==std::io::ErrorKind::NotFound=>Ok(None),
        Err(error)=>Err(format!("Cannot read LLM settings: {error}")),
    }
}

fn active_llm(data_dir:&Path)->Result<Option<(agent::LlmConfig,&'static str)>,String> {
    let saved=saved_llm(data_dir)?;
    let provider=saved.as_ref().map(|config|config.provider.as_str()).unwrap_or_else(||if std::env::var("OPENROUTER_API_KEY").ok().is_some_and(|key|!key.trim().is_empty()) {"openrouter"}else if std::env::var("OPENAI_API_KEY").ok().is_some_and(|key|!key.trim().is_empty()) {"openai"}else if std::env::var("ANTHROPIC_API_KEY").ok().is_some_and(|key|!key.trim().is_empty()) {"anthropic"}else{"openai"});
    validate_provider(provider)?;
    let file_key=saved.as_ref().and_then(|config|config.api_key.as_deref()).filter(|key|!key.trim().is_empty());
    let env_key=std::env::var(provider_env_key(provider)).ok().filter(|key|!key.trim().is_empty());
    let Some(key)=file_key.map(str::to_string).or(env_key) else {return Ok(None)};
    let model=saved.as_ref().map(|config|config.model.clone()).filter(|model|!model.is_empty()).or_else(||std::env::var(provider_env_model(provider)).ok()).unwrap_or_else(||default_model(provider).into());
    Ok(Some((agent::LlmConfig{provider:provider.into(),api_key:key,model},if file_key.is_some(){"saved"}else{"environment"})))
}

fn validate_model(model:&str)->Result<(),String> {
    if model.is_empty()||model.len()>120||!model.chars().all(|character|character.is_ascii_alphanumeric()||matches!(character,'-'|'_'|'.'|':'|'/'|'~')) {return Err("Enter a valid model ID".into());}
    Ok(())
}

fn write_llm_file(data_dir:&Path,settings:&StoredLlm)->Result<(),String> {
    let path=llm_file(data_dir);
    let temporary=data_dir.join(format!(".llm-{}.tmp",id()));
    let mut options=OpenOptions::new();options.write(true).create_new(true);
    #[cfg(unix)] {use std::os::unix::fs::OpenOptionsExt;options.mode(0o600);}
    let mut file=options.open(&temporary).map_err(|e|e.to_string())?;
    let bytes=serde_json::to_vec(settings).map_err(|e|e.to_string())?;
    let saved=file.write_all(&bytes).and_then(|_|file.sync_all()).and_then(|_|fs::rename(&temporary,&path));
    if saved.is_err(){let _=fs::remove_file(&temporary);}
    saved.map_err(|e|e.to_string())
}

#[derive(Deserialize)] struct LlmConfigPatch {#[serde(default="default_provider")] provider:String,model:String,api_key:Option<String>}
async fn save_llm_config(State(state):State<AppState>,Json(payload):Json<LlmConfigPatch>)->ApiResult {
    validate_provider(&payload.provider).map_err(bad)?;
    let model=payload.model.trim().to_string();validate_model(&model).map_err(bad)?;
    let existing=saved_llm(&state.data_dir).map_err(internal)?;
    let key=payload.api_key.filter(|key|!key.trim().is_empty()).or_else(||existing.as_ref().filter(|config|config.provider==payload.provider).and_then(|config|config.api_key.clone()));
    if key.is_none()&&std::env::var(provider_env_key(&payload.provider)).ok().is_none_or(|key|key.trim().is_empty()) {return Err(bad(format!("Enter an {} API key",provider_label(&payload.provider))));}
    write_llm_file(&state.data_dir,&StoredLlm{provider:payload.provider.clone(),model:model.clone(),api_key:key}).map_err(internal)?;
    Ok(Json(json!({"ok":true,"provider":payload.provider,"model":model})))
}

async fn delete_llm_config(State(state):State<AppState>)->ApiResult {
    match fs::remove_file(llm_file(&state.data_dir)) {
        Ok(())=>{},Err(error) if error.kind()==std::io::ErrorKind::NotFound=>{},Err(error)=>return Err(internal(error)),
    }
    Ok(Json(json!({"ok":true})))
}

#[derive(Deserialize)] struct LlmTest {provider:Option<String>,model:Option<String>,api_key:Option<String>}
async fn test_llm_connection(State(state):State<AppState>,Json(payload):Json<LlmTest>)->ApiResult {
    let active=active_llm(&state.data_dir).map_err(internal)?;
    let provider=payload.provider.unwrap_or_else(||active.as_ref().map(|(config,_)|config.provider.clone()).unwrap_or_else(default_provider));
    validate_provider(&provider).map_err(bad)?;
    let model=payload.model.or_else(||active.as_ref().filter(|(config,_)|config.provider==provider).map(|(config,_)|config.model.clone())).unwrap_or_else(||default_model(&provider).into());
    validate_model(&model).map_err(bad)?;
    let key=payload.api_key.filter(|key|!key.trim().is_empty()).or_else(||active.filter(|(config,_)|config.provider==provider).map(|(config,_)|config.api_key)).or_else(||std::env::var(provider_env_key(&provider)).ok().filter(|key|!key.trim().is_empty())).ok_or_else(||bad("Enter an API key for the selected provider"))?;
    let config=agent::LlmConfig{provider:provider.clone(),api_key:key,model:model.clone()};
    agent::complete(&state.client,&config,"Reply OK.",true).await.map_err(bad)?;
    Ok(Json(json!({"ok":true,"provider":provider,"model":model})))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "sliceagent=info,tower_http=info".into())).init();
    let addr = std::env::var("SLICER_AGENT_BIND").unwrap_or_else(|_| "127.0.0.1:8765".into());
    let socket_addr:std::net::SocketAddr=addr.parse()?;
    if !socket_addr.ip().is_loopback(){return Err("SliceAgent must bind to a loopback address; put HTTPS Caddy in front of it".into());}
    let public_url=std::env::var("SLICER_AGENT_PUBLIC_URL").ok().filter(|value|!value.is_empty());
    if let Some(url)=&public_url {
        let parsed=reqwest::Url::parse(url)?;
        if parsed.scheme()!="https"||parsed.host_str().is_none()||parsed.password().is_some()||!parsed.username().is_empty()||parsed.path()!="/"||parsed.query().is_some() {return Err("SLICER_AGENT_PUBLIC_URL must be an HTTPS origin, for example https://print.example.com".into());}
    }
    let data_dir = PathBuf::from(std::env::var("SLICER_AGENT_DATA").unwrap_or_else(|_| "data".into()));
    fs::create_dir_all(&data_dir)?;
    #[cfg(unix)] {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&data_dir,fs::Permissions::from_mode(0o700))?;
    }
    fs::create_dir_all(data_dir.join("assets"))?;
    fs::create_dir_all(data_dir.join("context"))?;
    fs::create_dir_all(data_dir.join("jobs"))?;
    fs::create_dir_all(data_dir.join("presets"))?;
    let db = Connection::open(data_dir.join("agent.sqlite"))?;
    init_db(&db)?;
    let auth=Arc::new(AppAuth::new(&data_dir,public_url.is_some()).map_err(std::io::Error::other)?);
    let state = AppState { db: Arc::new(Mutex::new(db)), data_dir, slicer: prusa::find_slicer(), client: reqwest::Client::builder().user_agent("SlicerAgent/0.1").timeout(std::time::Duration::from_secs(45)).build()?, auth };
    let pending_jobs={
        let db=state.db.lock().unwrap();
        rows(&db,"SELECT id FROM jobs WHERE status IN ('queued','interpreting','slicing') ORDER BY created_at",&[])?
            .into_iter().filter_map(|row|row["id"].as_str().map(str::to_string)).collect::<Vec<_>>()
    };
    for job_id in pending_jobs { let state_clone=state.clone();tokio::spawn(async move {process_job(state_clone,job_id).await;}); }
    let protected_api = Router::new()
        .route("/state", get(get_state))
        .route("/settings", axum::routing::patch(update_global_settings))
        .route("/llm/config", post(save_llm_config).delete(delete_llm_config))
        .route("/llm/test", post(test_llm_connection))
        .route("/projects", post(create_project))
        .route("/projects/{id}", get(get_project).patch(update_project))
        .route("/projects/{id}/context", axum::routing::patch(update_project_context).post(upload_project_context).layer(DefaultBodyLimit::max(1024 * 1024)))
        .route("/projects/{id}/assets", post(upload_asset).layer(DefaultBodyLimit::max(100 * 1024 * 1024)))
        .route("/projects/{id}/import", post(import_repository))
        .route("/presets", post(create_preset))
        .route("/presets/{id}", axum::routing::patch(update_preset))
        .route("/presets/{id}/ini", post(upload_preset_ini).layer(DefaultBodyLimit::max(5 * 1024 * 1024)))
        .route("/filament-prices", post(set_filament_price))
        .route("/conversations", post(create_conversation))
        .route("/conversations/{id}/settings", axum::routing::patch(update_conversation_settings))
        .route("/conversations/{id}", get(get_conversation))
        .route("/conversations/{id}/context", axum::routing::patch(update_conversation_context).post(upload_conversation_context).layer(DefaultBodyLimit::max(1024 * 1024)))
        .route("/conversations/{id}/messages", post(send_message))
        .route("/jobs/{id}", get(get_job))
        .route("/download/{job}/{file}", get(download))
        .layer(middleware::from_fn_with_state(state.clone(), authenticate));
    let api=Router::new()
        .route("/auth/login",post(login))
        .route("/auth/status",get(auth_status))
        .route("/auth/logout",post(logout))
        .merge(protected_api)
        .with_state(state.clone());
    let app = Router::new().nest("/api", api).fallback_service(ServeDir::new("web").append_index_html_on_directories(true)).layer(middleware::from_fn(security_headers));
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("SliceAgent listening at http://{addr}; slicer={:?}", state.slicer);
    axum::serve(listener, app).await?;
    Ok(())
}

fn mutation_allowed(headers:&HeaderMap)->bool {
    if headers.get("x-sliceagent-request").and_then(|v|v.to_str().ok())!=Some("1") {return false;}
    let Some(origin)=headers.get(header::ORIGIN).and_then(|v|v.to_str().ok()) else {return true};
    let Some(host)=headers.get(header::HOST).and_then(|v|v.to_str().ok()).and_then(|v|v.parse::<axum::http::uri::Authority>().ok()) else {return false};
    let Ok(origin)=reqwest::Url::parse(origin) else {return false};
    let request_port=host.port_u16().unwrap_or(if origin.scheme()=="https" {443}else{80});
    origin.host_str().is_some_and(|name|name.trim_matches(|c|c=='['||c==']').eq_ignore_ascii_case(host.host().trim_matches(|c|c=='['||c==']')))
        && origin.port_or_known_default()==Some(request_port)
}

async fn authenticate(State(state):State<AppState>,request:axum::extract::Request,next:Next)->Response {
    if !state.auth.authenticated(request.headers()){return (StatusCode::UNAUTHORIZED,"Sign in to SliceAgent").into_response();}
    if !matches!(*request.method(),axum::http::Method::GET|axum::http::Method::HEAD|axum::http::Method::OPTIONS)&&!mutation_allowed(request.headers()) {return (StatusCode::FORBIDDEN,"Request origin check failed").into_response();}
    next.run(request).await
}

#[derive(Deserialize)] struct LoginRequest {token:String}
async fn login(State(state):State<AppState>,headers:HeaderMap,Json(request):Json<LoginRequest>)->Result<Response,(StatusCode,String)> {
    if !mutation_allowed(&headers){return Err((StatusCode::FORBIDDEN,"Request origin check failed".into()));}
    if !state.auth.verify_token(&request.token){return Err((StatusCode::UNAUTHORIZED,"Invalid access token".into()));}
    Ok(([(header::SET_COOKIE,state.auth.login_cookie())],Json(json!({"ok":true}))).into_response())
}

async fn auth_status(State(state):State<AppState>,headers:HeaderMap)->ApiResult {
    Ok(Json(json!({"authenticated":state.auth.authenticated(&headers)})))
}

async fn logout(State(state):State<AppState>,headers:HeaderMap)->Result<Response,(StatusCode,String)> {
    if !mutation_allowed(&headers){return Err((StatusCode::FORBIDDEN,"Request origin check failed".into()));}
    Ok(([(header::SET_COOKIE,state.auth.logout_cookie(&headers))],Json(json!({"ok":true}))).into_response())
}

async fn security_headers(request:axum::extract::Request,next:Next)->Response {
    let api=request.uri().path().starts_with("/api/");
    let mut response=next.run(request).await;
    let headers=response.headers_mut();
    headers.insert("x-content-type-options","nosniff".parse().unwrap());
    headers.insert("referrer-policy","no-referrer".parse().unwrap());
    headers.insert("x-frame-options","DENY".parse().unwrap());
    headers.insert("permissions-policy","camera=(), microphone=(), geolocation=()".parse().unwrap());
    headers.insert("content-security-policy","default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' blob:; connect-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'".parse().unwrap());
    if api {headers.insert(header::CACHE_CONTROL,"no-store".parse().unwrap());}
    response
}

fn init_db(db: &Connection) -> rusqlite::Result<()> {
    db.execute_batch("PRAGMA foreign_keys=ON; PRAGMA journal_mode=WAL;
      CREATE TABLE IF NOT EXISTS presets(id TEXT PRIMARY KEY,name TEXT NOT NULL,printer TEXT NOT NULL,printer_profile TEXT NOT NULL,print_profile TEXT NOT NULL,filament_profile TEXT NOT NULL,ini_path TEXT,bed_width REAL NOT NULL,bed_depth REAL NOT NULL,gap REAL NOT NULL,location TEXT NOT NULL DEFAULT '',settings_json TEXT,created_at TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS filament_prices(location TEXT NOT NULL,filament_profile TEXT NOT NULL,chf_per_kg REAL NOT NULL,PRIMARY KEY(location,filament_profile));
      CREATE TABLE IF NOT EXISTS app_settings(id INTEGER PRIMARY KEY CHECK(id=1),default_preset_id TEXT,default_email TEXT);
      CREATE TABLE IF NOT EXISTS projects(id TEXT PRIMARY KEY,name TEXT NOT NULL,default_preset_id TEXT,price_unit TEXT NOT NULL DEFAULT 'g',price_rate REAL NOT NULL DEFAULT 0,email TEXT,default_max_minutes REAL NOT NULL DEFAULT 240,context_text TEXT NOT NULL DEFAULT '',created_at TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS conversations(id TEXT PRIMARY KEY,project_id TEXT NOT NULL,title TEXT NOT NULL,context_text TEXT NOT NULL DEFAULT '',preset_id TEXT,max_minutes REAL,max_grams REAL,email TEXT,created_at TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS context_documents(scope TEXT NOT NULL,scope_id TEXT NOT NULL,name TEXT NOT NULL,path TEXT NOT NULL,created_at TEXT NOT NULL,PRIMARY KEY(scope,scope_id));
      CREATE TABLE IF NOT EXISTS messages(id TEXT PRIMARY KEY,conversation_id TEXT NOT NULL,role TEXT NOT NULL,content TEXT NOT NULL,created_at TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS assets(id TEXT PRIMARY KEY,project_id TEXT NOT NULL,name TEXT NOT NULL,kind TEXT NOT NULL,path TEXT NOT NULL,source TEXT,info_json TEXT,created_at TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS jobs(id TEXT PRIMARY KEY,project_id TEXT NOT NULL,conversation_id TEXT NOT NULL,prompt TEXT NOT NULL,status TEXT NOT NULL,intent_json TEXT,result_json TEXT,progress_json TEXT,error TEXT,created_at TEXT NOT NULL,updated_at TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS usage(id TEXT PRIMARY KEY,project_id TEXT NOT NULL,job_id TEXT NOT NULL,model TEXT NOT NULL,input_tokens INTEGER NOT NULL,output_tokens INTEGER NOT NULL,cached_tokens INTEGER NOT NULL,cost_usd REAL,created_at TEXT NOT NULL);
      CREATE INDEX IF NOT EXISTS idx_assets_project ON assets(project_id); CREATE INDEX IF NOT EXISTS idx_jobs_project ON jobs(project_id); CREATE INDEX IF NOT EXISTS idx_messages_conversation ON messages(conversation_id);")?;
    let _ = db.execute("ALTER TABLE jobs ADD COLUMN progress_json TEXT", []);
    let _ = db.execute("ALTER TABLE projects ADD COLUMN default_max_minutes REAL NOT NULL DEFAULT 240", []);
    let _ = db.execute("ALTER TABLE projects ADD COLUMN context_text TEXT NOT NULL DEFAULT ''", []);
    let _ = db.execute("ALTER TABLE conversations ADD COLUMN context_text TEXT NOT NULL DEFAULT ''", []);
    let _ = db.execute("ALTER TABLE presets ADD COLUMN location TEXT NOT NULL DEFAULT ''", []);
    let _ = db.execute("ALTER TABLE presets ADD COLUMN settings_json TEXT", []);
    for column in ["preset_id TEXT","max_minutes REAL","max_grams REAL","email TEXT"] {let _=db.execute(&format!("ALTER TABLE conversations ADD COLUMN {column}"),[]);}
    let count: i64 = db.query_row("SELECT COUNT(*) FROM presets", [], |r| r.get(0))?;
    if count == 0 {
        let seeded = [
            ("MK4S · PLA · 0.20 structural", "MK4S", "Original Prusa MK4S HF0.4 nozzle", "0.20mm STRUCTURAL @MK4S 0.4", "Generic PLA @MK4S HF0.4", 250.0,210.0),
            ("CORE One · PLA · 0.20 structural", "CORE One", "Prusa CORE One 0.4 HF", "0.20mm STRUCTURAL @CORE One 0.4", "Generic PLA @CORE One 0.4", 250.0,220.0),
        ];
        for (name,printer,printer_profile,print_profile,filament_profile,w,h) in seeded {
            db.execute("INSERT INTO presets(id,name,printer,printer_profile,print_profile,filament_profile,bed_width,bed_depth,gap,created_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,5,?9)",params![id(),name,printer,printer_profile,print_profile,filament_profile,w,h,now()])?;
        }
    }
    db.execute("INSERT OR IGNORE INTO app_settings(id,default_preset_id) SELECT 1,id FROM presets WHERE printer='MK4S' LIMIT 1",[])?;
    Ok(())
}

fn preset_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Preset> {
    Ok(Preset { id:row.get(0)?,name:row.get(1)?,printer:row.get(2)?,printer_profile:row.get(3)?,print_profile:row.get(4)?,filament_profile:row.get(5)?,ini_path:row.get(6)?,bed_width:row.get(7)?,bed_depth:row.get(8)?,gap:row.get(9)?,location:row.get(10)?,settings_json:row.get(11)? })
}

fn load_preset(db: &Connection, preset_id: &str) -> Result<Preset, String> {
    db.query_row("SELECT id,name,printer,printer_profile,print_profile,filament_profile,ini_path,bed_width,bed_depth,gap,location,settings_json FROM presets WHERE id=?1",[preset_id],preset_from_row).map_err(|e|e.to_string())
}

fn rows(db: &Connection, sql: &str, args: &[&dyn rusqlite::ToSql]) -> rusqlite::Result<Vec<Value>> {
    let mut stmt = db.prepare(sql)?;
    let names: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
    let values = stmt.query_map(args, |row| {
        let mut obj = serde_json::Map::new();
        for (i,name) in names.iter().enumerate() {
            let value: Value = match row.get_ref(i)? {
                rusqlite::types::ValueRef::Null => Value::Null,
                rusqlite::types::ValueRef::Integer(x) => json!(x),
                rusqlite::types::ValueRef::Real(x) => json!(x),
                rusqlite::types::ValueRef::Text(x) => json!(String::from_utf8_lossy(x).to_string()),
                rusqlite::types::ValueRef::Blob(_) => Value::Null,
            };
            obj.insert(name.clone(),value);
        }
        Ok(Value::Object(obj))
    })?;
    values.collect()
}

async fn get_state(State(state): State<AppState>) -> ApiResult {
    let llm=active_llm(&state.data_dir).map_err(internal)?;
    let saved_config=saved_llm(&state.data_dir).map_err(internal)?;
    let llm_public=llm.as_ref().map(|(config,source)|json!({"configured":true,"provider":config.provider,"model":config.model,"source":source,"saved_configuration":saved_config.is_some()})).unwrap_or_else(||{
        let provider=saved_config.as_ref().map(|config|config.provider.as_str()).unwrap_or("openai");
        json!({"configured":false,"provider":provider,"model":saved_config.as_ref().map(|config|config.model.clone()).or_else(||std::env::var(provider_env_model(provider)).ok()).unwrap_or_else(||default_model(provider).into()),"source":null,"saved_configuration":saved_config.is_some()})
    });
    let db = state.db.lock().unwrap();
    let projects = rows(&db,"SELECT id,name,default_preset_id,price_unit,price_rate,email,default_max_minutes,created_at FROM projects ORDER BY created_at DESC",&[]).map_err(internal)?;
    let presets = rows(&db,"SELECT id,name,printer,printer_profile,print_profile,filament_profile,ini_path,bed_width,bed_depth,gap,location,settings_json FROM presets ORDER BY location,printer,name",&[]).map_err(internal)?;
    let filament_prices=rows(&db,"SELECT location,filament_profile,chf_per_kg FROM filament_prices ORDER BY location,filament_profile",&[]).map_err(internal)?;
    let settings=rows(&db,"SELECT default_preset_id,default_email FROM app_settings WHERE id=1",&[]).map_err(internal)?.into_iter().next();
    Ok(Json(json!({"projects":projects,"presets":presets,"filament_prices":filament_prices,"settings":settings,"slicer_available":state.slicer.is_some(),"llm_available":llm.is_some(),"llm":llm_public,"email_available":email_configured()})))
}

#[derive(Deserialize)] struct GlobalSettingsPatch { default_preset_id:Option<String>,default_email:Option<String> }
async fn update_global_settings(State(state):State<AppState>,Json(payload):Json<GlobalSettingsPatch>) -> ApiResult {
    let db=state.db.lock().unwrap();
    if let Some(preset)=payload.default_preset_id {load_preset(&db,&preset).map_err(bad)?;db.execute("UPDATE app_settings SET default_preset_id=?1 WHERE id=1",[preset]).map_err(internal)?;}
    if let Some(email)=payload.default_email {let value=(!email.trim().is_empty()).then_some(email.trim());db.execute("UPDATE app_settings SET default_email=?1 WHERE id=1",[value]).map_err(internal)?;}
    Ok(Json(json!({"ok":true})))
}

#[derive(Deserialize)] struct NewProject { name: String }
async fn create_project(State(state):State<AppState>,Json(payload):Json<NewProject>) -> ApiResult {
    let name=payload.name.trim(); if name.is_empty() {return Err(bad("Project name is required"));}
    let db=state.db.lock().unwrap();
    let project_id=id();
    db.execute("INSERT INTO projects(id,name,created_at) VALUES (?1,?2,?3)",params![project_id,name,now()]).map_err(internal)?;
    Ok(Json(json!({"id":project_id})))
}

async fn get_project(State(state):State<AppState>,UrlPath(project_id):UrlPath<String>) -> ApiResult {
    let db=state.db.lock().unwrap();
    let project=rows(&db,"SELECT id,name,default_preset_id,price_unit,price_rate,email,default_max_minutes,context_text,created_at,COALESCE(default_preset_id,(SELECT default_preset_id FROM app_settings WHERE id=1)) AS effective_preset_id,COALESCE(email,(SELECT default_email FROM app_settings WHERE id=1)) AS effective_email FROM projects WHERE id=?1",&[&project_id]).map_err(internal)?.into_iter().next().ok_or_else(||not_found("Project not found"))?;
    let context_document=rows(&db,"SELECT name,created_at FROM context_documents WHERE scope='project' AND scope_id=?1",&[&project_id]).map_err(internal)?.into_iter().next();
    let assets=rows(&db,"SELECT id,name,kind,source,info_json,created_at FROM assets WHERE project_id=?1 ORDER BY created_at DESC",&[&project_id]).map_err(internal)?;
    let conversations=rows(&db,"SELECT id,title,created_at FROM conversations WHERE project_id=?1 ORDER BY created_at DESC",&[&project_id]).map_err(internal)?;
    let jobs=rows(&db,"SELECT id,conversation_id,prompt,status,result_json,progress_json,error,created_at FROM jobs WHERE project_id=?1 ORDER BY created_at DESC LIMIT 30",&[&project_id]).map_err(internal)?;
    let usage=rows(&db,"SELECT model,SUM(input_tokens) AS input_tokens,SUM(output_tokens) AS output_tokens,SUM(cached_tokens) AS cached_tokens,SUM(cost_usd) AS cost_usd FROM usage WHERE project_id=?1 GROUP BY model",&[&project_id]).map_err(internal)?;
    Ok(Json(json!({"project":project,"context_document":context_document,"assets":assets,"conversations":conversations,"jobs":jobs,"usage":usage})))
}

#[derive(Deserialize)] struct ProjectPatch { name:Option<String>,default_preset_id:Option<String>,inherit_preset:Option<bool>,price_unit:Option<String>,price_rate:Option<f64>,email:Option<String>,inherit_email:Option<bool>,default_max_minutes:Option<f64> }
async fn update_project(State(state):State<AppState>,UrlPath(project_id):UrlPath<String>,Json(payload):Json<ProjectPatch>) -> ApiResult {
    if let Some(unit)=&payload.price_unit {if unit!="g" && unit!="m" {return Err(bad("Price unit must be g or m"));}}
    if let Some(rate)=payload.price_rate {if !rate.is_finite() || rate<0.0 {return Err(bad("Price rate must be non-negative"));}}
    if let Some(minutes)=payload.default_max_minutes {if !minutes.is_finite() || minutes<=0.0 {return Err(bad("Default plate time must be positive"));}}
    let db=state.db.lock().unwrap();
    if let Some(name)=payload.name {db.execute("UPDATE projects SET name=?1 WHERE id=?2",params![name,project_id]).map_err(internal)?;}
    if let Some(preset)=payload.default_preset_id {load_preset(&db,&preset).map_err(bad)?;db.execute("UPDATE projects SET default_preset_id=?1 WHERE id=?2",params![preset,project_id]).map_err(internal)?;}
    if payload.inherit_preset==Some(true) {db.execute("UPDATE projects SET default_preset_id=NULL WHERE id=?1",[&project_id]).map_err(internal)?;}
    if let Some(unit)=payload.price_unit {db.execute("UPDATE projects SET price_unit=?1 WHERE id=?2",params![unit,project_id]).map_err(internal)?;}
    if let Some(rate)=payload.price_rate {db.execute("UPDATE projects SET price_rate=?1 WHERE id=?2",params![rate,project_id]).map_err(internal)?;}
    if let Some(email)=payload.email {db.execute("UPDATE projects SET email=?1 WHERE id=?2",params![email,project_id]).map_err(internal)?;}
    if payload.inherit_email==Some(true) {db.execute("UPDATE projects SET email=NULL WHERE id=?1",[&project_id]).map_err(internal)?;}
    if let Some(minutes)=payload.default_max_minutes {db.execute("UPDATE projects SET default_max_minutes=?1 WHERE id=?2",params![minutes,project_id]).map_err(internal)?;}
    Ok(Json(json!({"ok":true})))
}

#[derive(Deserialize)] struct ContextPatch { text:String }
fn set_context_text(state:&AppState,scope:&str,scope_id:&str,text:&str) -> ApiResult {
    if text.len()>100_000 {return Err(bad("Context text must be below 100 KB"));}
    let db=state.db.lock().unwrap();
    let table=if scope=="project" {"projects"} else {"conversations"};
    let changed=db.execute(&format!("UPDATE {table} SET context_text=?1 WHERE id=?2"),params![text,scope_id]).map_err(internal)?;
    if changed==0 {return Err(not_found("Project or chat not found"));}
    Ok(Json(json!({"ok":true})))
}
async fn update_project_context(State(state):State<AppState>,UrlPath(scope_id):UrlPath<String>,Json(p):Json<ContextPatch>) -> ApiResult {set_context_text(&state,"project",&scope_id,&p.text)}
async fn update_conversation_context(State(state):State<AppState>,UrlPath(scope_id):UrlPath<String>,Json(p):Json<ContextPatch>) -> ApiResult {set_context_text(&state,"conversation",&scope_id,&p.text)}

async fn save_context_file(state:&AppState,scope:&str,scope_id:&str,mut multipart:Multipart) -> ApiResult {
    let table=if scope=="project" {"projects"} else {"conversations"};
    {
        let db=state.db.lock().unwrap();
        let exists:bool=db.query_row(&format!("SELECT EXISTS(SELECT 1 FROM {table} WHERE id=?1)"),[scope_id],|r|r.get(0)).map_err(internal)?;
        if !exists {return Err(not_found("Project or chat not found"));}
    }
    let field=multipart.next_field().await.map_err(bad)?.ok_or_else(||bad("No context file received"))?;
    let name=Path::new(field.file_name().unwrap_or("context.md")).file_name().and_then(|n|n.to_str()).ok_or_else(||bad("Invalid filename"))?.to_string();
    if !name.to_ascii_lowercase().ends_with(".md") && !name.to_ascii_lowercase().ends_with(".txt") {return Err(bad("Upload a Markdown or text file"));}
    let bytes=field.bytes().await.map_err(bad)?;
    if bytes.len()>500_000 {return Err(bad("Context file must be below 500 KB"));}
    std::str::from_utf8(&bytes).map_err(bad)?;
    let path=state.data_dir.join("context").join(format!("{}-{}.md",scope,id()));
    fs::write(&path,&bytes).map_err(internal)?;
    let db=state.db.lock().unwrap();
    db.execute("INSERT INTO context_documents(scope,scope_id,name,path,created_at) VALUES (?1,?2,?3,?4,?5) ON CONFLICT(scope,scope_id) DO UPDATE SET name=excluded.name,path=excluded.path,created_at=excluded.created_at",params![scope,scope_id,name,path.to_string_lossy(),now()]).map_err(internal)?;
    Ok(Json(json!({"ok":true,"name":name})))
}
async fn upload_project_context(State(state):State<AppState>,UrlPath(scope_id):UrlPath<String>,multipart:Multipart) -> ApiResult {save_context_file(&state,"project",&scope_id,multipart).await}
async fn upload_conversation_context(State(state):State<AppState>,UrlPath(scope_id):UrlPath<String>,multipart:Multipart) -> ApiResult {save_context_file(&state,"conversation",&scope_id,multipart).await}

fn save_asset(state:&AppState,project_id:&str,name:&str,bytes:&[u8],source:Option<&str>) -> Result<String,String> {
    if bytes.len()>50*1024*1024 {return Err("Individual files must be below 50 MB".into());}
    let lower=name.to_ascii_lowercase();
    if !lower.ends_with(".stl") && !lower.ends_with(".3mf") {return Err("Only STL and 3MF model files are accepted".into());}
    let filename=Path::new(name).file_name().and_then(|s|s.to_str()).ok_or("Invalid filename")?;
    let asset_id=id();
    let kind=if lower.ends_with(".stl") {"stl"} else {"3mf"};
    let path=state.data_dir.join("assets").join(format!("{asset_id}.{kind}"));
    fs::write(&path,bytes).map_err(|e|e.to_string())?;
    let info=if kind=="stl" {match mesh::Mesh::read(&path) {Ok(m)=>serde_json::to_string(&m.info()).ok(),Err(e)=>Some(json!({"warnings":[e]}).to_string())}} else {None};
    let db=state.db.lock().unwrap();
    db.execute("INSERT INTO assets(id,project_id,name,kind,path,source,info_json,created_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",params![asset_id,project_id,filename,kind,path.to_string_lossy(),source,info,now()]).map_err(|e|e.to_string())?;
    Ok(asset_id)
}

async fn upload_asset(State(state):State<AppState>,UrlPath(project_id):UrlPath<String>,mut multipart:Multipart) -> ApiResult {
    let exists:bool={let db=state.db.lock().unwrap(); db.query_row("SELECT EXISTS(SELECT 1 FROM projects WHERE id=?1)",[&project_id],|r|r.get(0)).map_err(internal)?};
    if !exists {return Err(not_found("Project not found"));}
    let mut saved=Vec::new();
    while let Some(field)=multipart.next_field().await.map_err(bad)? {
        let name=field.file_name().unwrap_or("model.stl").to_string();
        let bytes=field.bytes().await.map_err(bad)?;
        saved.push(save_asset(&state,&project_id,&name,&bytes,None).map_err(bad)?);
    }
    if saved.is_empty(){return Err(bad("No files received"));}
    Ok(Json(json!({"asset_ids":saved})))
}

#[derive(Deserialize)] struct ImportRequest { url:String }
async fn import_repository(State(state):State<AppState>,UrlPath(project_id):UrlPath<String>,Json(payload):Json<ImportRequest>) -> ApiResult {
    let saved=fetch_github_assets(&state,&project_id,&payload.url).await.map_err(bad)?;
    Ok(Json(json!({"asset_ids":saved})))
}

async fn fetch_github_assets(state:&AppState,project_id:&str,url:&str) -> Result<Vec<String>,String> {
    let parsed=reqwest::Url::parse(url).map_err(|e|e.to_string())?;
    if parsed.scheme()!="https" || parsed.host_str()!=Some("github.com") {return Err("First version imports public GitHub repositories via HTTPS".into());}
    let segments:Vec<_>=parsed.path_segments().ok_or("Invalid GitHub URL")?.collect();
    if segments.len()<2 {return Err("Use a repository URL such as https://github.com/owner/repo".into());}
    let owner=segments[0];let repo=segments[1].trim_end_matches(".git");
    if owner.is_empty() || repo.is_empty() || !owner.chars().all(|c|c.is_ascii_alphanumeric()||c=='-'||c=='_') || !repo.chars().all(|c|c.is_ascii_alphanumeric()||c=='-'||c=='_'||c=='.') {return Err("Invalid repository name".into());}
    let archive_url=format!("https://api.github.com/repos/{owner}/{repo}/zipball");
    let response=state.client.get(archive_url).header("Accept","application/vnd.github+json").send().await.map_err(|e|e.to_string())?;
    if !response.status().is_success() {return Err(format!("Repository download failed: {}",response.status()));}
    if response.content_length().is_some_and(|n|n>100*1024*1024) {return Err("Repository archive is over 100 MB".into());}
    let bytes=response.bytes().await.map_err(|e|e.to_string())?;
    if bytes.len()>100*1024*1024 {return Err("Repository archive is over 100 MB".into());}
    let mut archive=zip::ZipArchive::new(Cursor::new(bytes)).map_err(|e|e.to_string())?;
    let mut saved=Vec::new();
    for i in 0..archive.len() {
        if saved.len()>=100 {break;}
        let mut entry=archive.by_index(i).map_err(|e|e.to_string())?;
        let entry_name=entry.name().to_string();
        if !entry_name.to_ascii_lowercase().ends_with(".stl") || entry.is_dir() {continue;}
        if entry.size()>50*1024*1024 {continue;}
        let mut file=Vec::new();
        std::io::Read::read_to_end(&mut entry,&mut file).map_err(|e|e.to_string())?;
        if file.len()>50*1024*1024 {continue;}
        let name=Path::new(&entry_name).file_name().and_then(|s|s.to_str()).ok_or("Invalid archive filename")?;
        saved.push(save_asset(state,project_id,name,&file,Some(url))?);
    }
    if saved.is_empty(){return Err("No STL files found in the repository".into());}
    Ok(saved)
}

#[derive(Deserialize)] struct NewPreset { name:String,printer:String,printer_profile:String,print_profile:String,filament_profile:String,bed_width:f64,bed_depth:f64,gap:f64,#[serde(default)] location:String,settings_json:Option<String> }
async fn create_preset(State(state):State<AppState>,Json(p):Json<NewPreset>) -> ApiResult {
    if p.name.trim().is_empty() || p.printer.trim().is_empty() || p.bed_width<=0. || p.bed_depth<=0. || p.gap<0. || !p.bed_width.is_finite() || !p.bed_depth.is_finite() || !p.gap.is_finite() {return Err(bad("Invalid preset"));}
    if let Some(settings)=&p.settings_json {validate_preset_settings(settings).map_err(bad)?;}
    let preset_id=id();let db=state.db.lock().unwrap();
    db.execute("INSERT INTO presets(id,name,printer,printer_profile,print_profile,filament_profile,bed_width,bed_depth,gap,location,settings_json,created_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",params![preset_id,p.name,p.printer,p.printer_profile,p.print_profile,p.filament_profile,p.bed_width,p.bed_depth,p.gap,p.location.trim(),p.settings_json,now()]).map_err(internal)?;
    Ok(Json(json!({"id":preset_id})))
}

#[derive(Deserialize)] struct PresetPatch { name:Option<String>,printer:Option<String>,printer_profile:Option<String>,print_profile:Option<String>,filament_profile:Option<String>,bed_width:Option<f64>,bed_depth:Option<f64>,gap:Option<f64>,location:Option<String>,settings_json:Option<String> }
async fn update_preset(State(state):State<AppState>,UrlPath(preset_id):UrlPath<String>,Json(p):Json<PresetPatch>) -> ApiResult {
    if let Some(settings)=&p.settings_json {validate_preset_settings(settings).map_err(bad)?;}
    let db=state.db.lock().unwrap();
    load_preset(&db,&preset_id).map_err(bad)?;
    if let Some(v)=p.name {db.execute("UPDATE presets SET name=?1 WHERE id=?2",params![v,preset_id]).map_err(internal)?;}
    if let Some(v)=p.printer {db.execute("UPDATE presets SET printer=?1 WHERE id=?2",params![v,preset_id]).map_err(internal)?;}
    if let Some(v)=p.printer_profile {db.execute("UPDATE presets SET printer_profile=?1 WHERE id=?2",params![v,preset_id]).map_err(internal)?;}
    if let Some(v)=p.print_profile {db.execute("UPDATE presets SET print_profile=?1 WHERE id=?2",params![v,preset_id]).map_err(internal)?;}
    if let Some(v)=p.filament_profile {db.execute("UPDATE presets SET filament_profile=?1 WHERE id=?2",params![v,preset_id]).map_err(internal)?;}
    if let Some(v)=p.bed_width {if v<=0.||!v.is_finite(){return Err(bad("Invalid bed width"));}db.execute("UPDATE presets SET bed_width=?1 WHERE id=?2",params![v,preset_id]).map_err(internal)?;}
    if let Some(v)=p.bed_depth {if v<=0.||!v.is_finite(){return Err(bad("Invalid bed depth"));}db.execute("UPDATE presets SET bed_depth=?1 WHERE id=?2",params![v,preset_id]).map_err(internal)?;}
    if let Some(v)=p.gap {if v<0.||!v.is_finite(){return Err(bad("Invalid gap"));}db.execute("UPDATE presets SET gap=?1 WHERE id=?2",params![v,preset_id]).map_err(internal)?;}
    if let Some(v)=p.location {db.execute("UPDATE presets SET location=?1 WHERE id=?2",params![v.trim(),preset_id]).map_err(internal)?;}
    if let Some(v)=p.settings_json {db.execute("UPDATE presets SET settings_json=?1 WHERE id=?2",params![v,preset_id]).map_err(internal)?;}
    Ok(Json(json!({"ok":true})))
}

#[derive(Deserialize)] struct FilamentPrice {location:String,filament_profile:String,chf_per_kg:f64}
async fn set_filament_price(State(state):State<AppState>,Json(p):Json<FilamentPrice>) -> ApiResult {
    if p.filament_profile.trim().is_empty()||!p.chf_per_kg.is_finite()||p.chf_per_kg<0.0 {return Err(bad("Enter a filament profile and non-negative CHF/kg price"));}
    let db=state.db.lock().unwrap();
    db.execute("INSERT INTO filament_prices(location,filament_profile,chf_per_kg) VALUES (?1,?2,?3) ON CONFLICT(location,filament_profile) DO UPDATE SET chf_per_kg=excluded.chf_per_kg",params![p.location.trim(),p.filament_profile.trim(),p.chf_per_kg]).map_err(internal)?;
    Ok(Json(json!({"ok":true})))
}

fn validate_preset_settings(text:&str)->Result<(),String> {
    let settings:std::collections::BTreeMap<String,String>=serde_json::from_str(text).map_err(|e|e.to_string())?;
    for (key,value) in settings {
        if !["layer_height","fill_density","perimeters","support_material","brim_width","top_solid_layers","bottom_solid_layers"].contains(&key.as_str()) {return Err(format!("Unsupported setting: {key}"));}
        let numeric=value.trim_end_matches('%').parse::<f64>().map_err(|_|format!("Invalid value for {key}"))?;
        let valid=match key.as_str(){
            "layer_height"=>numeric>=0.05&&numeric<=0.4,
            "fill_density"=>numeric<=100.0,
            "perimeters"=>numeric>=1.0&&numeric<=20.0&&numeric.fract()==0.0,
            "support_material"=>numeric==0.0||numeric==1.0,
            "brim_width"=>numeric<=30.0,
            "top_solid_layers"|"bottom_solid_layers"=>numeric<=20.0&&numeric.fract()==0.0,
            _=>false,
        };
        if !numeric.is_finite()||numeric<0.0||!valid {return Err(format!("Invalid value for {key}"));}
    }
    Ok(())
}

async fn upload_preset_ini(State(state):State<AppState>,UrlPath(preset_id):UrlPath<String>,mut multipart:Multipart) -> ApiResult {
    {let db=state.db.lock().unwrap();load_preset(&db,&preset_id).map_err(bad)?;}
    let field=multipart.next_field().await.map_err(bad)?.ok_or_else(||bad("No INI file received"))?;
    let bytes=field.bytes().await.map_err(bad)?;
    let content=std::str::from_utf8(&bytes).map_err(bad)?;
    if !content.contains("=") {return Err(bad("Not a PrusaSlicer INI configuration"));}
    let path=state.data_dir.join("presets").join(format!("{preset_id}.ini"));
    fs::write(&path,&bytes).map_err(internal)?;
    let db=state.db.lock().unwrap();
    db.execute("UPDATE presets SET ini_path=?1 WHERE id=?2",params![path.to_string_lossy(),preset_id]).map_err(internal)?;
    Ok(Json(json!({"ok":true})))
}

#[derive(Deserialize)] struct NewConversation { project_id:String,title:Option<String> }
async fn create_conversation(State(state):State<AppState>,Json(p):Json<NewConversation>) -> ApiResult {
    let db=state.db.lock().unwrap();
    let exists:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM projects WHERE id=?1)",[&p.project_id],|r|r.get(0)).map_err(internal)?;
    if !exists{return Err(not_found("Project not found"));}
    let conversation_id=id();let title=p.title.unwrap_or_else(||"New conversation".into());
    db.execute("INSERT INTO conversations(id,project_id,title,created_at) VALUES (?1,?2,?3,?4)",params![conversation_id,p.project_id,title,now()]).map_err(internal)?;
    Ok(Json(json!({"id":conversation_id})))
}

async fn get_conversation(State(state):State<AppState>,UrlPath(conversation_id):UrlPath<String>) -> ApiResult {
    let db=state.db.lock().unwrap();
    let conversation=rows(&db,"SELECT id,project_id,title,context_text,preset_id,max_minutes,max_grams,email,created_at FROM conversations WHERE id=?1",&[&conversation_id]).map_err(internal)?.into_iter().next().ok_or_else(||not_found("Conversation not found"))?;
    let context_document=rows(&db,"SELECT name,created_at FROM context_documents WHERE scope='conversation' AND scope_id=?1",&[&conversation_id]).map_err(internal)?.into_iter().next();
    let messages=rows(&db,"SELECT id,role,content,created_at FROM messages WHERE conversation_id=?1 ORDER BY created_at,id",&[&conversation_id]).map_err(internal)?;
    let jobs=rows(&db,"SELECT id,prompt,status,result_json,progress_json,error,created_at FROM jobs WHERE conversation_id=?1 ORDER BY created_at DESC",&[&conversation_id]).map_err(internal)?;
    Ok(Json(json!({"conversation":conversation,"context_document":context_document,"messages":messages,"jobs":jobs})))
}

#[derive(Deserialize)] struct NewMessage { text:String }
async fn send_message(State(state):State<AppState>,UrlPath(conversation_id):UrlPath<String>,Json(p):Json<NewMessage>) -> ApiResult {
    let text=p.text.trim();if text.is_empty(){return Err(bad("Message is empty"));}
    let job_id=id();let db=state.db.lock().unwrap();
    let project_id:String=db.query_row("SELECT project_id FROM conversations WHERE id=?1",[&conversation_id],|r|r.get(0)).map_err(|_|not_found("Conversation not found"))?;
    db.execute("INSERT INTO messages(id,conversation_id,role,content,created_at) VALUES (?1,?2,'user',?3,?4)",params![id(),conversation_id,text,now()]).map_err(internal)?;
    db.execute("INSERT INTO jobs(id,project_id,conversation_id,prompt,status,created_at,updated_at) VALUES (?1,?2,?3,?4,'queued',?5,?5)",params![job_id,project_id,conversation_id,text,now()]).map_err(internal)?;
    drop(db);
    let state_clone=state.clone();let job_clone=job_id.clone();
    tokio::spawn(async move {process_job(state_clone,job_clone).await;});
    Ok(Json(json!({"job_id":job_id})))
}

#[derive(Deserialize)] struct ConversationSettingsPatch {preset_id:Option<String>,max_minutes:Option<f64>,max_grams:Option<f64>,email:Option<String>,inherit_preset:Option<bool>,inherit_minutes:Option<bool>,inherit_grams:Option<bool>,inherit_email:Option<bool>}
async fn update_conversation_settings(State(state):State<AppState>,UrlPath(conversation_id):UrlPath<String>,Json(p):Json<ConversationSettingsPatch>) -> ApiResult {
    let db=state.db.lock().unwrap();
    if !db.query_row("SELECT EXISTS(SELECT 1 FROM conversations WHERE id=?1)",[&conversation_id],|r|r.get::<_,bool>(0)).map_err(internal)? {return Err(not_found("Chat not found"));}
    if let Some(preset)=p.preset_id {load_preset(&db,&preset).map_err(bad)?;db.execute("UPDATE conversations SET preset_id=?1 WHERE id=?2",params![preset,conversation_id]).map_err(internal)?;}
    if p.inherit_preset==Some(true) {db.execute("UPDATE conversations SET preset_id=NULL WHERE id=?1",[&conversation_id]).map_err(internal)?;}
    if let Some(minutes)=p.max_minutes {if !minutes.is_finite()||minutes<=0.0 {return Err(bad("Time limit must be positive"));}db.execute("UPDATE conversations SET max_minutes=?1 WHERE id=?2",params![minutes,conversation_id]).map_err(internal)?;}
    if p.inherit_minutes==Some(true) {db.execute("UPDATE conversations SET max_minutes=NULL WHERE id=?1",[&conversation_id]).map_err(internal)?;}
    if let Some(grams)=p.max_grams {if !grams.is_finite()||grams<=0.0 {return Err(bad("Gram limit must be positive"));}db.execute("UPDATE conversations SET max_grams=?1 WHERE id=?2",params![grams,conversation_id]).map_err(internal)?;}
    if p.inherit_grams==Some(true) {db.execute("UPDATE conversations SET max_grams=NULL WHERE id=?1",[&conversation_id]).map_err(internal)?;}
    if let Some(email)=p.email {db.execute("UPDATE conversations SET email=?1 WHERE id=?2",params![email.trim(),conversation_id]).map_err(internal)?;}
    if p.inherit_email==Some(true) {db.execute("UPDATE conversations SET email=NULL WHERE id=?1",[&conversation_id]).map_err(internal)?;}
    Ok(Json(json!({"ok":true})))
}

async fn get_job(State(state):State<AppState>,UrlPath(job_id):UrlPath<String>) -> ApiResult {
    let db=state.db.lock().unwrap();
    let job=rows(&db,"SELECT id,project_id,conversation_id,prompt,status,intent_json,result_json,progress_json,error,created_at,updated_at FROM jobs WHERE id=?1",&[&job_id]).map_err(internal)?.into_iter().next().ok_or_else(||not_found("Job not found"))?;
    Ok(Json(job))
}

async fn download(State(state):State<AppState>,UrlPath((job_id,file)):UrlPath<(String,String)>) -> Result<Response,(StatusCode,String)> {
    if Uuid::parse_str(&job_id).is_err() || !file.starts_with("plate-") || !file.chars().all(|c|c.is_ascii_alphanumeric()||c=='.'||c=='-') || ![".3mf",".bgcode",".svg"].iter().any(|end|file.ends_with(end)) {return Err(not_found("File not found"));}
    let path=state.data_dir.join("jobs").join(job_id).join(&file);
    let bytes=tokio::fs::read(path).await.map_err(|_|not_found("File not found"))?;
    let content_type=if file.ends_with(".svg") {"image/svg+xml"} else {"application/octet-stream"};
    Ok(([(header::CONTENT_TYPE,content_type),(header::CONTENT_DISPOSITION,"attachment")],Body::from(bytes)).into_response())
}

#[derive(Clone)] struct AssetRecord { id:String,name:String,path:PathBuf,kind:String,info:Option<String> }

async fn process_job(state:AppState,job_id:String) {
    if let Err(error)=process_job_inner(&state,&job_id).await {
        tracing::error!("Job {job_id} failed: {error}");
        let db=state.db.lock().unwrap();
        let _=db.execute("UPDATE jobs SET status='failed',error=?1,updated_at=?2 WHERE id=?3",params![error,now(),job_id]);
        if let Ok(conversation_id)=db.query_row::<String,_,_>("SELECT conversation_id FROM jobs WHERE id=?1",[&job_id],|r|r.get(0)) {
            let _=db.execute("INSERT INTO messages(id,conversation_id,role,content,created_at) VALUES (?1,?2,'assistant',?3,?4)",params![id(),conversation_id,format!("I couldn't complete this slice: {error}"),now()]);
        }
    }
}

fn load_context(db:&Connection,scope:&str,scope_id:&str) -> Result<String,String> {
    let table=if scope=="project" {"projects"} else {"conversations"};
    let note:String=db.query_row(&format!("SELECT context_text FROM {table} WHERE id=?1"),[scope_id],|r|r.get(0)).map_err(|e|e.to_string())?;
    let document=rows(db,"SELECT name,path FROM context_documents WHERE scope=?1 AND scope_id=?2",&[&scope,&scope_id]).map_err(|e|e.to_string())?.into_iter().next();
    if let Some(doc)=document {
        let path=doc["path"].as_str().ok_or("Context file path missing")?;
        let contents=fs::read_to_string(path).map_err(|e|format!("Cannot read context file: {e}"))?;
        Ok(format!("{note}\n\nFile {}:\n{contents}",doc["name"].as_str().unwrap_or("context.md")))
    } else {Ok(note)}
}

async fn process_job_inner(state:&AppState,job_id:&str) -> Result<(),String> {
    let (project_id,conversation_id,prompt,file_names,project_context,chat_context)={
        let db=state.db.lock().unwrap();
        db.execute("UPDATE jobs SET status='interpreting',updated_at=?1 WHERE id=?2",params![now(),job_id]).map_err(|e|e.to_string())?;
        let row:(String,String,String)=db.query_row("SELECT project_id,conversation_id,prompt FROM jobs WHERE id=?1",[job_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).map_err(|e|e.to_string())?;
        let names:Vec<String>=rows(&db,"SELECT name FROM assets WHERE project_id=?1",&[&row.0]).map_err(|e|e.to_string())?.iter().filter_map(|v|v.get("name").and_then(|v|v.as_str()).map(str::to_string)).collect();
        let project_context=load_context(&db,"project",&row.0)?;
        let chat_context=load_context(&db,"conversation",&row.1)?;
        (row.0,row.1,row.2,names,project_context,chat_context)
    };
    let llm=active_llm(&state.data_dir)?;
    let (intent,usage)=agent::interpret(&state.client,llm.as_ref().map(|(config,_)|config),&prompt,&file_names,&project_context,&chat_context).await?;
    if let Some(usage)=usage {
        let db=state.db.lock().unwrap();
        db.execute("INSERT INTO usage(id,project_id,job_id,model,input_tokens,output_tokens,cached_tokens,cost_usd,created_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",params![id(),project_id,job_id,usage.model,usage.input_tokens as i64,usage.output_tokens as i64,usage.cached_tokens as i64,usage.cost_usd,now()]).map_err(|e|e.to_string())?;
    }
    if let Some(title)=intent.chat_title.as_deref().map(str::trim).filter(|title|!title.is_empty()) {
        let db=state.db.lock().unwrap();
        let first_message:bool=db.query_row("SELECT COUNT(*)=1 FROM messages WHERE conversation_id=?1 AND role='user'",[&conversation_id],|r|r.get(0)).map_err(|e|e.to_string())?;
        if first_message {db.execute("UPDATE conversations SET title=?1 WHERE id=?2 AND title='New conversation'",params![title.chars().take(70).collect::<String>(),conversation_id]).map_err(|e|e.to_string())?;}
    }
    if let Some(url)=&intent.source_url {fetch_github_assets(state,&project_id,url).await?;}
    if intent.action == "plan" {
        let names={
            let db=state.db.lock().unwrap();
            rows(&db,"SELECT name FROM assets WHERE project_id=?1 ORDER BY name",&[&project_id]).map_err(|e|e.to_string())?
                .into_iter().filter_map(|v|v["name"].as_str().map(str::to_string)).collect::<Vec<_>>()
        };
        let reply=plan_parts(&names);
        let db=state.db.lock().unwrap();
        db.execute("UPDATE jobs SET status='planned',intent_json=?1,result_json=?2,updated_at=?3 WHERE id=?4",params![serde_json::to_string(&intent).unwrap(),json!({"kind":"plan","summary":reply}).to_string(),now(),job_id]).map_err(|e|e.to_string())?;
        db.execute("INSERT INTO messages(id,conversation_id,role,content,created_at) VALUES (?1,?2,'assistant',?3,?4)",params![id(),conversation_id,reply,now()]).map_err(|e|e.to_string())?;
        return Ok(());
    }
    {
        let db=state.db.lock().unwrap();
        db.execute("UPDATE jobs SET status='slicing',intent_json=?1,updated_at=?2 WHERE id=?3",params![serde_json::to_string(&intent).unwrap(),now(),job_id]).map_err(|e|e.to_string())?;
    }
    let state_clone=state.clone();let job=job_id.to_string();let project=project_id.clone();
    let result=task::spawn_blocking(move || run_slice_job(&state_clone,&job,&project,&intent)).await.map_err(|e|e.to_string())??;
    let partial=result["excluded"].as_array().is_some_and(|items|!items.is_empty());
    let missing=result["excluded"].as_array().map(|items|items.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", ")).unwrap_or_default();
    let summary=format!("{} {} plate{} using {}. {}{}",if partial {"Partial result:"} else {"Prepared"},result["plates"].as_array().map(|v|v.len()).unwrap_or(0),if result["plates"].as_array().is_some_and(|v|v.len()==1){""}else{"s"},result["preset_name"].as_str().unwrap_or("the saved preset"),result["summary"].as_str().unwrap_or(""),if partial {format!(" Missing: {missing}.")} else {String::new()});
    let db=state.db.lock().unwrap();
    db.execute("UPDATE jobs SET status=?1,result_json=?2,updated_at=?3 WHERE id=?4",params![if partial {"partial"} else {"complete"},result.to_string(),now(),job_id]).map_err(|e|e.to_string())?;
    db.execute("INSERT INTO messages(id,conversation_id,role,content,created_at) VALUES (?1,?2,'assistant',?3,?4)",params![id(),conversation_id,summary,now()]).map_err(|e|e.to_string())?;
    Ok(())
}

fn plan_parts(names:&[String]) -> String {
    if names.is_empty() { return "I can help identify the parts first. Add the STL files or a source repository, and I’ll review the filenames before we discuss print time or slice anything.".into(); }
    let lower:Vec<String>=names.iter().map(|n|n.to_ascii_lowercase()).collect();
    let has=|name:&str|lower.iter().any(|n|n==name);
    let mut text=format!("I found {} model files. I have not sliced them.\n\n",names.len());
    if has("battery_mount.stl") && has("battery_mount_eu.stl") {
        text.push_str("Battery: battery_mount.stl and battery_mount_eu.stl appear to be alternatives; choose one after checking the battery's model or dimensions. An Amazon.de purchase does not establish which mount fits.\n\n");
    }
    if has("drive_motor_mount.stl") && has("drive_motor_mount_v2.stl") {
        text.push_str("Drive motor: drive_motor_mount.stl and drive_motor_mount_v2.stl look like revisions. I would select one only after checking the assembly instructions.\n\n");
    }
    if has("base_plate_layer1.stl") && has("base_plate_layer2.stl") {
        text.push_str("Base plate: layer1 and layer2 look like complementary pieces, but their names alone do not confirm quantities or assembly.\n\n");
    }
    let generic:Vec<_>=names.iter().filter(|n|!n.to_ascii_lowercase().contains("battery_mount") && !n.to_ascii_lowercase().contains("drive_motor_mount") && !n.to_ascii_lowercase().contains("base_plate_layer") && !n.to_ascii_lowercase().contains("pi_case")).collect();
    if !generic.is_empty() {text.push_str("Other loaded models: ");text.push_str(&generic.into_iter().map(|n|n.as_str()).collect::<Vec<_>>().join(", "));text.push_str(".\n\n");}
    text.push_str("The filenames do not establish a complete bill of materials or which camera pieces are required for your build. If you share the model's assembly or print instructions and the battery model, I can narrow the list and discuss print duration. I will wait for your request before slicing.");
    text
}

fn set_progress(state:&AppState,job_id:&str,phase:&str,plate:usize,part:usize,total:usize) {
    let db=state.db.lock().unwrap();
    let progress=json!({"phase":phase,"plate":plate,"part":part,"total":total});
    let _=db.execute("UPDATE jobs SET progress_json=?1,updated_at=?2 WHERE id=?3",params![progress.to_string(),now(),job_id]);
}

fn publish_plate_progress(state:&AppState,job_id:&str,result:&Value)->Result<(),String> {
    let db=state.db.lock().unwrap();
    db.execute("UPDATE jobs SET result_json=?1,updated_at=?2 WHERE id=?3",params![result.to_string(),now(),job_id]).map_err(|error|error.to_string())?;
    Ok(())
}

fn run_slice_job(state:&AppState,job_id:&str,project_id:&str,intent:&agent::Intent) -> Result<Value,String> {
    let slicer=state.slicer.as_ref().ok_or("PrusaSlicer executable was not found. Set PRUSA_SLICER on the server.")?;
    let (preset,assets,price_unit,price_rate,email,default_max_minutes,chat_max_grams,filament_price_chf_per_kg)={
        let db=state.db.lock().unwrap();
        let (default_id,price_unit,price_rate,email,default_max_minutes,chat_max_grams):(String,String,f64,Option<String>,f64,Option<f64>)=db.query_row("SELECT COALESCE(c.preset_id,p.default_preset_id,(SELECT default_preset_id FROM app_settings WHERE id=1)),p.price_unit,p.price_rate,COALESCE(c.email,p.email,(SELECT default_email FROM app_settings WHERE id=1)),COALESCE(c.max_minutes,p.default_max_minutes),c.max_grams FROM projects p JOIN conversations c ON c.project_id=p.id JOIN jobs j ON j.conversation_id=c.id WHERE j.id=?1",[job_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?))).map_err(|e|e.to_string())?;
        let preset_id=if let Some(printer)=&intent.printer {let selected=load_preset(&db,&default_id)?;if selected.printer.eq_ignore_ascii_case(printer) {default_id} else {db.query_row::<String,_,_>("SELECT id FROM presets WHERE lower(printer)=lower(?1) ORDER BY (location=?2) DESC,created_at DESC LIMIT 1",params![printer,selected.location],|r|r.get(0)).map_err(|_|format!("No preset found for printer {printer}"))?}} else {default_id};
        let preset=load_preset(&db,&preset_id)?;
        let filament_price_chf_per_kg:Option<f64>=db.query_row("SELECT chf_per_kg FROM filament_prices WHERE location=?1 AND filament_profile=?2",params![preset.location,preset.filament_profile],|r|r.get(0)).ok();
        let mut stmt=db.prepare("SELECT id,name,path,kind,info_json FROM assets WHERE project_id=?1 ORDER BY created_at,id").map_err(|e|e.to_string())?;
        let assets:Vec<AssetRecord>=stmt.query_map([project_id],|r|Ok(AssetRecord{id:r.get(0)?,name:r.get(1)?,path:PathBuf::from(r.get::<_,String>(2)?),kind:r.get(3)?,info:r.get(4)?})).map_err(|e|e.to_string())?.collect::<Result<_,_>>().map_err(|e|e.to_string())?;
        (preset,assets,price_unit,price_rate,email,default_max_minutes,chat_max_grams,filament_price_chf_per_kg)
    };
    let mut warnings=Vec::new();
    let mut parts=Vec::new();
    for asset in &assets {
        if asset.kind!="stl" {warnings.push(format!("{} is stored, but this version packs STL files only",asset.name));continue;}
        if intent.exclude_files.iter().any(|name|name.eq_ignore_ascii_case(&asset.name)) {continue;}
        let mesh=mesh::Mesh::read(&asset.path).map_err(|e|format!("{}: {e}",asset.name))?;
        let info=mesh.info();
        for warning in info.warnings {
            if let Some(count)=warning.strip_suffix(" zero-area triangles") {
                warnings.push(format!("{}: {count} triangles have zero area, so they add no printable surface. Check the sliced preview for missing geometry.",asset.name));
            } else {warnings.push(format!("{}: {warning}",asset.name));}
        }
        let oriented=mesh.oriented(&intent.orientation);
        if oriented.height>250.0 {warnings.push(format!("{} is taller than 250 mm; check the selected printer",asset.name));}
        parts.push(Part{asset_id:asset.id.clone(),name:asset.name.clone(),oriented});
        let _=&asset.info;
    }
    if parts.is_empty() {return Err("No printable STL files are stored in this project. Upload files or provide a GitHub repository URL.".into());}
    if parts.iter().any(|part|part.name.eq_ignore_ascii_case("battery_mount.stl")) && parts.iter().any(|part|part.name.eq_ignore_ascii_case("battery_mount_eu.stl")) {
        warnings.push("Both battery mount variants were included as requested; check which one fits your battery before printing.".into());
    }
    if parts.iter().any(|part|part.name.eq_ignore_ascii_case("drive_motor_mount.stl")) && parts.iter().any(|part|part.name.eq_ignore_ascii_case("drive_motor_mount_v2.stl")) {
        warnings.push("Both drive motor mount revisions were requested; check whether the assembly needs both.".into());
    }
    parts.sort_by(|a,b|(b.oriented.width*b.oriented.depth).total_cmp(&(a.oriented.width*a.oriented.depth)));
    if intent.repeat_copies {
        let originals=parts.clone();
        for _ in 0..29 {parts.extend(originals.iter().cloned());if parts.len()>100 {break;}}
        parts.truncate(100);
    }
    let requested_part_count=parts.len();
    let dir=state.data_dir.join("jobs").join(job_id);fs::create_dir_all(&dir).map_err(|e|e.to_string())?;
    let mut preset=preset;
    if !intent.print_settings.is_empty() {
        let mut merged:std::collections::BTreeMap<String,String>=preset.settings_json.as_deref().and_then(|text|serde_json::from_str(text).ok()).unwrap_or_default();
        merged.extend(intent.print_settings.clone());
        let text=serde_json::to_string(&merged).map_err(|e|e.to_string())?;
        validate_preset_settings(&text)?;
        preset.settings_json=Some(text);
    }
    let preset=prusa::prepare_preset(slicer,&preset,&dir)?;
    let mut remaining=parts;
    let mut plates=Vec::new();
    let mut excluded=Vec::new();
    let mut rejection_reasons:HashMap<String,String>=HashMap::new();
    let time_limit=intent.max_minutes.unwrap_or(default_max_minutes);
    let grams_limit=intent.max_grams.or(chat_max_grams);
    for plate_index in 1..=20 {
        if remaining.is_empty(){break;}
        let candidate_total=remaining.len();
        let mut plate=Plate::new(preset.bed_width,preset.bed_depth,preset.gap);
        let mut next=Vec::new();
        let mut metrics=SliceMetrics::default();
        let candidate=dir.join("candidate.3mf");let candidate_gcode=dir.join("candidate.gcode");
        for (candidate_index,part) in remaining.into_iter().enumerate() {
            set_progress(state,job_id,"testing layouts",plate_index,candidate_index+1,candidate_total);
            let mut trial=plate.clone();
            if !trial.add(part.clone()) {
                if plate.placements.is_empty() {
                    rejection_reasons.insert(part.asset_id.clone(),format!("Footprint {:.0} × {:.0} mm cannot fit with {:.0} mm edge spacing on the {:.0} × {:.0} mm bed, including a 90° rotation.",part.oriented.width,part.oriented.depth,preset.gap,preset.bed_width,preset.bed_depth));
                }
                next.push(part);continue;
            }
            prusa::write_plate_3mf(&trial,&candidate)?;
            let trial_metrics=prusa::slice_candidate(slicer,&preset,&candidate,&candidate_gcode)?;
            if trial_metrics.minutes.is_none() || trial_metrics.grams.is_none() || trial_metrics.metres.is_none() {return Err("PrusaSlicer output lacks time or filament estimates".into());}
            let time_ok=trial_metrics.minutes.unwrap()<time_limit;
            let grams_ok=grams_limit.is_none_or(|limit|trial_metrics.grams.unwrap()<=limit);
            if time_ok&&grams_ok {rejection_reasons.remove(&part.asset_id);plate=trial;metrics=trial_metrics;}
            else {
                let mut reasons=Vec::new();
                if !time_ok {reasons.push(format!("Estimated {:.0} min exceeds the {:.0} min per-plate limit",trial_metrics.minutes.unwrap().ceil(),time_limit));}
                if !grams_ok {reasons.push(format!("Estimated {:.1} g exceeds the {:.1} g per-plate limit",trial_metrics.grams.unwrap(),grams_limit.unwrap()));}
                rejection_reasons.insert(part.asset_id.clone(),reasons.join("; "));
                next.push(part);
            }
        }
        if plate.placements.is_empty() {
            remaining=next;
            break;
        }
        let base=format!("plate-{plate_index}");
        let input=dir.join(format!("{base}-layout.3mf"));
        let project=dir.join(format!("{base}.3mf"));
        let bgcode=dir.join(format!("{base}.bgcode"));
        let preview=dir.join(format!("{base}.svg"));
        set_progress(state,job_id,"exporting plate",plate_index,candidate_total,candidate_total);
        prusa::write_plate_3mf(&plate,&input)?;
        prusa::export_final(slicer,&preset,&input,&project,&bgcode)?;
        write_preview(&plate,&preview)?;
        let placements:Vec<Value>=plate.placements.iter().map(|p|json!({"asset_id":p.part.asset_id,"name":p.part.name,"x":p.x,"y":p.y,"width":p.part.oriented.width,"depth":p.part.oriented.depth,"height":p.part.oriented.height,"orientation":p.part.oriented.orientation})).collect();
        let print_cost=if price_unit=="m" {metrics.metres.unwrap()*price_rate} else {metrics.grams.unwrap()*price_rate};
        let filament_cost=filament_price_chf_per_kg.map(|price|metrics.grams.unwrap()*price/1000.0);
        plates.push(json!({"number":plate_index,"placements":placements,"metrics":metrics,"estimated_cost_chf":print_cost,"filament_cost_chf":filament_cost,"files":{"project":format!("/api/download/{job_id}/{base}.3mf"),"bgcode":format!("/api/download/{job_id}/{base}.bgcode"),"preview":format!("/api/download/{job_id}/{base}.svg")}}));
        publish_plate_progress(state,job_id,&json!({"kind":"slicing_progress","preset_name":&preset.name,"printer":&preset.printer,"plates":&plates,"warnings":&warnings,"excluded_details":[],"total_parts":requested_part_count,"filament_price_chf_per_kg":filament_price_chf_per_kg}))?;
        remaining=next;
        if intent.repeat_copies && intent.max_minutes.is_none() && intent.max_grams.is_none() {break;}
    }
    excluded.extend(remaining.iter().map(|part|part.name.clone()));
    if plates.is_empty(){
        let problems=excluded.iter().map(|name|{let reason=assets.iter().find(|asset|&asset.name==name).and_then(|asset|rejection_reasons.get(&asset.id)).map(String::as_str).unwrap_or("Could not fit the selected bed and limits");format!("{name}: {reason}")}).collect::<Vec<_>>().join("; ");
        return Err(format!("No parts could be sliced with preset {}. {problems}",preset.name));
    }
    let excluded_details:Vec<Value>=excluded.iter().map(|name|{
        let reason=assets.iter().find(|asset|&asset.name==name).and_then(|asset|rejection_reasons.get(&asset.id)).cloned().unwrap_or_else(||"Could not place under the selected bed, time, or material limits.".into());
        json!({"name":name,"reason":reason})
    }).collect();
    let total_grams:f64=plates.iter().filter_map(|v|v["metrics"]["grams"].as_f64()).sum();
    let total_metres:f64=plates.iter().filter_map(|v|v["metrics"]["metres"].as_f64()).sum();
    let total_cost_chf:f64=plates.iter().filter_map(|v|v["estimated_cost_chf"].as_f64()).sum();
    let summary=format!("Estimated {:.1} g, {:.2} m, CHF {:.2}. {}",total_grams,total_metres,total_cost_chf,if excluded.is_empty(){"All selected parts fit."}else{"Some parts remain unsliced; see the omitted parts list."});
    let email_status=if email.is_some(){"not_configured"}else{"no_recipient"};
    Ok(json!({"preset_name":preset.name,"printer":preset.printer,"location":preset.location,"filament_profile":preset.filament_profile,"plates":plates,"excluded":excluded,"excluded_details":excluded_details,"warnings":warnings,"total_parts":requested_part_count,"total_grams":total_grams,"total_metres":total_metres,"total_cost_chf":total_cost_chf,"filament_price_chf_per_kg":filament_price_chf_per_kg,"price_unit":price_unit,"price_rate":price_rate,"summary":summary,"email_status":email_status}))
}

fn write_preview(plate:&Plate,path:&Path) -> Result<(),String> {
    let scale=2.0;
    let width=plate.bed_width*scale;let height=plate.bed_depth*scale;
    let mut svg=format!("<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {width:.1} {height:.1}\" role=\"img\"><rect width=\"100%\" height=\"100%\" rx=\"16\" fill=\"#edf0ec\"/><rect x=\"1\" y=\"1\" width=\"{:.1}\" height=\"{:.1}\" rx=\"15\" fill=\"none\" stroke=\"#96a99c\" stroke-width=\"2\"/>",width-2.0,height-2.0);
    for (i,p) in plate.placements.iter().enumerate() {
        let x=p.x*scale;let y=p.y*scale;let w=p.part.oriented.width*scale;let h=p.part.oriented.depth*scale;
        svg.push_str(&format!("<rect x=\"{x:.1}\" y=\"{y:.1}\" width=\"{w:.1}\" height=\"{h:.1}\" rx=\"5\" fill=\"#cf5c3a\" stroke=\"#8e341d\" stroke-width=\"1\"/><text x=\"{:.1}\" y=\"{:.1}\" fill=\"white\" font-family=\"sans-serif\" font-size=\"13\" font-weight=\"700\">{}</text>",x+7.0,y+17.0,i+1));
    }
    svg.push_str("</svg>");fs::write(path,svg).map_err(|e|e.to_string())
}

fn email_configured() -> bool {false}

#[cfg(test)]
mod llm_settings_tests {
    use super::*;
    #[test]
    fn saved_llm_key_stays_server_side_and_overrides_environment() {
        let directory=std::env::temp_dir().join(format!("sliceagent-llm-test-{}",id()));
        fs::create_dir(&directory).unwrap();
        write_llm_file(&directory,&StoredLlm{provider:"openai".into(),model:"gpt-6-astra".into(),api_key:Some("test-only-key".into())}).unwrap();
        let (active,source)=active_llm(&directory).unwrap().unwrap();
        assert_eq!(active.api_key,"test-only-key");
        assert_eq!(active.provider,"openai");
        assert_eq!(source,"saved");
        #[cfg(unix)] {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(llm_file(&directory)).unwrap().permissions().mode()&0o777,0o600);
        }
        fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn existing_openai_config_defaults_to_openai_provider() {
        let saved:StoredLlm=serde_json::from_str(r#"{"model":"gpt-6-astra","api_key":"old-key"}"#).unwrap();
        assert_eq!(saved.provider,"openai");
        assert_eq!(saved.api_key.as_deref(),Some("old-key"));
        assert_eq!(provider_env_key("anthropic"),"ANTHROPIC_API_KEY");
        assert_eq!(provider_env_key("openai"),"OPENAI_API_KEY");
        assert_eq!(provider_env_key("openrouter"),"OPENROUTER_API_KEY");
    }
    #[test]
    fn saved_claude_key_uses_anthropic_provider() {
        let directory=std::env::temp_dir().join(format!("sliceagent-claude-test-{}",id()));
        fs::create_dir(&directory).unwrap();
        write_llm_file(&directory,&StoredLlm{provider:"anthropic".into(),model:"claude-sonnet-4-6".into(),api_key:Some("test-only-claude-key".into())}).unwrap();
        let (active,source)=active_llm(&directory).unwrap().unwrap();
        assert_eq!(active.provider,"anthropic");
        assert_eq!(active.model,"claude-sonnet-4-6");
        assert_eq!(active.api_key,"test-only-claude-key");
        assert_eq!(source,"saved");
        fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn rejects_model_ids_with_url_metacharacters() {
        assert!(validate_model("gpt-6-astra").is_ok());
        assert!(validate_model("anthropic/claude-sonnet-4.6").is_ok());
        assert!(validate_model("~anthropic/claude-sonnet-latest").is_ok());
        assert!(validate_model("../api-keys?x=1").is_err());
    }
}
