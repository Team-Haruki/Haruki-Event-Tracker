use std::path::{Path, PathBuf};

use opendal::{Operator, services};

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("invalid storage location `{0}`")]
    InvalidLocation(String),
    #[error("storage location `{0}` does not contain a file name")]
    MissingFileName(String),
    #[error("resolve current directory: {0}")]
    CurrentDir(#[from] std::io::Error),
    #[error("path `{0}` is not valid UTF-8")]
    NonUtf8Path(String),
    #[error("open storage `{location}`: {source}")]
    Open {
        location: String,
        #[source]
        source: Box<opendal::Error>,
    },
    #[error("read `{path}` from `{location}`: {source}")]
    Read {
        location: String,
        path: String,
        #[source]
        source: Box<opendal::Error>,
    },
    #[error("decode `{path}` from `{location}` as UTF-8: {source}")]
    Utf8 {
        location: String,
        path: String,
        #[source]
        source: std::string::FromUtf8Error,
    },
}

#[derive(Debug, Clone)]
pub struct StorageRoot {
    op: Operator,
    location: String,
}

#[derive(Debug, Clone)]
pub struct StorageFile {
    root: StorageRoot,
    path: String,
}

impl StorageRoot {
    pub fn from_dir_location(location: impl AsRef<str>) -> Result<Self, StorageError> {
        let location = location.as_ref().trim();
        if location.is_empty() {
            return Err(StorageError::InvalidLocation(location.to_owned()));
        }

        if is_storage_uri(location) {
            let root_uri = ensure_uri_directory(location);
            let op =
                Operator::from_uri(root_uri.as_str()).map_err(|source| StorageError::Open {
                    location: root_uri.clone(),
                    source: Box::new(source),
                })?;
            Ok(Self {
                op,
                location: root_uri,
            })
        } else {
            let root = absolute_path(location)?;
            local_root(root.as_path(), location)
        }
    }

    pub async fn read(&self, path: &str) -> Result<Vec<u8>, StorageError> {
        let normalized = normalize_object_path(path)?;
        let bytes =
            self.op
                .read(normalized.as_str())
                .await
                .map_err(|source| StorageError::Read {
                    location: self.location.clone(),
                    path: normalized.clone(),
                    source: Box::new(source),
                })?;
        Ok(bytes.to_vec())
    }

    pub async fn read_to_string(&self, path: &str) -> Result<String, StorageError> {
        let bytes = self.read(path).await?;
        String::from_utf8(bytes).map_err(|source| StorageError::Utf8 {
            location: self.location.clone(),
            path: path.to_owned(),
            source,
        })
    }
}

impl StorageFile {
    pub fn from_location(location: impl AsRef<str>) -> Result<Self, StorageError> {
        let location = location.as_ref().trim();
        if location.is_empty() {
            return Err(StorageError::InvalidLocation(location.to_owned()));
        }

        if is_storage_uri(location) {
            let (root_uri, path) = split_uri_file(location)?;
            let root = StorageRoot::from_dir_location(root_uri)?;
            Ok(Self { root, path })
        } else {
            let path = absolute_path(location)?;
            let file_name = path
                .file_name()
                .ok_or_else(|| StorageError::MissingFileName(location.to_owned()))?;
            let file_name = file_name
                .to_str()
                .ok_or_else(|| StorageError::NonUtf8Path(path.display().to_string()))?
                .to_owned();
            let parent = path.parent().unwrap_or_else(|| Path::new("."));
            let root = local_root(parent, location)?;
            Ok(Self {
                root,
                path: file_name,
            })
        }
    }

    pub async fn read_to_string(&self) -> Result<String, StorageError> {
        self.root.read_to_string(&self.path).await
    }
}

fn is_storage_uri(location: &str) -> bool {
    let Some(idx) = location.find("://") else {
        return false;
    };
    let scheme = &location[..idx];
    !scheme.is_empty()
        && scheme
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
}

fn local_root(root: &Path, display: &str) -> Result<StorageRoot, StorageError> {
    let root = root
        .to_str()
        .ok_or_else(|| StorageError::NonUtf8Path(root.display().to_string()))?;
    let op = Operator::via_iter(services::FS_SCHEME, [("root".to_owned(), root.to_owned())])
        .map_err(|source| StorageError::Open {
            location: display.to_owned(),
            source: Box::new(source),
        })?;
    Ok(StorageRoot {
        op,
        location: display.to_owned(),
    })
}

fn absolute_path(path: &str) -> Result<PathBuf, StorageError> {
    let path = PathBuf::from(path);
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn normalize_object_path(path: &str) -> Result<String, StorageError> {
    let path = path.trim_start_matches('/');
    if path.is_empty() || path.split('/').any(|part| part.is_empty() || part == "..") {
        return Err(StorageError::InvalidLocation(path.to_owned()));
    }
    Ok(path.to_owned())
}

fn ensure_uri_directory(location: &str) -> String {
    let (base, query) = split_query(location);
    let base = if base.ends_with('/') {
        base.to_owned()
    } else {
        format!("{base}/")
    };
    format!("{base}{query}")
}

fn split_uri_file(location: &str) -> Result<(String, String), StorageError> {
    let (base, query) = split_query(location);
    let trimmed = base.trim_end_matches('/');
    let scheme_end = trimmed
        .find("://")
        .map(|idx| idx + 3)
        .ok_or_else(|| StorageError::InvalidLocation(location.to_owned()))?;
    let slash = trimmed[scheme_end..]
        .rfind('/')
        .map(|idx| scheme_end + idx)
        .ok_or_else(|| StorageError::MissingFileName(location.to_owned()))?;
    if slash + 1 >= trimmed.len() {
        return Err(StorageError::MissingFileName(location.to_owned()));
    }
    let root = format!("{}{}", &trimmed[..=slash], query);
    let path = trimmed[slash + 1..].to_owned();
    Ok((root, path))
}

fn split_query(location: &str) -> (&str, &str) {
    match location.find('?') {
        Some(idx) => (&location[..idx], &location[idx..]),
        None => (location, ""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_remote_file_uri() {
        let (root, path) =
            split_uri_file("s3://bucket/path/to/config.yaml?region=ap-northeast-1").unwrap();
        assert_eq!(root, "s3://bucket/path/to/?region=ap-northeast-1");
        assert_eq!(path, "config.yaml");
    }

    #[test]
    fn ensures_remote_dir_uri() {
        assert_eq!(
            ensure_uri_directory("s3://bucket/path/to/master?region=ap-northeast-1"),
            "s3://bucket/path/to/master/?region=ap-northeast-1"
        );
    }

    #[tokio::test]
    async fn reads_local_file() {
        let dir = std::env::temp_dir().join(format!("haruki-storage-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("config.yaml");
        std::fs::write(&file, "ok: true\n").unwrap();

        let src = StorageFile::from_location(file.to_str().unwrap()).unwrap();
        assert_eq!(src.read_to_string().await.unwrap(), "ok: true\n");

        let _ = std::fs::remove_file(file);
        let _ = std::fs::remove_dir(dir);
    }

    #[tokio::test]
    async fn reads_file_uri() {
        let dir = std::env::temp_dir().join(format!(
            "haruki-storage-file-uri-test-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("config.yaml");
        std::fs::write(&file, "ok: true\n").unwrap();

        let uri = format!("file://{}", file.display());
        let src = StorageFile::from_location(uri).unwrap();
        assert_eq!(src.read_to_string().await.unwrap(), "ok: true\n");

        let _ = std::fs::remove_file(file);
        let _ = std::fs::remove_dir(dir);
    }
}
