use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs;
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use thiserror::Error;
use tracing::warn;

use crate::config::{Config, MediaCategory, ShareConfig};

pub const ROOT_ID: &str = "0";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Catalog {
    entries: HashMap<String, Entry>,
    children: HashMap<String, Vec<String>>,
    update_id: u32,
    file_count: usize,
    total_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogScan {
    pub catalog: Catalog,
    pub unsettled_files: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub id: String,
    pub parent_id: String,
    pub title: String,
    pub path: PathBuf,
    pub kind: EntryKind,
    pub size: u64,
    pub modified_seconds: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryKind {
    Container,
    Media(MediaKind),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaKind {
    pub category: MediaCategory,
    pub mime_type: &'static str,
    pub upnp_class: &'static str,
    pub dlna_profile: Option<&'static str>,
}

#[derive(Debug, Error)]
pub enum ScanError {
    #[error("failed to resolve share {name:?} at {path}: {source}")]
    ResolveShare {
        name: String,
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("catalog ID collision between {first} and {second}")]
    IdCollision { first: PathBuf, second: PathBuf },
}

impl Catalog {
    pub fn empty() -> Self {
        Self {
            entries: HashMap::new(),
            children: HashMap::from([(ROOT_ID.into(), Vec::new())]),
            update_id: 0,
            file_count: 0,
            total_bytes: 0,
        }
    }

    pub fn scan(config: &Config) -> Result<Self, ScanError> {
        Ok(Self::scan_with_minimum_age(config, Duration::ZERO)?.catalog)
    }

    pub fn scan_settled(config: &Config) -> Result<CatalogScan, ScanError> {
        Self::scan_with_minimum_age(config, config.settle_time())
    }

    fn scan_with_minimum_age(
        config: &Config,
        minimum_age: Duration,
    ) -> Result<CatalogScan, ScanError> {
        let mut catalog = Self::empty();
        let mut unsettled_files = 0;
        for share in &config.shares {
            unsettled_files += catalog.scan_share(
                share,
                config.scanner.recursive,
                config.scanner.follow_symlinks,
                config.scanner.include_hidden,
                minimum_age,
            )?;
        }
        catalog.sort_children();
        catalog.update_id = catalog.content_fingerprint();
        Ok(CatalogScan {
            catalog,
            unsettled_files,
        })
    }

    pub fn entry(&self, id: &str) -> Option<&Entry> {
        self.entries.get(id)
    }

    pub fn children(&self, id: &str) -> Option<impl Iterator<Item = &Entry>> {
        self.children
            .get(id)
            .map(|ids| ids.iter().filter_map(|child_id| self.entries.get(child_id)))
    }

    pub fn child_count(&self, id: &str) -> usize {
        self.children.get(id).map_or(0, Vec::len)
    }

    pub fn update_id(&self) -> u32 {
        self.update_id
    }

    pub fn object_update_id(&self, id: &str) -> u32 {
        let Some(children) = self.children.get(id) else {
            return self.update_id;
        };
        let mut hash = 0xcbf2_9ce4_8422_2325;
        hash_value(&mut hash, id.as_bytes());
        for child_id in children {
            hash_value(&mut hash, child_id.as_bytes());
            if let Some(entry) = self.entries.get(child_id) {
                hash_value(&mut hash, entry.title.as_bytes());
                hash_value(&mut hash, &entry.modified_seconds.to_le_bytes());
                hash_value(&mut hash, &entry.size.to_le_bytes());
            }
        }
        fold_update_id(hash)
    }

    pub fn file_count(&self) -> usize {
        self.file_count
    }

    pub fn total_bytes(&self) -> u64 {
        self.total_bytes
    }

    pub fn has_same_content(&self, other: &Self) -> bool {
        self.entries == other.entries && self.children == other.children
    }

    fn scan_share(
        &mut self,
        share: &ShareConfig,
        recursive: bool,
        follow_symlinks: bool,
        include_hidden: bool,
        minimum_age: Duration,
    ) -> Result<usize, ScanError> {
        let root = share
            .path
            .canonicalize()
            .map_err(|source| ScanError::ResolveShare {
                name: share.name.clone(),
                path: share.path.clone(),
                source,
            })?;
        let root_hash = stable_os_hash(root.as_os_str());
        let root_id = object_id('d', root_hash, Path::new(""));
        let metadata = fs::metadata(&root).map_err(|source| ScanError::ResolveShare {
            name: share.name.clone(),
            path: root.clone(),
            source,
        })?;
        self.insert_entry(Entry {
            id: root_id.clone(),
            parent_id: ROOT_ID.into(),
            title: share.name.clone(),
            path: root.clone(),
            kind: EntryKind::Container,
            size: 0,
            modified_seconds: modified_seconds(&metadata),
        })?;

        let mut pending = vec![(root.clone(), root_id)];
        let mut visited = HashSet::new();
        let mut unsettled_files = 0;
        while let Some((directory, parent_id)) = pending.pop() {
            if follow_symlinks {
                match directory.canonicalize() {
                    Ok(canonical) => {
                        if !visited.insert(canonical) {
                            continue;
                        }
                    }
                    Err(error) => {
                        warn!(path = %directory.display(), %error, "failed to resolve directory");
                        continue;
                    }
                }
            }
            let read_dir = match fs::read_dir(&directory) {
                Ok(read_dir) => read_dir,
                Err(error) => {
                    warn!(path = %directory.display(), %error, "failed to read directory");
                    continue;
                }
            };
            for directory_entry in read_dir {
                let directory_entry = match directory_entry {
                    Ok(entry) => entry,
                    Err(error) => {
                        warn!(path = %directory.display(), %error, "failed to read directory entry");
                        continue;
                    }
                };
                let file_name = directory_entry.file_name();
                if !include_hidden && is_hidden(&file_name) {
                    continue;
                }
                let path = directory_entry.path();
                let metadata = match if follow_symlinks {
                    fs::metadata(&path)
                } else {
                    fs::symlink_metadata(&path)
                } {
                    Ok(metadata) => metadata,
                    Err(error) => {
                        warn!(path = %path.display(), %error, "failed to inspect media path");
                        continue;
                    }
                };
                if metadata.file_type().is_symlink() && !follow_symlinks {
                    continue;
                }
                let relative = path.strip_prefix(&root).unwrap_or(&path);
                let title = file_name.to_string_lossy().into_owned();
                if metadata.is_dir() {
                    if !recursive {
                        continue;
                    }
                    let id = object_id('d', root_hash, relative);
                    self.insert_entry(Entry {
                        id: id.clone(),
                        parent_id: parent_id.clone(),
                        title,
                        path: path.clone(),
                        kind: EntryKind::Container,
                        size: 0,
                        modified_seconds: modified_seconds(&metadata),
                    })?;
                    pending.push((path, id));
                } else if metadata.is_file()
                    && let Some(media_kind) = media_kind(&path)
                    && share.media.contains(&media_kind.category)
                {
                    if !is_settled(&metadata, minimum_age) {
                        unsettled_files += 1;
                        continue;
                    }
                    let title = path
                        .file_stem()
                        .map_or_else(|| title.clone(), |stem| stem.to_string_lossy().into_owned());
                    let id = object_id('f', root_hash, relative);
                    self.file_count += 1;
                    self.total_bytes = self.total_bytes.saturating_add(metadata.len());
                    self.insert_entry(Entry {
                        id,
                        parent_id: parent_id.clone(),
                        title,
                        path,
                        kind: EntryKind::Media(media_kind),
                        size: metadata.len(),
                        modified_seconds: modified_seconds(&metadata),
                    })?;
                }
            }
        }
        Ok(unsettled_files)
    }

    fn insert_entry(&mut self, entry: Entry) -> Result<(), ScanError> {
        if let Some(previous) = self.entries.get(&entry.id) {
            return Err(ScanError::IdCollision {
                first: previous.path.clone(),
                second: entry.path,
            });
        }
        self.children
            .entry(entry.parent_id.clone())
            .or_default()
            .push(entry.id.clone());
        if matches!(entry.kind, EntryKind::Container) {
            self.children.entry(entry.id.clone()).or_default();
        }
        self.entries.insert(entry.id.clone(), entry);
        Ok(())
    }

    fn sort_children(&mut self) {
        for child_ids in self.children.values_mut() {
            child_ids.sort_unstable_by(|left, right| {
                let left = &self.entries[left];
                let right = &self.entries[right];
                entry_rank(&left.kind)
                    .cmp(&entry_rank(&right.kind))
                    .then_with(|| left.title.cmp(&right.title))
                    .then_with(|| left.id.cmp(&right.id))
            });
        }
    }

    fn content_fingerprint(&self) -> u32 {
        let mut entries: Vec<_> = self.entries.values().collect();
        entries.sort_unstable_by(|left, right| left.id.cmp(&right.id));
        let mut hash = 0xcbf2_9ce4_8422_2325;
        for entry in entries {
            hash_value(&mut hash, entry.id.as_bytes());
            hash_value(&mut hash, entry.parent_id.as_bytes());
            hash_value(&mut hash, entry.title.as_bytes());
            hash_value(&mut hash, &entry.modified_seconds.to_le_bytes());
            hash_value(&mut hash, &entry.size.to_le_bytes());
        }
        fold_update_id(hash)
    }
}

fn entry_rank(kind: &EntryKind) -> u8 {
    match kind {
        EntryKind::Container => 0,
        EntryKind::Media(_) => 1,
    }
}

fn is_hidden(name: &OsStr) -> bool {
    #[cfg(unix)]
    {
        name.as_bytes().first() == Some(&b'.')
    }
    #[cfg(windows)]
    {
        name.encode_wide().next() == Some(u16::from(b'.'))
    }
    #[cfg(not(any(unix, windows)))]
    {
        name.to_string_lossy().starts_with('.')
    }
}

fn modified_seconds(metadata: &fs::Metadata) -> u64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |duration| duration.as_secs())
}

fn is_settled(metadata: &fs::Metadata, minimum_age: Duration) -> bool {
    minimum_age.is_zero()
        || metadata.modified().is_ok_and(|modified| {
            SystemTime::now()
                .duration_since(modified)
                .is_ok_and(|age| age >= minimum_age)
        })
}

fn object_id(kind: char, root_hash: u64, relative: &Path) -> String {
    let path_hash = stable_os_hash(relative.as_os_str());
    format!("{kind}{root_hash:016x}{path_hash:016x}")
}

fn stable_os_hash(value: &OsStr) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325;
    #[cfg(unix)]
    extend_hash(&mut hash, value.as_bytes());
    #[cfg(windows)]
    for unit in value.encode_wide() {
        extend_hash(&mut hash, &unit.to_le_bytes());
    }
    #[cfg(not(any(unix, windows)))]
    extend_hash(&mut hash, value.to_string_lossy().as_bytes());
    hash
}

