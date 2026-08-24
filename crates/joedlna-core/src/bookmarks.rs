use std::collections::BTreeMap;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::Mutex;

#[derive(Debug, Clone)]
pub struct BookmarkStore {
    path: PathBuf,
    state: Arc<Mutex<BookmarkFile>>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct BookmarkFile {
    positions: BTreeMap<String, u64>,
}

#[derive(Debug, Error)]
pub enum BookmarkError {
    #[error("failed to read bookmarks from {path}: {source}")]
    Read { path: PathBuf, source: io::Error },
    #[error("failed to parse bookmarks from {path}: {source}")]
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    #[error("failed to serialize bookmarks: {0}")]
    Serialize(#[from] toml::ser::Error),
    #[error("failed to write bookmarks to {path}: {source}")]
    Write { path: PathBuf, source: io::Error },
}

impl BookmarkStore {
    pub async fn load(path: PathBuf) -> Result<Self, BookmarkError> {
        let state = match tokio::fs::read_to_string(&path).await {
            Ok(source) => toml::from_str(&source).map_err(|source| BookmarkError::Parse {
                path: path.clone(),
                source,
            })?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => BookmarkFile::default(),
            Err(source) => {
                return Err(BookmarkError::Read {
                    path: path.clone(),
                    source,
                });
            }
        };
        Ok(Self {
            path,
            state: Arc::new(Mutex::new(state)),
        })
    }

    pub async fn snapshot(&self) -> BTreeMap<String, u64> {
        self.state.lock().await.positions.clone()
    }

    pub async fn set(&self, object_id: String, position: u64) -> Result<(), BookmarkError> {
        let mut state = self.state.lock().await;
        if position == 0 {
            state.positions.remove(&object_id);
        } else {
            state.positions.insert(object_id, position);
        }
        let serialized = toml::to_string_pretty(&*state)?;
        let temporary = self.path.with_extension("toml.tmp");
        tokio::fs::write(&temporary, serialized)
            .await
            .map_err(|source| BookmarkError::Write {
                path: temporary.clone(),
                source,
            })?;
        replace_file(&temporary, &self.path)
            .await
            .map_err(|source| BookmarkError::Write {
                path: self.path.clone(),
                source,
            })?;
        Ok(())
    }
}

async fn replace_file(temporary: &PathBuf, destination: &PathBuf) -> io::Result<()> {
    match tokio::fs::rename(temporary, destination).await {
        Ok(()) => Ok(()),
        Err(error)
            if cfg!(windows)
                && matches!(
                    error.kind(),
                    io::ErrorKind::AlreadyExists | io::ErrorKind::PermissionDenied
                ) =>
        {
            match tokio::fs::remove_file(destination).await {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            tokio::fs::rename(temporary, destination).await
        }
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[tokio::test]
    async fn persists_and_removes_positions() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("bookmarks.toml");
        let store = BookmarkStore::load(path.clone()).await.unwrap();
        store.set("item-1".into(), 12_345).await.unwrap();

        let reloaded = BookmarkStore::load(path).await.unwrap();
        assert_eq!(reloaded.snapshot().await["item-1"], 12_345);

        reloaded.set("item-1".into(), 0).await.unwrap();
        assert!(!reloaded.snapshot().await.contains_key("item-1"));
    }
}
