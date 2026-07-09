//! Race-safe artifact and temporary-file helpers.

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

pub const STDIN_UPLOAD_LIMIT: u64 = 64 * 1024 * 1024;

pub fn write_artifact(path: &Path, bytes: &[u8], force: bool) -> Result<()> {
    let mut reservation = ReservedArtifact::reserve(path, force)?;
    reservation
        .as_file_mut()
        .write_all(bytes)
        .with_context(|| format!("writing temporary artifact for {}", path.display()))?;
    reservation.finalize(force)
}

#[derive(Debug)]
pub struct ReservedArtifact {
    final_path: PathBuf,
    tmp: tempfile::NamedTempFile,
}

impl ReservedArtifact {
    pub fn reserve(final_path: &Path, force: bool) -> Result<Self> {
        if !force && final_path.try_exists().unwrap_or(true) {
            bail!(overwrite_hint(final_path));
        }
        let parent = final_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let stem = final_path
            .file_stem()
            .and_then(|s| s.to_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("artifact");
        let suffix = final_path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| format!(".{e}"))
            .unwrap_or_default();
        let tmp = tempfile::Builder::new()
            .prefix(&format!(".{stem}.rdny-"))
            .suffix(&suffix)
            .tempfile_in(parent)
            .with_context(|| {
                format!("creating temporary output next to {}", final_path.display())
            })?;
        Ok(Self {
            final_path: final_path.to_path_buf(),
            tmp,
        })
    }
    pub fn tmp_path(&self) -> &Path {
        self.tmp.path()
    }
    pub fn as_file_mut(&mut self) -> &mut fs::File {
        self.tmp.as_file_mut()
    }
    pub fn finalize(self, force: bool) -> Result<()> {
        self.tmp.as_file().sync_all().with_context(|| {
            format!(
                "syncing temporary artifact for {}",
                self.final_path.display()
            )
        })?;
        if force {
            self.tmp
                .persist(&self.final_path)
                .map(|_| ())
                .map_err(|err| err.error)
                .with_context(|| format!("publishing {}", self.final_path.display()))
        } else {
            self.tmp
                .persist_noclobber(&self.final_path)
                .map(|_| ())
                .map_err(|err| err.error)
                .with_context(|| overwrite_hint(&self.final_path))
        }
    }
}

pub fn sanitize_download_name(raw: &str) -> String {
    let leaf = raw.rsplit(['/', '\\']).next().unwrap_or(raw);
    let was_dotfile = leaf.trim_start().starts_with('.');
    let mut out: String = leaf
        .chars()
        .map(|c| {
            if c.is_control() || is_dangerous_format_char(c) || matches!(c, '/' | '\\' | ':') {
                '_'
            } else {
                c
            }
        })
        .collect();
    out = out
        .trim_matches(|c: char| c.is_whitespace() || c == '.')
        .to_string();
    if out.is_empty() || out == "." || out == ".." || was_dotfile || out.starts_with('.') {
        out = "download.bin".to_string();
    }
    truncate_utf8_bytes(&out, 180)
}

fn is_dangerous_format_char(c: char) -> bool {
    matches!(
        c,
        '\u{00ad}'
            | '\u{034f}'
            | '\u{061c}'
            | '\u{115f}'..='\u{1160}'
            | '\u{17b4}'..='\u{17b5}'
            | '\u{180b}'..='\u{180f}'
            | '\u{200b}'..='\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2060}'..='\u{206f}'
            | '\u{3164}'
            | '\u{fe00}'..='\u{fe0f}'
            | '\u{feff}'
            | '\u{ffa0}'
            | '\u{1bca0}'..='\u{1bca3}'
            | '\u{1d173}'..='\u{1d17a}'
            | '\u{e0100}'..='\u{e01ef}'
    )
}

fn truncate_utf8_bytes(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

#[derive(Debug)]
pub struct StdinUpload {
    _dir: tempfile::TempDir,
    path: PathBuf,
}
impl StdinUpload {
    pub fn path(&self) -> &Path {
        &self.path
    }
}
impl Drop for StdinUpload {
    fn drop(&mut self) {
        // TempDir removes the whole private upload tree.
    }
}

pub fn stdin_upload(reader: impl Read) -> Result<StdinUpload> {
    stdin_upload_in(reader, std::env::temp_dir())
}

fn stdin_upload_in(reader: impl Read, parent: impl AsRef<Path>) -> Result<StdinUpload> {
    let dir = tempfile::Builder::new()
        .prefix("rdny-upload-")
        .tempdir_in(parent)
        .context("creating private stdin upload directory")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).ok();
    }
    let mut limited = reader.take(STDIN_UPLOAD_LIMIT + 1);
    let mut bytes = Vec::new();
    limited
        .read_to_end(&mut bytes)
        .context("reading upload data from stdin")?;
    if bytes.len() as u64 > STDIN_UPLOAD_LIMIT {
        bail!(
            "stdin upload is larger than {} MiB; pass a file path instead or reduce the input",
            STDIN_UPLOAD_LIMIT / 1024 / 1024
        );
    }
    let path = dir.path().join("stdin-upload");
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .with_context(|| format!("creating {}", path.display()))?;
    file.write_all(&bytes)
        .with_context(|| format!("writing {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).ok();
    }
    Ok(StdinUpload { _dir: dir, path })
}

