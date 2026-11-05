//! Identifier generation.

use std::fmt::Write;

/// Returns a random RFC 4122 version-4 UUID string (`xxxxxxxx-xxxx-4xxx-yxxx-xxxxxxxxxxxx`).
///
/// Kenku FM identifies playlists, tracks, soundboards and sounds with v4 UUIDs. Using the same
/// format keeps IDs interchangeable with Kenku remote-control clients such as the Stream Deck
/// plugin. IDs only need to be unique, not unpredictable, so a fast non-cryptographic RNG is fine.
pub fn new_id() -> String {
    let mut bytes = fastrand::u128(..).to_be_bytes();
    bytes[6] = (bytes[6] & 0x0f) | 0x40; // version 4
    bytes[8] = (bytes[8] & 0x3f) | 0x80; // RFC 4122 variant
    let mut id = String::with_capacity(36);
    for (index, byte) in bytes.iter().enumerate() {
        if matches!(index, 4 | 6 | 8 | 10) {
            id.push('-');
        }
        let _ = write!(id, "{byte:02x}");
    }
    id
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn has_uuid_v4_shape() {
        let id = new_id();
        let groups: Vec<&str> = id.split('-').collect();
        let lengths: Vec<usize> = groups.iter().map(|group| group.len()).collect();
        assert_eq!(lengths, [8, 4, 4, 4, 12]);
        assert!(groups[2].starts_with('4'));
        assert!(matches!(groups[3].as_bytes()[0], b'8' | b'9' | b'a' | b'b'));
        assert!(id.chars().all(|c| c == '-' || c.is_ascii_hexdigit()));
    }

    #[test]
    fn ids_are_unique() {
        let ids: std::collections::HashSet<String> = (0..1000).map(|_| new_id()).collect();
        assert_eq!(ids.len(), 1000);
    }
}
