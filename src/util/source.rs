//! Conversion between Kenku-style media URLs and local file paths.
//!
//! Kenku FM stores the location of every track and sound in a `url` field. Files added from disk
//! are saved as percent-encoded `file://` URLs, but users may also paste plain paths or web URLs.

use std::fmt;
use std::fmt::Write;
use std::path::{Path, PathBuf};

/// Why a media `url` cannot be played from the local filesystem.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceError {
    /// The location is empty.
    Empty,
    /// An `http(s)` URL: streaming from the network is not supported yet.
    Remote(String),
    /// Any other URL scheme.
    UnsupportedScheme(String),
}

impl fmt::Display for SourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("no file location set"),
            Self::Remote(url) => write!(f, "web URLs are not supported yet: {url}"),
            Self::UnsupportedScheme(scheme) => write!(f, "unsupported URL scheme `{scheme}`"),
        }
    }
}

impl std::error::Error for SourceError {}

/// Resolves a track/sound `url` (a `file://` URL or a plain path) into a filesystem path.
pub fn resolve_local_path(url: &str) -> Result<PathBuf, SourceError> {
    let url = url.trim();
    if url.is_empty() {
        return Err(SourceError::Empty);
    }
    let Some((scheme, rest)) = split_scheme(url) else {
        return Ok(PathBuf::from(url));
    };
    match scheme.to_ascii_lowercase().as_str() {
        "file" => {
            // `file:///abs/path`, or `file://localhost/abs/path`.
            let path = rest.strip_prefix("localhost").filter(|p| p.starts_with('/')).unwrap_or(rest);
            Ok(bytes_to_path(percent_decode(path)))
        }
        "http" | "https" => Err(SourceError::Remote(url.to_owned())),
        other => Err(SourceError::UnsupportedScheme(other.to_owned())),
    }
}

/// Encodes a path as a `file://` URL exactly like Kenku FM's `encodeFilePath`
/// (`encodeURIComponent`, keeping `/` and `:` readable).
pub fn path_to_url(path: &Path) -> String {
    let mut url = String::from("file://");
    for &byte in path.as_os_str().as_encoded_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' => url.push(byte as char),
            b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')' | b'/' | b':' => url.push(byte as char),
            b'\\' => url.push('/'),
            _ => {
                let _ = write!(url, "%{byte:02X}");
            }
        }
    }
    url
}

/// Turns a file name into a display title, like Kenku FM's `cleanFileName`:
/// drops the extension, turns `_`/`-` into spaces, collapses whitespace and capitalises words.
pub fn title_from_path(path: &Path) -> String {
    let stem = path
        .file_stem()
        .or_else(|| path.file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut title = String::with_capacity(stem.len());
    for word in stem.split(|c: char| c == '_' || c == '-' || c.is_whitespace()).filter(|w| !w.is_empty()) {
        if !title.is_empty() {
            title.push(' ');
        }
        let mut chars = word.chars();
        if let Some(first) = chars.next() {
            title.extend(first.to_uppercase());
            title.push_str(chars.as_str());
        }
    }
    title
}

/// Splits `scheme://rest`; returns `None` for plain paths.
fn split_scheme(url: &str) -> Option<(&str, &str)> {
    let (scheme, rest) = url.split_once("://")?;
    // RFC 3986: scheme = ALPHA *( ALPHA / DIGIT / "+" / "-" / "." )
    let mut chars = scheme.chars();
    let valid = chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
    valid.then_some((scheme, rest))
}

fn percent_decode(input: &str) -> Vec<u8> {
    fn hex(byte: u8) -> Option<u8> {
        match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            b'A'..=b'F' => Some(byte - b'A' + 10),
            _ => None,
        }
    }
    let bytes = input.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let (Some(high), Some(low)) = (hex(bytes[index + 1]), hex(bytes[index + 2])) {
                decoded.push(high << 4 | low);
                index += 3;
                continue;
            }
        }
        decoded.push(bytes[index]);
        index += 1;
    }
    decoded
}

#[cfg(unix)]
fn bytes_to_path(bytes: Vec<u8>) -> PathBuf {
    use std::os::unix::ffi::OsStringExt;
    PathBuf::from(std::ffi::OsString::from_vec(bytes))
}

#[cfg(not(unix))]
fn bytes_to_path(bytes: Vec<u8>) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_paths_pass_through() {
        assert_eq!(resolve_local_path("/music/a b.mp3"), Ok(PathBuf::from("/music/a b.mp3")));
        assert_eq!(resolve_local_path("  relative.ogg "), Ok(PathBuf::from("relative.ogg")));
        // `://` inside a path is not a scheme.
        assert_eq!(resolve_local_path("/odd/x://y.mp3"), Ok(PathBuf::from("/odd/x://y.mp3")));
    }

    #[test]
    fn decodes_kenku_file_urls() {
        let url = "file:///home/gm/M%C3%BAsica/Tavern%20%231%20(loop).mp3";
        assert_eq!(resolve_local_path(url), Ok(PathBuf::from("/home/gm/Música/Tavern #1 (loop).mp3")));
        assert_eq!(resolve_local_path("file://localhost/a.wav"), Ok(PathBuf::from("/a.wav")));
        // Malformed escapes are kept literally.
        assert_eq!(resolve_local_path("file:///100%.mp3"), Ok(PathBuf::from("/100%.mp3")));
        assert_eq!(resolve_local_path("file:///a%2"), Ok(PathBuf::from("/a%2")));
    }

    #[test]
    fn rejects_unsupported_locations() {
        assert_eq!(resolve_local_path(""), Err(SourceError::Empty));
        assert!(matches!(resolve_local_path("https://example.com/a.mp3"), Err(SourceError::Remote(_))));
        assert!(matches!(resolve_local_path("ftp://host/a.mp3"), Err(SourceError::UnsupportedScheme(s)) if s == "ftp"));
    }

    #[test]
    fn url_round_trip() {
        let path = Path::new("/home/gm/Música/Tavern #1 (loop) 100%.mp3");
        let url = path_to_url(path);
        assert_eq!(url, "file:///home/gm/M%C3%BAsica/Tavern%20%231%20(loop)%20100%25.mp3");
        assert_eq!(resolve_local_path(&url), Ok(path.to_path_buf()));
    }

    #[test]
    fn titles_are_cleaned() {
        assert_eq!(title_from_path(Path::new("/x/dark_forest-ambience  01.ogg")), "Dark Forest Ambience 01");
        assert_eq!(title_from_path(Path::new("battle.theme.mp3")), "Battle.theme");
        assert_eq!(title_from_path(Path::new("ünïcode_song.flac")), "Ünïcode Song");
    }
}
