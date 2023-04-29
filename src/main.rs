use std::{path::PathBuf, vec};

use anyhow::Result;

use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use ytd_rs::{Arg, YoutubeDL};

const PLAYLIST_INFO_FETCH_URL: &str = "https://www.googleapis.com/youtube/v3/playlists";
const PLAYLIST_ITEMS_FETCH_URL: &str = "https://www.googleapis.com/youtube/v3/playlistItems";

#[derive(Debug, Serialize, Deserialize)]
struct Config {
    output: Option<PathBuf>,
    output_dir: Option<PathBuf>,
    key: String,
    playlist_id: String,
    max_results: Option<u8>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            output: Some(PathBuf::from("cache.json")),
            output_dir: Some(PathBuf::from("songs")),
            key: String::new(),
            playlist_id: "PLk9ysb9-28aN419VMKtdkIe1WIDiSTIYh".to_owned(),
            max_results: Some(50),
        }
    }
}

#[derive(Debug)]
struct PlaylistInfo {
    song_urls: Vec<String>,
}

impl PlaylistInfo {
    fn new(cfg: &Config, client: &Client) -> Result<Self> {
        let playlist_info = client
            .get(PLAYLIST_INFO_FETCH_URL)
            .query(&[("id", &cfg.playlist_id)])
            .query(&[("part", "contentDetails")])
            .query(&[("key", &cfg.key)])
            .send()?;
        let playlist_info: Value = serde_json::from_str(&playlist_info.text()?)?;

        let size = playlist_info
            .get("items")
            .and_then(|items| items.get(0))
            .and_then(|first_onj| first_onj.get("contentDetails"))
            .and_then(|content_details| content_details.get("itemCount"))
            .and_then(Value::as_u64)
            .expect("Failed to get playlist size");

        let songs = Self::init_song_list(size, cfg, client)?;
        Ok(Self {
            // size,
            song_urls: songs,
        })
    }

    fn init_song_list(size: u64, cfg: &Config, client: &Client) -> Result<Vec<String>> {
        let mut items = Vec::new();
        let mut page_token: Option<String> = None;

        while items.len() < size.try_into()? {
            let req = client
                .get(PLAYLIST_ITEMS_FETCH_URL)
                .query(&[("playlistId", &cfg.playlist_id)])
                .query(&[("part", "contentDetails")])
                .query(&[("key", &cfg.key)])
                .query(&[("maxResults", &cfg.max_results)]);

            let req = if let Some(ref token) = page_token {
                req.query(&[("pageToken", token)])
            } else {
                req
            };

            let playlist_items = req.send()?;

            let json: Value = serde_json::from_str(&playlist_items.text()?)?;

            if let Some(tkn) = json.get("nextPageToken") {
                page_token = tkn.as_str().map(str::to_owned);
            }

            for item_id in json
                .get("items")
                .and_then(Value::as_array)
                .iter()
                .flat_map(|items| items.iter())
                .filter_map(|item| item.get("contentDetails"))
                .filter_map(|content_details| content_details.get("videoId"))
            {
                items.push(format!(
                    "https://www.youtube.com/watch?v={}",
                    item_id.as_str().unwrap()
                ));
            }
        }

        Ok(items)
    }
}

fn main() -> Result<()> {
    dotenv::dotenv()?;

    let cfg = Config {
        key: dotenv::var("key")?,
        ..Default::default()
    };
    let client = Client::builder().build()?;

    let info = PlaylistInfo::new(&cfg, &client)?;

    println!("{:#?}", info);
    let args = vec![
        Arg::new("--extract-audio"),
        Arg::new_with_arg("--audio-format", "mp3"),
        Arg::new("--add-metadata"),
        Arg::new_with_arg("--metadata-from-title", "%(artist) - %(title)s"),
        Arg::new("--write-sub"),
        Arg::new_with_arg("--output", "%(title).90s.%(ext)s"),
    ];

    let ytdlp = YoutubeDL::new_multiple_links(
        &cfg.output_dir.unwrap_or(PathBuf::from("songs")),
        args,
        info.song_urls,
    )?;
    ytdlp.download()?;

    Ok(())
}
