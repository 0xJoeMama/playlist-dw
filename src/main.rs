use std::{
    cmp,
    collections::HashSet,
    env,
    path::PathBuf,
    process::{Output, Stdio},
    sync::Arc,
    time::Duration,
};

use anyhow::{Ok, Result};

use clap::Parser;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::{
    fs::{self, File, OpenOptions},
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
    sync::RwLock,
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
#[command(author, version, about)]
struct Config {
    /// Where the downloaded songs will be placed. You probably want this to be your local music
    /// library
    #[arg(short, long, default_value = "songs")]
    output_dir: PathBuf,
    /// Where download cache will be put. This should probably be near the songs folder.
    #[arg(short, long)]
    #[clap(default_value = "cache")]
    cache_file: PathBuf,
    /// The ID of the target playlist. The playlist ID is part of the playlist link. To get it,
    /// open your playlist, check its link and copy everything after "link="
    playlist_id: String,
    /// The maximum amount of results the YouTube API should respond with.
    /// Higher numbers increase request latency, but decrease request amounts.
    /// Set to the maximum by default.
    #[arg(short, long, default_value_t = 50)]
    max_results: u8,
    /// Whether the app should run as a daemon process.
    /// If run as a daemon, it stays active in the background and reruns every set amount of
    /// seconds.
    #[arg(short, long, default_value_t = false)]
    daemon: bool,
    /// How often the app should run, if in daemon mode.
    #[arg(short, long, default_value_t = 7200)]
    seconds_interval: u64,
    /// Maximum amount of tasks the app should launch on download.
    /// Higher numbers make downloads faster, but take up more CPU cycles(stress out the computer
    /// more).
    #[arg(long, default_value_t = 10)]
    max_tasks: usize,
}

/// Download cache
#[derive(Debug)]
struct Cache {
    // old cache stored as a set
    inner: HashSet<String>,
    // file that is kept open while the app is running
    cache_file: File,
    // whether there has been an update
    dirty: bool,
}

impl Cache {
    /// Create a [Cache] object using the provided runtime config.
    /// Will attempt to parse an existing file, but if it fails it will create a new file.
    async fn from(cfg: &Config) -> Result<Self> {
        let path = &cfg.cache_file;
        // if it exists parse it
        if path.exists() {
            let mut file = OpenOptions::new()
                .read(true) // we need read for now
                .write(true) // we need write for emit
                .append(true) // we open in append mode so as not to delete other cached files
                .open(path)
                .await?;

            // parse the contents of the existing file
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
            // otherwise create it anew
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

    /// Appends 'urls' to the end of the file, **without saving**.
    async fn emit(&mut self, urls: &[String]) -> Result<()> {
        let mut output = urls.join("\n");
        output.push('\n');

        self.cache_file.write_all(output.as_bytes()).await?;

        self.dirty = true;
        Ok(())
    }

    // Saves the file, if there were changes to it.
    async fn poll(&mut self) -> Result<()> {
        if self.dirty {
            self.cache_file.flush().await?;
        }

        Ok(())
    }
}

/// Required information from YouTube API and also the runtime config.
#[derive(Debug)]
struct DownloadInfo {
    song_urls: Vec<String>, // urls to be downloaded
    output_dir: PathBuf,    // the output directory
    cache: Cache,           // local cache
    max_tasks: usize,       // max amount of tasks to start
}

impl DownloadInfo {
    // Collect the necessary information for a run.
    async fn collect(cfg: &Config, client: &Client, key: &str) -> Result<Self> {
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
            .await?
            .json::<Value>()
            .await?;

        let size = playlist_info
            .get("items")
            .and_then(|items| items.get(0))
            .and_then(|first_onj| first_onj.get("contentDetails"))
            .and_then(|content_details| content_details.get("itemCount"))
            .and_then(Value::as_u64)
            .expect("Failed to get playlist size");

        println!("Playlist size: {}", size);

        let cache = Cache::from(cfg).await?;

        let song_urls = Self::init_song_list(size, cfg, client, key, &cache).await?;

        Ok(Self {
            song_urls,
            output_dir: cfg.output_dir.clone(),
            cache,
            max_tasks: cfg.max_tasks,
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
            // we ask for very specific fields to improve latency
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

            let json = req.send().await?.json::<Value>().await?;

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
            "Done fetching playlist items: {} new items found",
            items.len()
        );

        Ok(items)
    }

    async fn create_task(
        urls: &[String],
        cache: Arc<RwLock<Cache>>, // cursed type kekw
        outdir: PathBuf,
    ) -> Result<Output> {
        let mut cmd = Command::new("yt-dlp");
        let out = cmd
            .current_dir(outdir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .args(YT_DLP_ARGS)
            .args(urls)
            .output()
            .await?;

        cache.write().await.emit(urls).await?;
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
        let batch_size = self.song_urls.len() / self.max_tasks + 1;
        let urls = Arc::new(self.song_urls);
        let cache = Arc::new(RwLock::new(self.cache));

        println!("Starting download of {} items", urls.len());

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

        while js.join_next().await.is_some() {}

        println!("Done downloading. Writing cache.");
        cache.write().await.poll().await?;

        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // initialize dev env vars
    #[cfg(debug_assertions)]
    dotenv::dotenv()?;

    // parse command line arguments in a runtime config
    let cfg = Config::parse();

    // get the API key from the environment
    let key = env::var("YOUTUBE_API_KEY")?;
    #[cfg(debug_assertions)]
    println!("Using API key: {}", key);

    // HTTP client
    let client = Client::builder().gzip(true).build()?;

    // create an interval which will be used in case this was run as a daemon process
    let mut interval = time::interval(Duration::from_secs(cfg.seconds_interval));

    loop {
        // the first turn, this will run immediately
        interval.tick().await;
        // create the required metadata for the download
        // this includes collecting the urls as well as creating/parsing the cache that will be/is used.
        if let Result::Ok(info) = DownloadInfo::collect(&cfg, &client, &key).await {
            // induce the download using the collected info
            // since this might be daemon-ified we need to handle errors in this case
            match info.download().await {
                Result::Ok(_) => println!("Success"),
                Err(err) => eprintln!("Failed to download: {err}"),
            }
        }

        if !cfg.daemon {
            break;
        }
    }

    Ok(())
}
