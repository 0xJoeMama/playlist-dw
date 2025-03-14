use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::{
    cmp, env,
    path::PathBuf,
    process::{ExitStatus, Stdio},
    sync::Arc,
    thread,
    time::Duration,
};

use anyhow::{anyhow, Result};

use clap::Parser;
use reqwest::Client;
use serde_json::Value as JsonValue;
use tokio::fs;
use tokio::{
    fs::{File, OpenOptions},
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
    runtime::Builder as RuntimeBuilder,
    sync::Mutex,
    task::JoinSet,
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

enum DownloadFileEntry {
    PlaylistId(String),
    VideoId(String),
    File(PathBuf),
}

struct DownloadFile {
    path: PathBuf,
    entries: Vec<DownloadFileEntry>,
}

impl TryFrom<&PathBuf> for DownloadFile {
    type Error = anyhow::Error;

    fn try_from(value: &PathBuf) -> std::result::Result<Self, Self::Error> {
        let path = value
            .parent()
            .map(|it| it.to_path_buf())
            .ok_or(anyhow!("path problem"))?;

        let s = std::fs::read_to_string(value)?;
        let entries = s
            .lines()
            .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
            .filter_map(|line| {
                // cut comments out
                let line = line.split_once('#').map_or(line, |(a, _)| a);
                if let Some(loc) = line.strip_prefix("include ") {
                    Some(DownloadFileEntry::File(path.join(loc)))
                } else {
                    let (t, id) = line.split_once(": ")?;
                    let id = id.trim();
                    let t = t.trim();
                    match t.trim() {
                        "playlist" => Some(DownloadFileEntry::PlaylistId(id.to_owned())),
                        "song" => Some(DownloadFileEntry::VideoId(id.to_owned())),
                        _ => None,
                    }
                }
            })
            .collect();

        Ok(DownloadFile { path, entries })
    }
}

#[derive(Debug)]
struct DownloadEntry {
    path: PathBuf,
    url: String,
}

pub(crate) fn create_song_from_id(song_id: &str) -> String {
    format!("https://www.youtube.com/watch?v={song_id}")
}

impl DownloadFile {
    async fn eval_file(
        self,
        client: &Client,
        cfg: &Config,
        key: &str,
        cache: &Cache,
        res_buf: &mut Vec<DownloadEntry>,
    ) -> Result<()> {
        println!(
            "Evaluating input file in folder {}",
            self.path.to_string_lossy()
        );
        for entry in self.entries {
            match entry {
                DownloadFileEntry::PlaylistId(ref id) => res_buf.extend(
                    DownloadInfo::get_playlist_songs(client, cfg, id, key, cache, &self.path)
                        .await?,
                ),
                DownloadFileEntry::File(ref path_buf) => {
                    if let Ok(new_file) = DownloadFile::try_from(path_buf) {
                        Box::pin(new_file.eval_file(client, cfg, key, cache, res_buf))
                            .await
                            .expect("could not evaluate nested file")
                    }
                }
                DownloadFileEntry::VideoId(ref id) => res_buf.push(DownloadEntry {
                    path: self.path.to_path_buf(),
                    url: create_song_from_id(id),
                }),
            }
        }
        Ok(())
    }
}

#[derive(Debug, Parser)]
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
    /// The IDs of the target playlists. The playlist ID is part of the playlist link. To get it,
    /// open your playlist, check its link and copy everything after "link="
    playlist_ids: Vec<String>,
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
    /// Config file used to execute. This file contains lines with either playlist IDs or with
    /// include <path>, which includes in the current run a file from a different location.
    /// All playlists in one file are included in the directory their configs are located in.
    #[arg(required = true, last = true)]
    file: Option<PathBuf>,
}

/// Download cache
#[derive(Debug)]
struct Cache {
    // old cache stored as a set
    contents: HashMap<String, PathBuf>,
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

        if path.exists() {
            let mut cache_file = OpenOptions::new()
                .read(true) // we need read for now
                .write(true) // we need write for emit
                .append(true) // we open in append mode so as not to delete other cached files
                .open(path)
                .await?;

            // parse the contents of the existing file
            let contents = {
                let mut contents = String::new();
                cache_file.read_to_string(&mut contents).await?;

                contents
                    .lines()
                    .map(|line| {
                        let (url, path) = line.split_once(" : ").unwrap();
                        (url.trim().to_owned(), cfg.cache_file.join(path.trim()))
                    })
                    .collect()
            };

            Ok(Self {
                contents,
                cache_file,
                dirty: false,
            })
        } else {
            Ok(Self {
                contents: HashMap::new(),
                cache_file: File::create(path).await?,
                dirty: false,
            })
        }
    }

    async fn contains(&self, url: &str) -> bool {
        self.contents.contains_key(url)
    }

    /// Appends 'urls' to the end of the file, **without saving**.
    async fn emit(&mut self, downloads: &[DownloadEntry]) -> Result<()> {
        let mut output = downloads
            .iter()
            .map(|it| format!("{} : {}", it.url, it.path.to_string_lossy()))
            .collect::<Vec<_>>()
            .join("\n");
        output.push('\n');

        self.cache_file.write_all(output.as_bytes()).await?;

        self.dirty = true;
        Ok(())
    }

    // Saves the file, if there were changes to it.
    async fn save(&mut self) -> Result<()> {
        if self.dirty {
            self.cache_file.flush().await?;
            self.dirty = false;
        }

        Ok(())
    }
}

