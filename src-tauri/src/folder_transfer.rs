use std::fs;
use std::path::{Component, Path};

pub(crate) const MAX_ENTRIES: usize = 20_000;
pub(crate) const MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;

#[derive(Debug)]
pub(crate) struct FolderBundle {
    file: tempfile::NamedTempFile,
}

impl FolderBundle {
    pub fn path(&self) -> &Path {
        self.file.path()
    }
    pub fn size(&self) -> std::io::Result<u64> {
        self.file.as_file().metadata().map(|m| m.len())
    }
}

pub(crate) fn validate_relative_path(path: &Path) -> Result<(), String> {
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(name) => {
                let name = name.to_str().ok_or("文件名不是有效 UTF-8。")?;
                if name.contains(['\\', ':', '\0', '<', '>', '"', '|', '?', '*'])
                    || name.ends_with(['.', ' '])
                {
                    return Err("文件名不能跨平台传输。".into());
                }
                let stem = name.split('.').next().unwrap_or("").to_ascii_uppercase();
                if matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
                    || ["COM", "LPT"].iter().any(|prefix| {
                        stem.strip_prefix(prefix).is_some_and(|n| {
                            matches!(n, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
                        })
                    })
                {
                    return Err("文件夹含有 Windows 保留文件名。".into());
                }
            }
            _ => return Err("文件夹条目不能越过接收目录。".into()),
        }
    }
    Ok(())
}

pub(crate) fn walk(root: &Path) -> Result<Vec<(std::path::PathBuf, bool, u64)>, String> {
    let mut entries = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0)];
    let mut bytes = 0_u64;
    while let Some((path, depth)) = stack.pop() {
        if depth > 64 || entries.len() >= MAX_ENTRIES {
            return Err("文件夹层级或条目过多。".into());
        }
        let meta = fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
        if meta.file_type().is_symlink() || (!meta.is_dir() && !meta.is_file()) {
            return Err("暂不支持符号链接或特殊文件。".into());
        }
        let relative = path
            .strip_prefix(root)
            .map_err(|e| e.to_string())?
            .to_path_buf();
        validate_relative_path(&relative)?;
        let size = if meta.is_file() { meta.len() } else { 0 };
        bytes = bytes.checked_add(size).ok_or("文件夹过大。")?;
        if bytes > MAX_BYTES {
            return Err("文件夹超过 2 GiB 传输上限。".into());
        }
        entries.push((relative, meta.is_dir(), size));
        if meta.is_dir() {
            let mut children = fs::read_dir(&path)
                .map_err(|e| e.to_string())?
                .map(|e| e.map(|e| e.path()))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| e.to_string())?;
            children.sort();
            stack.extend(children.into_iter().rev().map(|path| (path, depth + 1)));
        }
    }
    Ok(entries)
}

pub(crate) fn bundle(root: &Path) -> Result<FolderBundle, String> {
    let entries = walk(root)?;
    let mut file = tempfile::NamedTempFile::new().map_err(|e| e.to_string())?;
    {
        let mut archive = tar::Builder::new(file.as_file_mut());
        archive.follow_symlinks(false);
        archive.sparse(false);
        for (relative, directory, _) in entries {
            let name = if relative.as_os_str().is_empty() {
                Path::new(".")
            } else {
                &relative
            };
            if directory {
                archive.append_dir(name, root.join(&relative))
            } else {
                archive.append_path_with_name(root.join(&relative), name)
            }
            .map_err(|e| e.to_string())?;
        }
        archive.finish().map_err(|e| e.to_string())?;
    }
    let bundle = FolderBundle { file };
    if bundle.size().map_err(|e| e.to_string())? > MAX_BYTES {
        return Err("打包后的文件夹超过 2 GiB 上限。".into());
    }
    Ok(bundle)
}

pub(crate) fn extract(archive: &Path, destination: &Path) -> Result<(), String> {
    let mut archive = tar::Archive::new(fs::File::open(archive).map_err(|e| e.to_string())?);
    let mut bytes = 0_u64;
    for (index, entry) in archive.entries().map_err(|e| e.to_string())?.enumerate() {
        if index >= MAX_ENTRIES {
            return Err("文件夹条目过多。".into());
        }
        let mut entry = entry.map_err(|e| e.to_string())?;
        validate_relative_path(&entry.path().map_err(|e| e.to_string())?)?;
        let kind = entry.header().entry_type();
        if !kind.is_file() && !kind.is_dir() {
            return Err("文件夹包含链接或特殊条目。".into());
        }
        bytes = bytes.checked_add(entry.size()).ok_or("文件夹过大。")?;
        if bytes > MAX_BYTES {
            return Err("展开后的文件夹超过上限。".into());
        }
        if !entry.unpack_in(destination).map_err(|e| e.to_string())? {
            return Err("文件夹条目越过接收目录。".into());
        }
    }
    Ok(())
}

pub(crate) fn copy_directory(source: &Path, destination: &Path) -> std::io::Result<()> {
    let entries = walk(source).map_err(std::io::Error::other)?;
    fs::create_dir(destination)?;
    let result = (|| {
        for (relative, directory, _) in entries {
            if relative.as_os_str().is_empty() {
                continue;
            }
            let output = destination.join(&relative);
            if directory {
                fs::create_dir_all(&output)?;
            } else {
                let mut file = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(output)?;
                std::io::copy(&mut fs::File::open(source.join(relative))?, &mut file)?;
            }
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(destination);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folders_keep_nested_names_unicode_and_empty_directories() {
        let source = tempfile::tempdir().unwrap();
        fs::create_dir_all(source.path().join("空目录")).unwrap();
        fs::create_dir_all(source.path().join("nested")).unwrap();
        fs::write(source.path().join("nested/中文.txt"), "内容").unwrap();
        let bundle = bundle(source.path()).unwrap();
        let output = tempfile::tempdir().unwrap();
        extract(bundle.path(), output.path()).unwrap();
        assert!(output.path().join("空目录").is_dir());
        assert_eq!(
            fs::read_to_string(output.path().join("nested/中文.txt")).unwrap(),
            "内容"
        );
    }

    #[test]
    fn cross_platform_paths_cannot_escape_or_use_device_names() {
        for path in [
            "../outside",
            "/outside",
            "x/../outside",
            "C:/outside",
            "x\\..\\outside",
            "CON.txt",
            "x/LPT1",
        ] {
            assert!(validate_relative_path(Path::new(path)).is_err(), "{path}");
        }
    }

    #[test]
    fn extracting_a_link_cannot_create_a_path_outside_the_receiving_folder() {
        let mut archive = tempfile::NamedTempFile::new().unwrap();
        {
            let mut builder = tar::Builder::new(archive.as_file_mut());
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_size(0);
            header.set_mode(0o777);
            builder
                .append_link(&mut header, "link", "../outside")
                .unwrap();
            builder.finish().unwrap();
        }
        let root = tempfile::tempdir().unwrap();
        assert!(extract(archive.path(), root.path()).is_err());
        assert!(!root.path().join("link").exists());
    }
}
