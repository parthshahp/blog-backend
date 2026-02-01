use axum::{extract::State, routing::get, Json, Router};
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
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct Movie {
    name: String,
    watched_date: String,
    rating: Option<f32>,
    link: String,
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
        .with_env_filter(
            env::var("RUST_LOG").unwrap_or_else(|_| "info,hyper=warn".to_string()),
        )
        .init();

    let rss_url = env::var("RSS_URL").unwrap_or_else(|_| "https://letterboxd.com/istangel/rss/".to_string());
    let data_path = env::var("DATA_PATH").unwrap_or_else(|_| "data/movies.json".to_string());
    let port: u16 = env::var("PORT")
        .unwrap_or_else(|_| "3000".to_string())
        .parse()
        .unwrap_or(3000);

    let storage = Storage::load(PathBuf::from(data_path)).await?;
    let app_state = AppState {
        storage: Arc::new(RwLock::new(storage)),
        client: Client::builder().user_agent("blog-backend/0.1").build()?,
        rss_url,
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
        .with_state(app_state);

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    info!(%port, "server listening");
    axum::serve(tokio::net::TcpListener::bind(addr).await?, app).await?;
    Ok(())
}

async fn healthz() -> &'static str {
    "ok"
}

async fn list_movies(State(state): State<AppState>) -> Json<Vec<Movie>> {
    let storage = state.storage.read().await;
    Json(storage.movies.clone())
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
    Some(Movie {
        link,
        rating,
        name,
        watched_date,
    })
}

fn get_ext(exts: &ExtensionMap, namespace: &str, name: &str) -> Option<String> {
    exts.get(namespace)
        .and_then(|map| map.get(name))
        .and_then(|values| values.first())
        .and_then(|ext| ext.value.clone())
}