/// Required information from YouTube API and also the runtime config.
#[derive(Debug)]
struct DownloadInfo {
    pending_downloads: Vec<DownloadEntry>, // urls to be downloaded
    cache: Cache,                          // local cache
    max_tasks: usize,                      // max amount of tasks to start
}

impl DownloadInfo {
    async fn get_playlist_songs(
        client: &Client,
        cfg: &Config,
        id: &str,
        key: &str,
        cache: &Cache,
        output_dir: &Path,
    ) -> Result<Vec<DownloadEntry>> {
        println!("Gathering info for playlist {}", id);
        let playlist_info = client
            .get(PLAYLIST_INFO_FETCH_URL)
            .query(&[
                ("part", "contentDetails"),
                ("fields", "items/contentDetails/itemCount"),
                ("id", id),
                ("key", key),
            ])
            .send()
            .await?
            .json::<JsonValue>()
            .await?;

        let playlist_size = playlist_info
            .get("items")
            .and_then(|items| items.get(0))
            .and_then(|first_onj| first_onj.get("contentDetails"))
            .and_then(|content_details| content_details.get("itemCount"))
            .and_then(JsonValue::as_u64)
            .unwrap_or_else(|| panic!("could not get size of playlist {id}"));

        println!("Playlist size: {}", playlist_size);

        Self::init_song_list(
            playlist_size,
            id,
            cfg.max_results,
            output_dir,
            client,
            key,
            cache,
        )
        .await
    }

    // Collect the necessary information for a run.
    async fn collect(cfg: &Config, client: &Client, key: &str) -> Result<Self> {
        println!("Fetching playlist info...");

        let cache = Cache::from(cfg).await?;

        let mut pending_downloads = Vec::new();
        if let Some(ref input_file) = cfg.file {
            println!("Using file {} for input", input_file.to_str().unwrap());
            let download_file: DownloadFile = input_file.try_into()?;
            download_file
                .eval_file(client, cfg, key, &cache, &mut pending_downloads)
                .await
                .expect("could not evaluate input file");
        }

        for id in &cfg.playlist_ids {
            pending_downloads.extend(
                Self::get_playlist_songs(client, cfg, id, key, &cache, &cfg.output_dir)
                    .await
                    .expect("cannot initialize playlist stuff"),
            );
        }

        Ok(Self {
            pending_downloads,
            cache,
            max_tasks: cfg.max_tasks,
        })
    }

    async fn init_song_list(
        playlist_size: u64,
        playlist_id: &str,
        max_results: u8,
        output_dir: &Path,
        client: &Client,
        key: &str,
        cache: &Cache,
    ) -> Result<Vec<DownloadEntry>> {
        let mut downloads = Vec::new();
        let mut fetched_cnt: u64 = 0;
        let mut next_page_token: Option<String> = None;
        let req_params = &[
            ("part", "contentDetails"),
            ("playlistId", playlist_id),
            ("key", key),
            ("maxResults", &max_results.to_string()),
            // we ask for very specific fields to improve latency
            ("fields", "items/contentDetails/videoId,nextPageToken"),
        ];

        println!("Fetching playlist items");

        while fetched_cnt < playlist_size {
            let req = client.get(PLAYLIST_ITEMS_FETCH_URL).query(req_params);

            let req = if let Some(ref token) = next_page_token {
                req.query(&[("pageToken", token)])
            } else {
                req
            };

            let res = req.send().await?.json::<JsonValue>().await?;

            if let Some(tkn) = res.get("nextPageToken") {
                next_page_token = tkn.as_str().map(str::to_owned);
            }

            // TODO: Maybe don't(?) create strings...
            for song_id in res
                .get("items")
                .and_then(JsonValue::as_array)
                .iter()
                .flat_map(|items| items.iter())
                .filter_map(|item| item.get("contentDetails"))
                .filter_map(|content_details| content_details.get("videoId"))
            {
                fetched_cnt += 1;
                let entry = {
                    if let Some(song_id) = song_id.as_str() {
                        DownloadEntry {
                            url: create_song_from_id(song_id),
                            path: output_dir.to_path_buf(),
                        }
                    } else {
                        return Err(anyhow!("Could not properly parse playlistItems request"));
                    }
                };

                if !cache.contains(&entry.url).await {
                    downloads.push(entry);
                }
            }
        }

        println!(
            "Done fetching playlist items: {} new items found",
            downloads.len()
        );

        Ok(downloads)
    }

