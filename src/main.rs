use anyhow::Context;
use chrono::NaiveDate;
use reqwest::Client;
use rss::{Channel, Item, extension::ExtensionMap};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    env,
    io::Cursor,
    time::Duration,
};
use tokio::time;
use tracing::{info, warn};

#[derive(Debug, Serialize, Deserialize, Clone)]
struct Movie {
    name: String,
    watched_date: String,
    rating: Option<f32>,
    link: String,
    poster_url: Option<String>,
}

const GIST_FILENAME: &str = "movies.json";

#[derive(Deserialize)]
struct GistResponse {
    files: HashMap<String, GistFile>,
}

#[derive(Deserialize)]
struct GistFile {
    content: String,
}

#[derive(Serialize)]
struct GistUpdate {
    files: HashMap<String, GistFileContent>,
}

#[derive(Serialize)]
struct GistFileContent {
    content: String,
}

struct Storage {
    gist_id: String,
    github_token: String,
    client: Client,
    movies: Vec<Movie>,
}

impl Storage {
    async fn load(client: Client, gist_id: String, github_token: String) -> anyhow::Result<Self> {
        let response = client
            .get(format!("https://api.github.com/gists/{gist_id}"))
            .header("Authorization", format!("Bearer {github_token}"))
            .header("Accept", "application/vnd.github+json")
            .send()
            .await
            .context("fetching gist")?;

        if !response.status().is_success() {
            anyhow::bail!("gist fetch failed with status {}", response.status());
        }

        let gist: GistResponse = response.json().await.context("parsing gist response")?;
        let movies = gist
            .files
            .get(GIST_FILENAME)
            .map(|f| serde_json::from_str::<Vec<Movie>>(&f.content))
            .transpose()
            .context("parsing movies from gist")?
            .unwrap_or_default();

        Ok(Self {
            gist_id,
            github_token,
            client,
            movies,
        })
    }

    async fn save(&self) -> anyhow::Result<()> {
        let content = serde_json::to_string_pretty(&self.movies)?;
        let mut files = HashMap::new();
        files.insert(GIST_FILENAME.to_string(), GistFileContent { content });
        let update = GistUpdate { files };

        let response = self
            .client
            .patch(format!("https://api.github.com/gists/{}", self.gist_id))
            .header("Authorization", format!("Bearer {}", self.github_token))
            .header("Accept", "application/vnd.github+json")
            .json(&update)
            .send()
            .await
            .context("patching gist")?;

        if !response.status().is_success() {
            anyhow::bail!("gist patch failed with status {}", response.status());
        }

        Ok(())
    }

    fn sort_movies(&mut self) {
        self.movies
            .sort_by(|a, b| b.watched_date.cmp(&a.watched_date));
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_env_filter(env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string()))
        .init();

    let rss_url =
        env::var("RSS_URL").unwrap_or_else(|_| "https://letterboxd.com/istangel/rss/".to_string());
    let github_token = env::var("GITHUB_TOKEN").context("GITHUB_TOKEN not set")?;
    let gist_id = env::var("GIST_ID").context("GIST_ID not set")?;

    info!(gist_id = %gist_id, rss_url = %rss_url, "starting letterboxd-gist-sync");

    let client = Client::builder()
        .user_agent("blog-backend/0.1")
        .timeout(Duration::from_secs(10))
        .build()?;

    let mut storage = Storage::load(client.clone(), gist_id, github_token)
        .await
        .context("loading gist")?;
    info!(count = storage.movies.len(), "loaded movies from gist");

    if let Err(err) = refresh_from_rss(&mut storage, &client, &rss_url).await {
        warn!(error = %err, "initial rss refresh failed");
    }

    let mut interval = time::interval(Duration::from_secs(60 * 60 * 24));
    interval.tick().await; // consume the immediate first tick
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            () = &mut shutdown => {
                info!("shutting down");
                break;
            }
            _ = interval.tick() => {
                if let Err(err) = refresh_from_rss(&mut storage, &client, &rss_url).await {
                    warn!(error = %err, "scheduled rss refresh failed");
                }
            }
        }
    }

    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = ctrl_c => {}
                    _ = term.recv() => {}
                }
            }
            Err(err) => {
                warn!(error = %err, "failed to install SIGTERM handler");
                ctrl_c.await.ok();
            }
        }
    }
    #[cfg(not(unix))]
    ctrl_c.await.ok();
}

async fn refresh_from_rss(
    storage: &mut Storage,
    client: &Client,
    rss_url: &str,
) -> anyhow::Result<usize> {
    info!("refreshing rss feed");
    let response = client.get(rss_url).send().await?;
    if !response.status().is_success() {
        anyhow::bail!("rss fetch failed with status {}", response.status());
    }
    let bytes = response.bytes().await?;
    let channel = Channel::read_from(Cursor::new(bytes))?;

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

    if new_movies.is_empty() {
        info!("no new movies");
        Ok(0)
    } else {
        let count = new_movies.len();
        storage.movies.extend(new_movies);
        storage.sort_movies();
        storage.save().await?;
        info!(%count, "stored new movies");
        Ok(count)
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
        .or_else(|| item.title().map(ToString::to_string))
        .unwrap_or_else(|| link.clone());
    let rating = get_ext(exts, "letterboxd", "memberRating").and_then(|r| r.parse().ok());
    let poster_url = extract_poster_url(item.description());
    Some(Movie {
        name,
        watched_date,
        rating,
        link,
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
