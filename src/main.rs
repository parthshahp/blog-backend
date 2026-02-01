use anyhow::Context;
use axum::{
    body::Body,
    extract::State,
    http::{Request, StatusCode},
    middleware::{self, Next},
    response::Response,
    routing::get,
    Json, Router,
};
use chrono::NaiveDate;
use reqwest::Client;
use rss::{extension::ExtensionMap, Channel, Item};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    env,
    io::Cursor,
    net::SocketAddr,
    path::PathBuf,
    sync::Arc,
    time::Duration,
};
use tokio::{fs, sync::RwLock, time};
use tracing::{info, warn};

#[derive(Clone)]
struct AppState {
    storage: Arc<RwLock<Storage>>,
    client: Client,
    rss_url: String,
    allowlist: AllowList,
}

#[derive(Clone, Default)]
struct AllowList {
    origins: Option<HashSet<String>>,
    hosts: Option<HashSet<String>>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct Movie {
    name: String,
    watched_date: String,
    rating: Option<f32>,
    link: String,
    poster_url: Option<String>,
}

struct Storage {
    path: PathBuf,
    movies: Vec<Movie>,
}

impl Storage {
    async fn load(path: PathBuf) -> anyhow::Result<Self> {
        let movies = if fs::try_exists(&path).await? {
            let bytes = fs::read(&path).await?;
            serde_json::from_slice::<Vec<Movie>>(&bytes)?
        } else {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).await?;
            }
            fs::write(&path, b"[]\n").await?;
            Vec::new()
        };
        Ok(Self { path, movies })
    }

    async fn save(&self) -> anyhow::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).await?;
        }
        let bytes = serde_json::to_vec_pretty(&self.movies)?;
        fs::write(&self.path, bytes).await?;
        Ok(())
    }

    fn sort_movies(&mut self) {
        self.movies.sort_by(|a, b| b.watched_date.cmp(&a.watched_date));
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_env_filter(
            env::var("RUST_LOG").unwrap_or_else(|_| "info,hyper=warn".to_string()),
        )
        .init();

    let rss_url = env::var("RSS_URL").unwrap_or_else(|_| "https://letterboxd.com/istangel/rss/".to_string());
    let data_path = env::var("DATA_PATH").unwrap_or_else(|_| "data/movies.json".to_string());
    let allowed_origins = parse_allowlist(env::var("ALLOWED_ORIGINS").ok());
    let allowed_hosts = parse_allowlist(env::var("ALLOWED_HOSTS").ok());
    let port: u16 = env::var("PORT")
        .unwrap_or_else(|_| "3000".to_string())
        .parse()
        .unwrap_or(3000);

    let data_path = PathBuf::from(data_path);
    info!(
        data_path = %data_path.display(),
        seed = "none",
        rss_url = %rss_url,
        %port,
        allowed_origins = %allowlist_display(&allowed_origins),
        allowed_hosts = %allowlist_display(&allowed_hosts),
        "starting blog-backend"
    );

    let storage = Storage::load(data_path.clone())
        .await
        .context("loading data file")?;
    info!(count = storage.movies.len(), "loaded movies");
    let app_state = AppState {
        storage: Arc::new(RwLock::new(storage)),
        client: Client::builder().user_agent("blog-backend/0.1").build()?,
        rss_url,
        allowlist: AllowList {
            origins: allowed_origins,
            hosts: allowed_hosts,
        },
    };

    let bg_state = app_state.clone();
    tokio::spawn(async move {
        if let Err(err) = refresh_from_rss(&bg_state).await {
            warn!(error = %err, "initial rss refresh failed");
        }

        let next_tick = time::Instant::now() + Duration::from_secs(60 * 60 * 24);
        let mut interval = time::interval_at(next_tick, Duration::from_secs(60 * 60 * 24));
        interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            if let Err(err) = refresh_from_rss(&bg_state).await {
                warn!(error = %err, "scheduled rss refresh failed");
            }
        }
    });

    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/movies", get(list_movies))
        .with_state(app_state.clone())
        .layer(middleware::from_fn_with_state(
            app_state,
            restrict_requests,
        ));

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .context("binding listen address")?;
    info!(%port, "server listening");
    axum::serve(listener, app)
        .await
        .context("serving http")?;
    Ok(())
}

async fn healthz() -> &'static str {
    "ok"
}

async fn list_movies(State(state): State<AppState>) -> Json<Vec<Movie>> {
    let storage = state.storage.read().await;
    Json(storage.movies.clone())
}