    async fn create_download_tasks(
        downloads: &[DownloadEntry],
        cache: Arc<Mutex<Cache>>, // cursed type kekw
    ) -> Result<Vec<ExitStatus>> {
        let mut stati = Vec::with_capacity(downloads.len());
        for entry in downloads {
            let mut cmd = Command::new("yt-dlp");
            fs::create_dir_all(&entry.path).await?;
            stati.push(
                cmd.current_dir(&entry.path)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .args(YT_DLP_ARGS)
                    .args([&entry.url])
                    .spawn()?
                    .wait()
                    .await?,
            );
        }
        cache.lock().await.emit(downloads).await?;
        println!("emitted cache");
        Ok(stati)
    }

    async fn download(self) -> Result<()> {
        if self.pending_downloads.is_empty() {
            return Ok(());
        }

        // fs::create_dir_all(&self.output_dir).await?;

        let batch_size = cmp::max(1, self.pending_downloads.len() / self.max_tasks);

        let downloads = Arc::new(self.pending_downloads);
        let cache = Arc::new(Mutex::new(self.cache));

        println!("Starting download of {} items", downloads.len());

        let mut js = (0..downloads.len())
            .step_by(batch_size)
            .map(|batch| {
                let urls = Arc::clone(&downloads);
                let cache = Arc::clone(&cache);

                async move {
                    let batch = &urls[batch..cmp::min(batch + batch_size, urls.len())];
                    Self::create_download_tasks(batch, cache).await
                }
            })
            .fold(JoinSet::new(), |mut js, next| {
                js.spawn(next);
                js
            });

        while js.join_next().await.is_some() {}

        println!("Done downloading. Writing cache.");
        cache.lock().await.save().await?;

        Ok(())
    }
}

fn main() {
    // initialize dev env vars
    #[cfg(debug_assertions)]
    dotenv::dotenv().expect("could not initialize environment");

    // parse command line arguments in a runtime config
    let cfg = Config::parse();

    // get the API key from the environment
    let key = env::var("YOUTUBE_API_KEY")
        .expect("The YOUTUBE_API_KEY environment variable should be set");
    let lock_fp: PathBuf = PathBuf::from(
        env::var("XDG_CACHE_HOME")
            .or(env::var("HOME"))
            .unwrap_or_else(|_| ".".to_owned()),
    )
    .join("playlist-dw.lock");

    #[cfg(debug_assertions)]
    println!("Lock file: {}", lock_fp.to_str().unwrap());

    let lockfile = std::fs::File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(lock_fp)
        .expect("could not acquire daemon lock");
    let mut lock = fd_lock::RwLock::new(lockfile);
    // we need to use an Option since otherwise this would exit even if we aren't running as a
    // daemon
    let _lock = if cfg.daemon {
        // exit the program if we cannot write:
        // it either means we cannot acquire the lock or that another instance is already running
        let mut lock = lock.try_write().expect("daemon lock is used by another process. If that is not true, delete the lock file and retry");
        writeln!(&mut lock, "{}", std::process::id()).expect("could not write to daemon lock");
        Some(lock)
    } else {
        None
    };

    #[cfg(debug_assertions)]
    println!("Using API key: {}", key);

    loop {
        let runtime = RuntimeBuilder::new_multi_thread()
            .worker_threads(cfg.max_tasks)
            .enable_all()
            .build()
            .expect("Could not build a tokio runtime");

        runtime.block_on(async {
            // HTTP client
            let client = Client::builder()
                .gzip(true)
                .no_proxy()
                .build()
                .expect("Could not create an HTTP client");

            // create the required metadata for the download
            // this includes collecting the urls as well as creating/parsing the cache that will be/is used.
            if let Ok(info) = DownloadInfo::collect(&cfg, &client, &key).await {
                // start the download using the collected info
                // since this might be daemon-ified we need to handle errors in this case
                match info.download().await {
                    Result::Ok(_) => println!("Finished"),
                    Result::Err(err) => eprintln!("{err}"),
                }
            }
        });

        drop(runtime);

        if !cfg.daemon {
            break;
        }

        thread::sleep(Duration::from_secs(cfg.seconds_interval));
    }
}