fn overwrite_hint(path: &Path) -> String {
    format!(
        "refusing to overwrite {}; pass --force to replace an existing file",
        path.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sanitizes_malicious_names() {
        assert_eq!(sanitize_download_name("../.ssh/id"), "id");
        assert_eq!(sanitize_download_name(".profile"), "download.bin");
        assert_eq!(sanitize_download_name("a/b\0c"), "b_c");
        assert_eq!(sanitize_download_name(".."), "download.bin");
    }
    #[test]
    fn no_overwrite_or_symlink_by_default() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("x");
        fs::write(&p, b"old").unwrap();
        assert!(write_artifact(&p, b"new", false).is_err());
        assert_eq!(fs::read(&p).unwrap(), b"old");
    }

    #[cfg(unix)]
    #[test]
    fn refuses_symlink_by_default() {
        use std::os::unix::fs::symlink;
        let d = tempfile::tempdir().unwrap();
        let target = d.path().join("target");
        let link = d.path().join("link");
        fs::write(&target, b"old").unwrap();
        symlink(&target, &link).unwrap();
        assert!(write_artifact(&link, b"new", false).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"old");
    }
    #[test]
    fn force_overwrites() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("x");
        fs::write(&p, b"old").unwrap();
        write_artifact(&p, b"new", true).unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"new");
    }

    #[cfg(unix)]
    #[test]
    fn force_replaces_symlink_not_target() {
        use std::os::unix::fs::symlink;
        let d = tempfile::tempdir().unwrap();
        let target = d.path().join("target");
        let link = d.path().join("link");
        fs::write(&target, b"old").unwrap();
        symlink(&target, &link).unwrap();
        write_artifact(&link, b"new", true).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"old");
        assert_eq!(fs::read(&link).unwrap(), b"new");
        assert!(
            !fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn failed_write_preserves_old_output_and_cleans_temp() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("x.txt");
        fs::write(&p, b"old").unwrap();
        let err = ReservedArtifact::reserve(&p, false).unwrap_err();
        assert!(format!("{err:?}").contains("refusing to overwrite"));
        assert_eq!(fs::read(&p).unwrap(), b"old");
        assert_eq!(fs::read_dir(d.path()).unwrap().count(), 1);
    }

    #[test]
    fn reservation_preserves_final_extension_and_finalizes() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("movie.mp4");
        let mut r = ReservedArtifact::reserve(&p, false).unwrap();
        assert_eq!(
            r.tmp_path().extension().and_then(|e| e.to_str()),
            Some("mp4")
        );
        r.as_file_mut().write_all(b"video").unwrap();
        r.finalize(false).unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"video");
        assert_eq!(fs::read_dir(d.path()).unwrap().count(), 1);
    }

    #[test]
    fn dropped_reservation_cleans_temp() {
        let d = tempfile::tempdir().unwrap();
        let tmp = {
            let r = ReservedArtifact::reserve(&d.path().join("movie.mp4"), false).unwrap();
            r.tmp_path().to_path_buf()
        };
        assert!(!tmp.exists());
    }
    #[test]
    fn stdin_upload_is_private_unique_and_cleaned() {
        let a = stdin_upload(&b"a"[..]).unwrap();
        let b = stdin_upload(&b"b"[..]).unwrap();
        assert_ne!(a.path(), b.path());
        assert_eq!(fs::read(a.path()).unwrap(), b"a");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(a.path()).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(a.path().parent().unwrap())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
        let p = a.path().to_path_buf();
        let parent = p.parent().unwrap().to_path_buf();
        drop(a);
        assert!(!p.exists());
        assert!(!parent.exists());
    }

    #[test]
    fn stdin_upload_error_cleans_private_tree() {
        let parent = tempfile::tempdir().unwrap();
        let data = vec![0u8; (STDIN_UPLOAD_LIMIT + 1) as usize];
        assert!(stdin_upload_in(&data[..], parent.path()).is_err());
        assert_eq!(fs::read_dir(parent.path()).unwrap().count(), 0);
    }

    #[test]
    fn parallel_stdin_uploads_are_unique() {
        let handles: Vec<_> = (0..8)
            .map(|i| std::thread::spawn(move || stdin_upload(&[i][..]).unwrap()))
            .collect();
        let uploads: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        for (idx, upload) in uploads.iter().enumerate() {
            assert_eq!(fs::read(upload.path()).unwrap(), vec![idx as u8]);
            assert_eq!(
                uploads
                    .iter()
                    .filter(|other| other.path() == upload.path())
                    .count(),
                1
            );
        }
    }
    #[test]
    fn stdin_upload_limit_has_error() {
        let data = vec![0u8; (STDIN_UPLOAD_LIMIT + 1) as usize];
        let e = stdin_upload(&data[..]).unwrap_err();
        assert!(format!("{e}").contains("pass a file path"));
    }

    #[test]
    fn unicode_length_limit_is_char_safe() {
        let name = format!("{}x", "é".repeat(180));
        let sanitized = sanitize_download_name(&name);
        assert!(sanitized.len() <= 180);
        assert!(sanitized.ends_with('é'));
    }

    #[test]
    fn dangerous_unicode_format_chars_are_removed_or_bounded() {
        assert_eq!(
            sanitize_download_name("safe\u{202e}gnp.exe"),
            "safe_gnp.exe"
        );
        assert_eq!(
            sanitize_download_name("zero\u{200b}width.txt"),
            "zero_width.txt"
        );
        let many = format!("{}ok.txt", "\u{200d}".repeat(400));
        let sanitized = sanitize_download_name(&many);
        assert!(sanitized.len() <= 180);
        assert!(!sanitized.contains('\u{200d}'));
    }
}
