use std::{cmp, collections::HashSet, path::PathBuf, process::Output, sync::Arc, time::Duration};

use anyhow::{Ok, Result};

use clap::Parser;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::{
    fs::{self, File, OpenOptions},
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
    sync::Mutex,
    task::JoinSet,
    time,
};

const PLAYLIST_INFO_FETCH_URL: &str = "https://www.googleapis.com/youtube/v3/playlists";
const PLAYLIST_ITEMS_FETCH_URL: &str = "https://www.googleapis.com/youtube/v3/playlistItems";

const YT_DLP_ARGS: [&str; 8] = [
    "--extract-audio",
    "--audio-format",
    "mp3",
    "--add-metadata",
    "--metadata-from-title",
    "%(artist) - %(title)s",
    "--output",
    "%(title).90s.%(ext)s",
];

#[derive(Debug, Serialize, Deserialize, Parser)]
#[command(author, version, about, long_about = None)]
struct Config {
    #[arg(short, long)]
    #[clap(default_value = "songs")]
    output_dir: PathBuf,
    #[arg(short, long)]
    #[clap(default_value = "cache")]
    cache_file: PathBuf,
    playlist_id: String,
    #[arg(short, long)]
    #[clap(default_value = "50")]
    max_results: u8,
    #[arg(short, long)]
    #[clap(default_value = "false")]
    daemon: bool,
    #[arg(short, long)]
    #[clap(default_value = "7200")]
    seconds_interval: u64,
}

#[derive(Debug)]
struct Cache {
    inner: HashSet<String>,
    cache_file: File,
    dirty: bool,
}

impl Cache {
    async fn get_or_create(cfg: &Config) -> Result<Self> {
        let path = &cfg.cache_file;
        if path.exists() {
            let mut file = OpenOptions::new()
                .read(true)
                .write(true)
                .append(true)
                .open(path)
                .await?;
            let set = {
                let mut contents = String::new();
                file.read_to_string(&mut contents).await?;

                contents.lines().map(str::to_owned).collect()
            };

            Ok(Self {
                inner: set,
                cache_file: file,
                dirty: false,
            })
        } else {
            Ok(Self {
                inner: HashSet::new(),
                cache_file: File::create(path).await?,
                dirty: false,
            })
        }
    }

    fn contains(&self, url: &str) -> bool {
        self.inner.contains(url)
    }

    async fn emit(&mut self, urls: &[String]) -> Result<()> {
        let mut output = urls.join("\n");
        output.push('\n');

        self.cache_file.write_all(output.as_bytes()).await?;

        self.dirty = true;
        Ok(())
    }

    async fn poll(&mut self) -> Result<()> {
        if self.dirty {
            self.cache_file.flush().await?;
        }

        Ok(())
    }
}

#[derive(Debug)]
struct DownloadInfo {
    song_urls: Vec<String>,
    output_dir: PathBuf,
    cache: Cache,
}

impl DownloadInfo {
    async fn new(cfg: &Config, client: &Client, key: &str) -> Result<Self> {
        println!("Fetching playlist info...");
        let playlist_info = client
            .get(PLAYLIST_INFO_FETCH_URL)
            .query(&[
                ("part", "contentDetails"),
                ("fields", "items/contentDetails/itemCount"),
                ("id", &cfg.playlist_id),
                ("key", key),
            ])
            .send()
            .await?;

        let playlist_info: Value = serde_json::from_str(&playlist_info.text().await?)?;

        let size = playlist_info
            .get("items")
            .and_then(|items| items.get(0))
            .and_then(|first_onj| first_onj.get("contentDetails"))
            .and_then(|content_details| content_details.get("itemCount"))
            .and_then(Value::as_u64)
            .expect("Failed to get playlist size");

        println!("Playlist size: {}", size);

        let cache = Cache::get_or_create(cfg).await?;

        let song_urls = Self::init_song_list(size, cfg, client, key, &cache).await?;
        Ok(Self {
            song_urls,
            output_dir: cfg.output_dir.clone(),
            cache,
        })
    }

    async fn init_song_list(
        size: u64,
        cfg: &Config,
        client: &Client,
        key: &str,
        cache: &Cache,
    ) -> Result<Vec<String>> {
        let mut items = Vec::new();
        let mut fetched: u64 = 0;
        let mut page_token: Option<String> = None;
        let params = &[
            ("part", "contentDetails"),
            ("playlistId", &cfg.playlist_id),
            ("key", key),
            ("maxResults", &cfg.max_results.to_string()),
            ("fields", "items/contentDetails/videoId,nextPageToken"),
        ];

        println!("Fetching playlist items");

        while fetched < size {
            let req = client.get(PLAYLIST_ITEMS_FETCH_URL).query(params);

            let req = if let Some(ref token) = page_token {
                req.query(&[("pageToken", token)])
            } else {
                req
            };

            let playlist_items = req.send().await?;

            let json: Value = serde_json::from_str(&playlist_items.text().await?)?;

            if let Some(tkn) = json.get("nextPageToken") {
                page_token = tkn.as_str().map(str::to_owned);
            }

            // TODO: Maybe don't(?) create 64 strings...
            for item_id in json
                .get("items")
                .and_then(Value::as_array)
                .iter()
                .flat_map(|items| items.iter())
                .filter_map(|item| item.get("contentDetails"))
                .filter_map(|content_details| content_details.get("videoId"))
                .filter_map(Value::as_str)
            {
                fetched += 1;
                let url = format!("https://www.youtube.com/watch?v={}", item_id);
                if !cache.contains(&url) {
                    items.push(url);
                }
            }
        }

        println!(
            "Done fetching playlist items. {} new items found",
            items.len()
        );

        Ok(items)
    }

    async fn create_task(
        urls: &[String],
        cache: Arc<Mutex<Cache>>, // cursed type kekw
        outdir: PathBuf,
    ) -> Result<Output> {
        let mut cmd = Command::new("yt-dlp");
        let out = cmd
            .current_dir(outdir)
            .args(YT_DLP_ARGS)
            .args(urls)
            .output()
            .await?;

        cache.lock().await.emit(urls).await?;
        Ok(out)
    }

    async fn download(self) -> Result<()> {
        if self.song_urls.is_empty() {
            return Ok(());
        }

        if !self.output_dir.exists() {
            fs::create_dir_all(&self.output_dir).await?;
        }

        let outdir = self.output_dir;
        let batch_size = self.song_urls.len() / 10;
        let urls = Arc::new(self.song_urls);
        let cache = Arc::new(Mutex::new(self.cache));

        let mut js = JoinSet::new();
        for batch in (0..urls.len()).step_by(batch_size) {
            let urls = urls.clone();
            let cache = cache.clone();
            let outdir = outdir.clone();

            js.spawn(async move {
                // cursed slicing...
                let batch = &urls[batch..cmp::min(batch + batch_size, urls.len())];
                Self::create_task(batch, cache, outdir).await
            });
        }

        println!("Starting download of {} items", urls.len());

        while js.join_next().await.is_some() {}

        cache.lock().await.poll().await?;

        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenv::dotenv()?;
    let cfg = Config::parse();

    let key = dotenv::var("YOUTUBE_API_KEY")?;
    println!("Using API key: {}", key);

    let client = Client::builder().gzip(true).build()?;

    let mut interval = time::interval(Duration::from_secs(cfg.seconds_interval));

    loop {
        interval.tick().await;
        let info = DownloadInfo::new(&cfg, &client, &key).await?;
        info.download().await?;
        if !cfg.daemon {
            break;
        }
    }

    Ok(())
}