async fn restrict_requests(
    State(state): State<AppState>,
    req: Request<Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    if let Some(allowed) = state.allowlist.origins.as_ref() {
        if let Some(origin) = req.headers().get(axum::http::header::ORIGIN) {
            let origin = origin
                .to_str()
                .map_err(|_| StatusCode::BAD_REQUEST)?
                .trim();
            if !is_origin_allowed(origin, allowed) {
                return Err(StatusCode::FORBIDDEN);
            }
        }
    }

    if let Some(allowed) = state.allowlist.hosts.as_ref() {
        let host = req
            .headers()
            .get(axum::http::header::HOST)
            .ok_or(StatusCode::FORBIDDEN)?
            .to_str()
            .map_err(|_| StatusCode::BAD_REQUEST)?
            .trim();
        let host_no_port = host.split(':').next().unwrap_or(host);
        if !allowed.contains(host) && !allowed.contains(host_no_port) {
            return Err(StatusCode::FORBIDDEN);
        }
    }

    Ok(next.run(req).await)
}


async fn refresh_from_rss(state: &AppState) -> anyhow::Result<usize> {
    info!("refreshing rss feed");
    let response = state.client.get(&state.rss_url).send().await?;
    if !response.status().is_success() {
        anyhow::bail!("rss fetch failed with status {}", response.status());
    }
    let bytes = response.bytes().await?;
    let channel = Channel::read_from(Cursor::new(bytes))?;

    let mut storage = state.storage.write().await;
    let mut existing: HashSet<(String, String)> = storage
        .movies
        .iter()
        .map(|m| (m.watched_date.clone(), m.name.clone()))
        .collect();
    let mut new_movies = Vec::new();

    for item in channel.items() {
        if let Some(movie) = movie_from_item(item) {
            let key = (movie.watched_date.clone(), movie.name.clone());
            if existing.insert(key) {
                new_movies.push(movie);
            }
        }
    }

    if !new_movies.is_empty() {
        let count = new_movies.len();
        storage.movies.extend(new_movies);
        storage.sort_movies();
        storage.save().await?;
        info!(%count, "stored new movies");
        Ok(count)
    } else {
        info!("no new movies");
        Ok(0)
    }
}

fn movie_from_item(item: &Item) -> Option<Movie> {
    let link = item.link().unwrap_or_default().trim().to_string();
    if link.is_empty() {
        return None;
    }

    let exts = item.extensions();
    let watched_date = get_ext(exts, "letterboxd", "watchedDate")?;
    if NaiveDate::parse_from_str(&watched_date, "%Y-%m-%d").is_err() {
        return None;
    }
    let name = get_ext(exts, "letterboxd", "filmTitle")
        .or_else(|| item.title().map(|t| t.to_string()))
        .unwrap_or_else(|| link.clone());
    let rating = get_ext(exts, "letterboxd", "memberRating").and_then(|r| r.parse().ok());
    let poster_url = extract_poster_url(item.description());
    Some(Movie {
        link,
        rating,
        name,
        watched_date,
        poster_url,
    })
}

fn get_ext(exts: &ExtensionMap, namespace: &str, name: &str) -> Option<String> {
    exts.get(namespace)
        .and_then(|map| map.get(name))
        .and_then(|values| values.first())
        .and_then(|ext| ext.value.clone())
}

fn extract_poster_url(description: Option<&str>) -> Option<String> {
    let description = description?;
    let img_tag = description.split("<img").nth(1)?;
    let src_part = img_tag.split("src=\"").nth(1)?;
    let url = src_part.split('"').next()?.trim();
    if url.is_empty() {
        None
    } else {
        Some(url.to_string())
    }
}

fn parse_allowlist(raw: Option<String>) -> Option<HashSet<String>> {
    let raw = raw?;
    let mut set = HashSet::new();
    for value in raw.split(',') {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            set.insert(trimmed.to_string());
        }
    }
    if set.is_empty() { None } else { Some(set) }
}

fn allowlist_display(list: &Option<HashSet<String>>) -> String {
    match list {
        Some(values) if !values.is_empty() => {
            let mut items: Vec<_> = values.iter().cloned().collect();
            items.sort();
            items.join(",")
        }
        _ => "none".to_string(),
    }
}

fn is_origin_allowed(origin: &str, allowed: &HashSet<String>) -> bool {
    if allowed.contains(origin) {
        return true;
    }

    if !allowed.contains("localhost") {
        return false;
    }

    let origin = origin.trim();
    let rest = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"));
    let rest = match rest {
        Some(value) => value,
        None => return false,
    };
    let host = rest.split('/').next().unwrap_or(rest);
    let host = host.split(':').next().unwrap_or(host);
    host == "localhost"
}