fn hash_value(hash: &mut u64, value: &[u8]) {
    extend_hash(hash, value);
    *hash = (*hash ^ 0xff).wrapping_mul(0x0000_0100_0000_01b3);
}

fn extend_hash(hash: &mut u64, value: &[u8]) {
    for byte in value {
        *hash = (*hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3);
    }
}

fn fold_update_id(hash: u64) -> u32 {
    (hash ^ (hash >> 32)) as u32
}

pub fn media_kind(path: &Path) -> Option<MediaKind> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    let kind = match extension.as_str() {
        "mp3" => MediaKind {
            category: MediaCategory::Audio,
            mime_type: "audio/mpeg",
            upnp_class: "object.item.audioItem.musicTrack",
            dlna_profile: None,
        },
        "m4a" | "mp4a" => MediaKind {
            category: MediaCategory::Audio,
            mime_type: "audio/mp4",
            upnp_class: "object.item.audioItem.musicTrack",
            dlna_profile: None,
        },
        "aac" => MediaKind {
            category: MediaCategory::Audio,
            mime_type: "audio/aac",
            upnp_class: "object.item.audioItem.musicTrack",
            dlna_profile: None,
        },
        "flac" => MediaKind {
            category: MediaCategory::Audio,
            mime_type: "audio/flac",
            upnp_class: "object.item.audioItem.musicTrack",
            dlna_profile: None,
        },
        "wav" => MediaKind {
            category: MediaCategory::Audio,
            mime_type: "audio/wav",
            upnp_class: "object.item.audioItem.musicTrack",
            dlna_profile: None,
        },
        "ogg" | "oga" => MediaKind {
            category: MediaCategory::Audio,
            mime_type: "audio/ogg",
            upnp_class: "object.item.audioItem.musicTrack",
            dlna_profile: None,
        },
        "mp4" | "m4v" => MediaKind {
            category: MediaCategory::Video,
            mime_type: "video/mp4",
            upnp_class: "object.item.videoItem",
            dlna_profile: None,
        },
        "mkv" => MediaKind {
            category: MediaCategory::Video,
            mime_type: "video/x-matroska",
            upnp_class: "object.item.videoItem",
            dlna_profile: None,
        },
        "avi" => MediaKind {
            category: MediaCategory::Video,
            mime_type: "video/x-msvideo",
            upnp_class: "object.item.videoItem",
            dlna_profile: None,
        },
        "mpeg" | "mpg" | "mpe" => MediaKind {
            category: MediaCategory::Video,
            mime_type: "video/mpeg",
            upnp_class: "object.item.videoItem",
            dlna_profile: None,
        },
        "ts" | "m2ts" | "mts" => MediaKind {
            category: MediaCategory::Video,
            mime_type: "video/mp2t",
            upnp_class: "object.item.videoItem",
            dlna_profile: None,
        },
        "mov" => MediaKind {
            category: MediaCategory::Video,
            mime_type: "video/quicktime",
            upnp_class: "object.item.videoItem",
            dlna_profile: None,
        },
        "webm" => MediaKind {
            category: MediaCategory::Video,
            mime_type: "video/webm",
            upnp_class: "object.item.videoItem",
            dlna_profile: None,
        },
        "jpg" | "jpeg" => MediaKind {
            category: MediaCategory::Image,
            mime_type: "image/jpeg",
            upnp_class: "object.item.imageItem.photo",
            dlna_profile: None,
        },
        "png" => MediaKind {
            category: MediaCategory::Image,
            mime_type: "image/png",
            upnp_class: "object.item.imageItem.photo",
            dlna_profile: None,
        },
        "gif" => MediaKind {
            category: MediaCategory::Image,
            mime_type: "image/gif",
            upnp_class: "object.item.imageItem.photo",
            dlna_profile: None,
        },
        _ => return None,
    };
    Some(kind)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;
    use test_case::test_case;

    use super::*;
    use crate::config::{NetworkConfig, ScannerConfig};

    #[test_case("song.mp3", Some("audio/mpeg") ; "mp3")]
    #[test_case("movie.MKV", Some("video/x-matroska") ; "case insensitive")]
    #[test_case("notes.txt", None ; "unsupported")]
    fn recognizes_media(name: &str, expected: Option<&str>) {
        assert_eq!(
            media_kind(Path::new(name)).map(|kind| kind.mime_type),
            expected
        );
    }

    #[test]
    fn builds_virtual_share_hierarchy() {
        let temp = tempdir().unwrap();
        fs::create_dir(temp.path().join("Season 1")).unwrap();
        fs::write(temp.path().join("Season 1/Episode.mkv"), b"video").unwrap();
        fs::write(temp.path().join("ignore.txt"), b"text").unwrap();
        let config = Config {
            friendly_name: "Test".into(),
            network: NetworkConfig::default(),
            scanner: ScannerConfig::default(),
            shares: vec![ShareConfig {
                name: "Shows".into(),
                path: temp.path().into(),
                media: MediaCategory::ALL.into(),
            }],
        };

        let catalog = Catalog::scan(&config).unwrap();

        assert_eq!(catalog.file_count(), 1);
        assert_eq!(catalog.children(ROOT_ID).unwrap().count(), 1);
        let share = catalog.children(ROOT_ID).unwrap().next().unwrap();
        assert_eq!(share.title, "Shows");
        assert!(matches!(share.kind, EntryKind::Container));
    }

    #[test]
    fn update_id_is_stable_for_unchanged_content() {
        let temp = tempdir().unwrap();
        fs::write(temp.path().join("song.mp3"), b"audio").unwrap();
        let config = Config {
            friendly_name: "Test".into(),
            network: NetworkConfig::default(),
            scanner: ScannerConfig::default(),
            shares: vec![ShareConfig {
                name: "Music".into(),
                path: temp.path().into(),
                media: MediaCategory::ALL.into(),
            }],
        };

        let first = Catalog::scan(&config).unwrap();
        let second = Catalog::scan(&config).unwrap();

        assert_eq!(first.update_id(), second.update_id());
        assert_ne!(first.update_id(), 0);
    }

    #[test]
    fn share_filters_media_categories() {
        let temp = tempdir().unwrap();
        fs::write(temp.path().join("movie.mp4"), b"video").unwrap();
        fs::write(temp.path().join("song.mp3"), b"audio").unwrap();
        let config = Config {
            friendly_name: "Test".into(),
            network: NetworkConfig::default(),
            scanner: ScannerConfig::default(),
            shares: vec![ShareConfig {
                name: "Videos".into(),
                path: temp.path().into(),
                media: vec![MediaCategory::Video],
            }],
        };

        let catalog = Catalog::scan(&config).unwrap();

        assert_eq!(catalog.file_count(), 1);
        let share = catalog.children(ROOT_ID).unwrap().next().unwrap();
        let item = catalog.children(&share.id).unwrap().next().unwrap();
        assert_eq!(item.title, "movie");
    }

    #[test]
    fn non_recursive_scan_only_includes_root_files() {
        let temp = tempdir().unwrap();
        fs::create_dir(temp.path().join("nested")).unwrap();
        fs::write(temp.path().join("root.mp4"), b"video").unwrap();
        fs::write(temp.path().join("nested/child.mp4"), b"video").unwrap();
        let config = Config {
            friendly_name: "Test".into(),
            network: NetworkConfig::default(),
            scanner: ScannerConfig {
                recursive: false,
                ..ScannerConfig::default()
            },
            shares: vec![ShareConfig {
                name: "Videos".into(),
                path: temp.path().into(),
                media: vec![MediaCategory::Video],
            }],
        };

        let catalog = Catalog::scan(&config).unwrap();

        assert_eq!(catalog.file_count(), 1);
        let share = catalog.children(ROOT_ID).unwrap().next().unwrap();
        let children = catalog.children(&share.id).unwrap().collect::<Vec<_>>();
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].title, "root");
    }

    #[test]
    fn settled_scan_withholds_recently_modified_media() {
        let temp = tempdir().unwrap();
        fs::write(temp.path().join("encoding.mp4"), b"partial video").unwrap();
        let config = Config {
            friendly_name: "Test".into(),
            network: NetworkConfig::default(),
            scanner: ScannerConfig::default(),
            shares: vec![ShareConfig {
                name: "Videos".into(),
                path: temp.path().into(),
                media: vec![MediaCategory::Video],
            }],
        };

        let scan = Catalog::scan_settled(&config).unwrap();

        assert_eq!(scan.unsettled_files, 1);
        assert_eq!(scan.catalog.file_count(), 0);
        assert_eq!(Catalog::scan(&config).unwrap().file_count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn unix_path_hash_preserves_raw_byte_identity() {
        assert_eq!(stable_os_hash(OsStr::new("abc")), 0xe71f_a219_0541_574b);
    }
}
