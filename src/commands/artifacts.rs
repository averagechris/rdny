//! Race-safe artifact and temporary-file helpers.

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;

pub const STDIN_UPLOAD_LIMIT: u64 = 64 * 1024 * 1024;
pub const DEFAULT_MAX_DOWNLOAD_BYTES: u64 = 256 * 1024 * 1024;

pub fn configured_max_download_bytes(cli: Option<u64>) -> Result<u64> {
    if let Some(value) = cli {
        return Ok(value);
    }
    match std::env::var("RDNY_MAX_DOWNLOAD_BYTES") {
        Ok(raw) => raw
            .parse::<u64>()
            .context("RDNY_MAX_DOWNLOAD_BYTES must be an integer byte count"),
        Err(std::env::VarError::NotPresent) => Ok(DEFAULT_MAX_DOWNLOAD_BYTES),
        Err(err) => Err(err).context("reading RDNY_MAX_DOWNLOAD_BYTES"),
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ArtifactContext {
    pub instance: Option<String>,
    pub target: Option<String>,
    pub url: Option<String>,
}

impl From<crate::session::PageIdentity> for ArtifactContext {
    fn from(identity: crate::session::PageIdentity) -> Self {
        Self {
            instance: identity.instance,
            target: identity.target,
            url: identity.url,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HumanArtifactOutput {
    Saved,
    BarePath,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProducedArtifact {
    pub schema_version: u8,
    pub kind: &'static str,
    pub path: String,
    #[serde(rename = "type")]
    pub media_type: String,
    pub bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip)]
    human_path: PathBuf,
    #[serde(skip)]
    human_output: HumanArtifactOutput,
}

impl ProducedArtifact {
    pub fn new(
        published: PublishedArtifact,
        human_path: PathBuf,
        human_output: HumanArtifactOutput,
        media_type: impl Into<String>,
        dimensions: Option<(u32, u32)>,
        context: ArtifactContext,
    ) -> Self {
        let (width, height) =
            dimensions.map_or((None, None), |(width, height)| (Some(width), Some(height)));
        Self {
            schema_version: 1,
            kind: "artifact",
            path: published.path.to_string_lossy().into_owned(),
            media_type: media_type.into(),
            bytes: published.bytes,
            width,
            height,
            instance: context.instance,
            target: context.target,
            url: context.url,
            human_path,
            human_output,
        }
    }

    pub fn human_summary(&self) -> String {
        let path: String = self
            .human_path
            .to_string_lossy()
            .chars()
            .filter_map(|character| {
                if character.is_control() || character == '\u{7f}' {
                    None
                } else if is_dangerous_format_char(character) {
                    Some('_')
                } else {
                    Some(character)
                }
            })
            .collect();
        match self.human_output {
            HumanArtifactOutput::Saved => format!("saved {path}"),
            HumanArtifactOutput::BarePath => path,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublishedArtifact {
    pub path: PathBuf,
    pub bytes: u64,
}

pub fn write_artifact(path: &Path, bytes: &[u8], force: bool) -> Result<PublishedArtifact> {
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
    pub fn finalize(self, force: bool) -> Result<PublishedArtifact> {
        self.tmp.as_file().sync_all().with_context(|| {
            format!(
                "syncing temporary artifact for {}",
                self.final_path.display()
            )
        })?;
        if force {
            self.tmp
                .persist(&self.final_path)
                .map_err(|err| err.error)
                .with_context(|| format!("publishing {}", self.final_path.display()))?;
        } else {
            self.tmp
                .persist_noclobber(&self.final_path)
                .map_err(|err| err.error)
                .with_context(|| overwrite_hint(&self.final_path))?;
        }
        published_metadata(&self.final_path)
    }
}

fn published_metadata(path: &Path) -> Result<PublishedArtifact> {
    let bytes = fs::metadata(path)
        .with_context(|| format!("reading final artifact metadata for {}", path.display()))?
        .len();
    let path = path
        .canonicalize()
        .unwrap_or_else(|_| absolute_normalized(path));
    Ok(PublishedArtifact { path, bytes })
}

fn absolute_normalized(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("/"))
            .join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if normalized.file_name().is_some() {
                    normalized.pop();
                }
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

pub fn normalize_media_type(content_type: Option<&str>) -> String {
    content_type
        .and_then(|value| value.split(';').next())
        .map(str::trim)
        .filter(|value| {
            let mut parts = value.split('/');
            parts.next().is_some_and(|part| !part.is_empty())
                && parts.next().is_some_and(|part| !part.is_empty())
                && parts.next().is_none()
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"!#$&^_.+-/".contains(&byte))
        })
        .map(str::to_ascii_lowercase)
        .unwrap_or_else(|| "application/octet-stream".to_string())
}

pub fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 24 || &bytes[..8] != b"\x89PNG\r\n\x1a\n" || &bytes[12..16] != b"IHDR" {
        return None;
    }
    let width = u32::from_be_bytes(bytes[16..20].try_into().ok()?);
    let height = u32::from_be_bytes(bytes[20..24].try_into().ok()?);
    (width > 0 && height > 0).then_some((width, height))
}

pub fn jpeg_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 4 || bytes[..2] != [0xff, 0xd8] {
        return None;
    }
    let mut offset = 2;
    while offset + 4 <= bytes.len() {
        while offset < bytes.len() && bytes[offset] == 0xff {
            offset += 1;
        }
        let marker = *bytes.get(offset)?;
        offset += 1;
        if matches!(marker, 0xd8 | 0xd9) {
            continue;
        }
        let length = u16::from_be_bytes(bytes.get(offset..offset + 2)?.try_into().ok()?) as usize;
        if length < 2 || offset + length > bytes.len() {
            return None;
        }
        if matches!(marker, 0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf) {
            let height = u16::from_be_bytes(bytes.get(offset + 3..offset + 5)?.try_into().ok()?);
            let width = u16::from_be_bytes(bytes.get(offset + 5..offset + 7)?.try_into().ok()?);
            return (width > 0 && height > 0).then_some((u32::from(width), u32::from(height)));
        }
        offset += length;
    }
    None
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
    let path = dir.path().join("stdin-upload");
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .with_context(|| format!("creating {}", path.display()))?;
    copy_bounded(reader, &mut file, STDIN_UPLOAD_LIMIT, "stdin upload")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).ok();
    }
    Ok(StdinUpload { _dir: dir, path })
}

pub fn copy_bounded(
    mut reader: impl Read,
    mut writer: impl Write,
    max: u64,
    label: &str,
) -> Result<u64> {
    let mut buf = [0_u8; 64 * 1024];
    let mut total = 0_u64;
    loop {
        let n = reader
            .read(&mut buf)
            .with_context(|| format!("reading {label}"))?;
        if n == 0 {
            break;
        }
        total = total.saturating_add(n as u64);
        if total > max {
            bail!(
                "{label} is larger than {max} bytes; pass a file path, increase the limit, or reduce the input"
            );
        }
        writer
            .write_all(&buf[..n])
            .with_context(|| format!("writing {label}"))?;
    }
    Ok(total)
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

    fn artifact_fixture() -> ProducedArtifact {
        ProducedArtifact::new(
            PublishedArtifact {
                path: PathBuf::from("/tmp/capture.png"),
                bytes: 123,
            },
            PathBuf::from("capture\n\u{1b}[31m\u{202e}.png"),
            HumanArtifactOutput::Saved,
            "image/png",
            Some((640, 480)),
            ArtifactContext {
                instance: Some("instance-1".into()),
                target: Some("target-1".into()),
                url: Some("https://example.test/page".into()),
            },
        )
    }

    #[test]
    fn artifact_schema_snapshot_is_exact() {
        assert_eq!(
            serde_json::to_value(artifact_fixture()).unwrap(),
            serde_json::json!({
                "schemaVersion": 1,
                "kind": "artifact",
                "path": "/tmp/capture.png",
                "type": "image/png",
                "bytes": 123,
                "width": 640,
                "height": 480,
                "instance": "instance-1",
                "target": "target-1",
                "url": "https://example.test/page"
            })
        );
    }

    #[test]
    fn artifact_optional_fields_are_omitted() {
        let artifact = ProducedArtifact::new(
            PublishedArtifact {
                path: PathBuf::from("/tmp/page.pdf"),
                bytes: 10,
            },
            PathBuf::from("page.pdf"),
            HumanArtifactOutput::Saved,
            "application/pdf",
            None,
            ArtifactContext::default(),
        );
        assert_eq!(
            serde_json::to_value(artifact).unwrap(),
            serde_json::json!({
                "schemaVersion": 1,
                "kind": "artifact",
                "path": "/tmp/page.pdf",
                "type": "application/pdf",
                "bytes": 10
            })
        );
    }

    #[test]
    fn human_artifact_paths_are_sanitized() {
        assert_eq!(artifact_fixture().human_summary(), "saved capture[31m_.png");
        let mut artifact = artifact_fixture();
        artifact.human_output = HumanArtifactOutput::BarePath;
        assert_eq!(artifact.human_summary(), "capture[31m_.png");
    }

    #[test]
    fn publication_returns_canonical_path_and_final_size() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/../artifact.bin");
        fs::create_dir(dir.path().join("nested")).unwrap();
        let published = write_artifact(&path, b"final bytes", false).unwrap();
        assert!(published.path.is_absolute());
        assert_eq!(
            published.path,
            dir.path().join("artifact.bin").canonicalize().unwrap()
        );
        assert_eq!(published.bytes, 11);
        assert_eq!(
            absolute_normalized(Path::new("/../../artifact.bin")),
            Path::new("/artifact.bin")
        );
    }

    #[test]
    fn parses_png_and_jpeg_dimensions() {
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        png.extend_from_slice(&640_u32.to_be_bytes());
        png.extend_from_slice(&480_u32.to_be_bytes());
        assert_eq!(png_dimensions(&png), Some((640, 480)));
        assert_eq!(png_dimensions(b"not png"), None);

        let jpeg = [
            0xff, 0xd8, 0xff, 0xc0, 0x00, 0x11, 0x08, 0x00, 0x03, 0x00, 0x05, 0x03, 0x01, 0x11,
            0x00, 0x02, 0x11, 0x00, 0x03, 0x11, 0x00,
        ];
        assert_eq!(jpeg_dimensions(&jpeg), Some((5, 3)));
        assert_eq!(jpeg_dimensions(&jpeg[..8]), None);
    }

    #[test]
    fn normalizes_download_content_type_with_fallback() {
        assert_eq!(
            normalize_media_type(Some(" Text/Plain; Charset=UTF-8 ")),
            "text/plain"
        );
        assert_eq!(normalize_media_type(None), "application/octet-stream");
        assert_eq!(
            normalize_media_type(Some("not a media type")),
            "application/octet-stream"
        );
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
}
