use bytes::Bytes;
use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;
use std::str::FromStr;
use std::{fs, io};

use anyhow::Result;
use futures::StreamExt;
use futures::TryStreamExt;
use indicatif::MultiProgress;
use indicatif::ProgressBar;
use indicatif::ProgressState;
use indicatif::ProgressStyle;
use librespot::core::session::Session;
use librespot::playback::config::PlayerConfig;
use librespot::playback::mixer::NoOpVolume;
use librespot::playback::mixer::VolumeGetter;
use librespot::playback::player::Player;

use crate::channel_sink::ChannelSink;
use crate::encoder::Format;
use crate::encoder::Samples;
use crate::channel_sink::SinkEvent;
use crate::track::Track;
use crate::track::TrackMetadata;

pub struct Downloader {
    player_config: PlayerConfig,
    session: Session,
    progress_bar: MultiProgress,
}

#[derive(Debug, Clone)]
pub struct DownloadOptions {
    pub destination: PathBuf,
    pub compression: Option<u32>,
    pub parallel: usize,
    pub format: Format,
    pub folder_structure: FolderStructure
}

impl DownloadOptions {
    pub fn new(
        destination: Option<String>,
        compression: Option<u32>,
        parallel: usize,
        format: Format,
        folder_structure: FolderStructure
    ) -> Self {
        let destination =
            destination.map_or_else(|| std::env::current_dir().unwrap(), PathBuf::from);
        DownloadOptions {
            destination,
            compression,
            parallel,
            format,
            folder_structure
        }
    }
}

#[derive(Debug, Clone)]
pub enum FolderStructure {
    FLAT,
    ALBUM,
}

impl FromStr for FolderStructure {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> std::result::Result<Self, <Self as FromStr>::Err> {
        match s.to_uppercase().as_str() {
            "FLAT" => Ok(Self::FLAT),
            "ALBUM" => Ok(Self::ALBUM),
            _ => Err(anyhow::anyhow!("Unrecognized structure: {}", s))
        }
    }
}

impl Downloader {
    pub fn new(session: Session) -> Self {
        Downloader {
            player_config: PlayerConfig::default(),
            session,
            progress_bar: MultiProgress::new(),
        }
    }

    pub async fn download_tracks(
        self,
        tracks: Vec<Track>,
        options: &DownloadOptions,
    ) -> Result<()> {
        futures::stream::iter(tracks)
            .map(|track| {
                self.download_track(track, options)
            })
            .buffer_unordered(options.parallel)
            .try_collect::<Vec<_>>()
            .await?;

        Ok(())
    }

    #[tracing::instrument(name = "download_track", skip(self))]
    async fn download_track(&self, track: Track, options: &DownloadOptions) -> Result<()> {
        let pb = self.progress_bar.add(ProgressBar::new(1));
        pb.set_style(ProgressStyle::with_template("{spinner:.green} {msg} [{elapsed_precise}] [{wide_bar:.cyan/blue}] {bytes}/{total_bytes} ({eta})")?
            .with_key("eta", |state: &ProgressState, w: &mut dyn Write| write!(w, "{:.1}s", state.eta().as_secs_f64()).unwrap())
            .progress_chars("#>-"));

        let link_path = options.destination.join(format!(".index/{:?}", track.id.id));
        let link_path = link_path.as_path();

        if let Some(parent) = link_path.parent() {
            if !parent.exists() {
                fs::create_dir_all(parent)?;
            }
        }

        if link_path.is_symlink() {
            let dest = fs::read_link(link_path)?;
            if dest.exists() {
                pb.set_message(format!("{}", dest.to_str().unwrap_or_default()));
                return Ok(())
            }
            else {
                fs::remove_file(link_path)?;
            }
        }

        let metadata = track.metadata(&self.session).await?;
        tracing::info!("Downloading track: {:?}", metadata);

        let file_name = self.get_file_name(&metadata, FolderStructure::ALBUM);
        let path = options
            .destination
            .join(file_name.clone())
            .with_extension(options.format.extension())
            .to_str()
            .ok_or(anyhow::anyhow!("Could not set the output path"))?
            .to_string();

        pb.set_message(file_name.clone());

        let (sink, mut sink_channel) = ChannelSink::new(&metadata);

        let file_size = sink.get_approximate_size();
        pb.set_length(file_size as u64);

        let player = Player::new(
            self.player_config.clone(),
            self.session.clone(),
            self.volume_getter(),
            move || Box::new(sink),
        );

        let album_art = self.fetch_album_art(&metadata).await?;

        pb.enable_steady_tick(Duration::from_millis(100));

        player.load(track.id, true, 0);

        let mut samples = Vec::<i32>::new();

        tokio::spawn(async move {
            player.await_end_of_track().await;
            player.stop();
        });

        while let Some(event) = sink_channel.recv().await {
            match event {
                SinkEvent::Write { bytes, total, mut content } => {
                    tracing::trace!("Written {} bytes out of {}", bytes, total);
                    pb.set_position(bytes as u64);
                    samples.append(&mut content);
                }
                SinkEvent::Finished => {
                    tracing::info!("Finished downloading track: {:?}", file_name);
                    break;
                }
            }
        }

        tracing::info!("Encoding track: {:?}", file_name);
        pb.set_message(format!("Encoding {}", &file_name));
        let samples = Samples::new(samples, 44100, 2, 16);
        let encoder = crate::encoder::get_encoder(options.format);
        let stream = encoder.encode(samples, metadata, album_art).await?;

        pb.set_message(format!("Writing {}", &file_name));
        tracing::info!("Writing track: {:?} to file: {}", file_name, &path);
        stream.write_to_file(&path).await?;

        write_link(&path, &link_path)?;

        pb.finish_with_message(format!("Downloaded {}", &file_name));
        Ok(())
    }

