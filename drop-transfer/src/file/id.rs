use std::{
    borrow::Borrow,
    fmt,
    hash::Hash,
    path::{Path, PathBuf},
};

use base64::prelude::*;
use serde::{Deserialize, Serialize};
use sha2::Digest;

use crate::utils::Hidden;

#[derive(Hash, Clone, PartialEq, Eq)]
pub struct FileSubPath(Vec<String>);

#[derive(Hash, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FileId(String);

const SEPARATOR: &str = "/";

impl From<sha2::Sha256> for FileId {
    fn from(hash: sha2::Sha256) -> Self {
        let out = hash.finalize();
        let id = BASE64_URL_SAFE_NO_PAD.encode(out);
        Self(id)
    }
}

impl From<String> for FileId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for FileId {
    fn from(value: &str) -> Self {
        Self(value.to_string())
    }
}

impl From<&FileSubPath> for FileId {
    fn from(value: &FileSubPath) -> Self {
        Self(value.to_string())
    }
}

impl fmt::Display for FileId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for FileId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl Borrow<String> for FileId {
    fn borrow(&self) -> &String {
        &self.0
    }
}

impl fmt::Debug for FileId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("FileId").field(&Hidden(&self.0)).finish()
    }
}

impl FileSubPath {
    pub fn from_file_name(path: impl AsRef<Path>) -> crate::Result<Self> {
        let name = path
            .as_ref()
            .file_name()
            .ok_or_else(|| crate::Error::BadPath("Missing file name".into()))?
            .to_str()
            .ok_or_else(|| crate::Error::BadPath("File name should be valid UTF8".into()))?;

        Ok(Self(vec![name.to_owned()]))
    }

    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &String> {
        self.0.iter()
    }

    pub fn append(&mut self, name: String) {
        self.0.push(name);
    }

    pub fn append_file_name(mut self, path: impl AsRef<Path>) -> crate::Result<Self> {
        let name = path
            .as_ref()
            .file_name()
            .ok_or_else(|| crate::Error::BadPath("Missing file name".into()))?
            .to_str()
            .ok_or_else(|| crate::Error::BadPath("File name should be valid UTF8".into()))?;

        self.0.push(name.to_owned());
        Ok(self)
    }

    pub fn name(&self) -> &str {
        self.0.last().expect("Missing last path component")
    }

    pub fn from_path(path: impl AsRef<Path>) -> crate::Result<Self> {
        let vec = path
            .as_ref()
            .iter()
            .map(|cmp| {
                cmp.to_str()
                    .map(String::from)
                    .ok_or_else(|| crate::Error::BadPath("Paths should be valid UTF8".into()))
            })
            .collect::<Result<_, _>>()?;
        Ok(Self(vec))
    }
}

impl<T> From<T> for FileSubPath
where
    T: AsRef<str>,
{
    fn from(value: T) -> Self {
        let vec = value
            .as_ref()
            .split(SEPARATOR)
            .map(ToString::to_string)
            .collect();
        Self(vec)
    }
}

impl From<FileSubPath> for PathBuf {
    fn from(FileSubPath(value): FileSubPath) -> Self {
        value.into_iter().collect()
    }
}

impl From<&FileSubPath> for PathBuf {
    fn from(value: &FileSubPath) -> Self {
        value.0.iter().collect()
    }
}

impl From<FileSubPath> for Box<Path> {
    fn from(value: FileSubPath) -> Self {
        PathBuf::from(value).into_boxed_path()
    }
}

impl From<&FileSubPath> for Box<Path> {
    fn from(value: &FileSubPath) -> Self {
        PathBuf::from(value).into_boxed_path()
    }
}

impl ToString for FileSubPath {
    fn to_string(&self) -> String {
        self.0.join(SEPARATOR)
    }
}

impl fmt::Debug for FileSubPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("FileSubPath")
            .field(&Hidden(self.to_string()))
            .finish()
    }
}

impl serde::Serialize for FileSubPath {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.to_string().serialize(serializer)
    }
}

impl<'de> serde::Deserialize<'de> for FileSubPath {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let str = String::deserialize(deserializer)?;
        Ok(Self::from(str.as_str()))
    }
}
