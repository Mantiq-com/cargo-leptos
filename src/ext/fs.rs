use crate::internal_prelude::*;
use camino::{Utf8Path, Utf8PathBuf};
use std::{collections::VecDeque, path::Path};
use tokio::fs::{self, ReadDir};

use super::path::PathExt;

pub async fn rm_dir_content<P: AsRef<Path>>(dir: P) -> Result<()> {
    try_rm_dir_content(&dir)
        .await
        .wrap_err(format!("Could not remove contents of {:?}", dir.as_ref()))
}

async fn try_rm_dir_content<P: AsRef<Path>>(dir: P) -> Result<()> {
    let dir = dir.as_ref();

    if !dir.exists() {
        debug!("Leptos not cleaning {dir:?} because it does not exist");
        return Ok(());
    }

    let mut entries = self::read_dir(dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();

        if entry.file_type().await?.is_dir() {
            self::remove_dir_all(path).await?;
        } else {
            self::remove_file(path).await?;
        }
    }
    Ok(())
}

/// Removes the contents of `dir` like [`rm_dir_content`], except the entry
/// that is, or contains, `keep`. `keep` is kept whole: nothing below it is
/// touched.
pub async fn rm_dir_content_except(dir: &Utf8Path, keep: &Utf8Path) -> Result<()> {
    if !dir.exists() {
        debug!("Leptos not cleaning {dir:?} because it does not exist");
        return Ok(());
    }

    let mut entries = self::read_dir(dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        let path = Utf8PathBuf::try_from(entry.path())
            .wrap_err_with(|| format!("Non-UTF-8 entry in {dir}"))?;
        if keep.starts_with(&path) {
            continue;
        }
        if entry.file_type().await?.is_dir() {
            self::remove_dir_all(&path).await?;
        } else {
            self::remove_file(&path).await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod rm_dir_content_except_tests {
    use super::*;
    use std::fs;
    use temp_dir::TempDir;

    #[tokio::test]
    async fn keeps_the_named_directory_and_removes_everything_else() {
        let dir = TempDir::new().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        fs::create_dir_all(root.join("pkg/snippets")).unwrap();
        fs::write(root.join("pkg/app.wasm"), b"x").unwrap();
        fs::write(root.join("pkg/snippets/a.js"), b"x").unwrap();
        fs::create_dir_all(root.join("images")).unwrap();
        fs::write(root.join("images/logo.svg"), b"x").unwrap();
        fs::write(root.join("index.html"), b"x").unwrap();

        rm_dir_content_except(&root, &root.join("pkg"))
            .await
            .unwrap();

        assert!(root.join("pkg/app.wasm").exists());
        assert!(root.join("pkg/snippets/a.js").exists());
        assert!(!root.join("images").exists());
        assert!(!root.join("index.html").exists());
    }

    #[tokio::test]
    async fn keeps_the_ancestors_of_a_nested_directory() {
        let dir = TempDir::new().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        fs::create_dir_all(root.join("static/pkg")).unwrap();
        fs::write(root.join("static/pkg/app.wasm"), b"x").unwrap();
        fs::write(root.join("static/other.txt"), b"x").unwrap();
        fs::write(root.join("index.html"), b"x").unwrap();

        rm_dir_content_except(&root, &root.join("static/pkg"))
            .await
            .unwrap();

        assert!(root.join("static/pkg/app.wasm").exists());
        assert!(
            root.join("static/other.txt").exists(),
            "an ancestor is kept whole"
        );
        assert!(!root.join("index.html").exists());
    }
}

pub async fn write<P: AsRef<Path>, C: AsRef<[u8]>>(path: P, contents: C) -> Result<()> {
    fs::write(&path, contents)
        .await
        .wrap_err(format!("Could not write to {:?}", path.as_ref()))
}

pub async fn read(path: impl AsRef<Path>) -> Result<Vec<u8>> {
    fs::read(&path)
        .await
        .wrap_err(format!("Could not read {:?}", path.as_ref()))
}

pub async fn create_dir(path: impl AsRef<Path>) -> Result<()> {
    trace!("FS create_dir {:?}", path.as_ref());
    fs::create_dir(&path)
        .await
        .wrap_err(format!("Could not create dir {:?}", path.as_ref()))
}

pub async fn create_dir_all<P: AsRef<Path>>(path: P) -> Result<()> {
    trace!("FS create_dir_all {:?}", path.as_ref());
    fs::create_dir_all(&path)
        .await
        .wrap_err(format!("Could not create {:?}", path.as_ref()))
}
pub async fn read_to_string<P: AsRef<Path>>(path: P) -> Result<String> {
    fs::read_to_string(&path)
        .await
        .wrap_err(format!("Could not read to string {:?}", path.as_ref()))
}

pub async fn copy<P: AsRef<Path>, Q: AsRef<Path>>(from: P, to: Q) -> Result<u64> {
    fs::copy(&from, &to)
        .await
        .wrap_err(format!("copy {:?} to {:?}", from.as_ref(), to.as_ref()))
}

pub async fn read_dir<P: AsRef<Path>>(path: P) -> Result<ReadDir> {
    fs::read_dir(&path)
        .await
        .wrap_err(format!("Could not read dir {:?}", path.as_ref()))
}

pub async fn rename<P: AsRef<Path>, Q: AsRef<Path>>(from: P, to: Q) -> Result<()> {
    fs::rename(&from, &to).await.wrap_err(format!(
        "Could not rename from {:?} to {:?}",
        from.as_ref(),
        to.as_ref()
    ))
}

pub async fn remove_file<P: AsRef<Path>>(path: P) -> Result<()> {
    fs::remove_file(&path)
        .await
        .wrap_err(format!("Could not remove file {:?}", path.as_ref()))
}

#[allow(dead_code)]
pub async fn remove_dir<P: AsRef<Path>>(path: P) -> Result<()> {
    fs::remove_dir(&path)
        .await
        .wrap_err(format!("Could not remove dir {:?}", path.as_ref()))
}

pub async fn remove_dir_all<P: AsRef<Path>>(path: P) -> Result<()> {
    fs::remove_dir_all(&path)
        .await
        .wrap_err(format!("Could not remove dir {:?}", path.as_ref()))
}

pub async fn copy_dir_all(src: impl AsRef<Utf8Path>, dst: impl AsRef<Path>) -> Result<()> {
    cp_dir_all(&src, &dst).await.wrap_err(format!(
        "Copy dir recursively from {:?} to {:?}",
        src.as_ref(),
        dst.as_ref()
    ))
}

async fn cp_dir_all(src: impl AsRef<Utf8Path>, dst: impl AsRef<Path>) -> Result<()> {
    let src = src.as_ref();
    let dst = Utf8PathBuf::from_path_buf(dst.as_ref().to_path_buf()).unwrap();

    self::create_dir_all(&dst).await?;

    let mut dirs = VecDeque::new();
    dirs.push_back(src.to_owned());

    while let Some(dir) = dirs.pop_front() {
        let mut entries = dir.read_dir_utf8()?;

        while let Some(Ok(entry)) = entries.next() {
            let from = entry.path().to_owned();
            let to = from.rebase(src, &dst)?;

            if entry.file_type()?.is_dir() {
                self::create_dir_all(&to).await?;
                dirs.push_back(from);
            } else {
                self::copy(from, to).await?;
            }
        }
    }
    Ok(())
}