    fn volume_getter(&self) -> Box<dyn VolumeGetter + Send> {
        Box::new(NoOpVolume)
    }

    fn get_file_name(&self, metadata: &TrackMetadata, structure: FolderStructure) -> String {
        // If there is more than 3 artists, add the first 3 and add "and others" at the end
        let artists = metadata
                .artists
                .iter()
            .map(|artist| artist.name.clone());

        let artists_name = if artists.len() > 3 {
            artists
                .take(3)
                .chain(["and others".to_string()])
                .collect::<Vec<_>>()
                .join(", ")
        } else {
            artists.collect::<Vec<String>>().join(", ")
        };

        let album_artist = metadata
            .artists
            .iter()
            .take(1)
            .map(|artist| artist.name.clone())
            .collect::<Vec<String>>()
            .join(", ");


        let parts = match structure {
            FolderStructure::FLAT => vec![
                format!("{} - {}", artists_name, metadata.track_name)
            ],
            FolderStructure::ALBUM => vec![
                album_artist,
                match metadata.album.num_discs {
                    1 => metadata.album.name.clone(),
                    _ => format!("{} (Disc {})", metadata.album.name, metadata.disc_number)
                },
                format!("{:0>2} {}", metadata.number, metadata.track_name)
            ]
        };

        parts.into_iter()
            .map(|part|  self.clean_file_name(part))
            .collect::<Vec<_>>()
            .join("/")
    }

    fn clean_file_name(&self, file_name: String) -> String {
        let invalid_chars = ['<', '>', ':', '\'', '"', '/', '\\', '|', '?', '*'];
        let mut clean = String::new();

        // Everything but Windows should allow non-ascii characters
        let allows_non_ascii = !cfg!(windows);
        for c in file_name.chars() {
            if !invalid_chars.contains(&c) && (c.is_ascii() || allows_non_ascii) && !c.is_control()
            {
                clean.push(c);
            }
        }
        clean
    }

    async fn fetch_album_art(&self, metadata: &TrackMetadata) -> Result<Bytes>{
        match metadata.album.cover {
            Some(ref cover) => {
                self.session.spclient()
                    .get_image(&cover.id)
                    .await
                    .map_err(|e| anyhow::anyhow!("{:?}", e))
            }
            None => Err(anyhow::anyhow!("No cover art!"))
        }
    }
}

fn write_link<P: AsRef<Path>, Q: AsRef<Path>>(original: P, link: Q) -> io::Result<()> {
    if cfg!(windows) {
        #[cfg(any(windows, doc))] {
            use std::os::windows::fs::symlink_file;
            symlink_file(&original, &link)?;
        }
    }
    else {
        #[cfg(any(unix, doc))] {
            use std::os::unix::fs::symlink;
            symlink(&original, &link)?;
        }
    }
    Ok(())
}
