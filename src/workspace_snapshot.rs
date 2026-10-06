const MAGIC: &[u8; 4] = b"WNM1";
const MAX_FILES: usize = 128;
const MAX_PATH_BYTES: usize = 255;
const MAX_FILE_BYTES: usize = 1_048_576;
const MAX_SNAPSHOT_BYTES: usize = 16_777_216;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceFile {
    pub path: String,
    pub contents: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkspaceSnapshotError {
    InvalidFormat,
    InvalidPath,
    DuplicatePath,
    TooManyFiles,
    FileTooLarge,
    SnapshotTooLarge,
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::{decode, encode, WorkspaceFile, WorkspaceSnapshotError};

    fn file(path: &str, contents: &[u8]) -> WorkspaceFile {
        WorkspaceFile {
            path: path.into(),
            contents: contents.to_vec(),
        }
    }

    #[test]
    fn encodes_and_decodes_a_bounded_snapshot() {
        let files = vec![file("src/main.rs", b"fn main() {}"), file("README.md", b"demo")];

        assert_eq!(decode(&encode(&files).expect("encode")).expect("decode"), files);
    }

    #[test]
    fn rejects_noncanonical_or_escaping_paths() {
        for path in ["", "/etc/passwd", "../secret", "src/../secret", "a//b", "a\\b", "C:/secret"] {
            assert_eq!(encode(&[file(path, b"x")]), Err(WorkspaceSnapshotError::InvalidPath));
        }
    }

    #[test]
    fn rejects_duplicate_paths_and_file_directory_collisions() {
        assert_eq!(
            encode(&[file("src/main.rs", b"a"), file("src/main.rs", b"b")]),
            Err(WorkspaceSnapshotError::DuplicatePath),
        );
        assert_eq!(
            encode(&[file("src", b"a"), file("src/main.rs", b"b")]),
            Err(WorkspaceSnapshotError::InvalidPath),
        );
    }

    #[test]
    fn rejects_snapshots_outside_file_and_total_bounds() {
        assert_eq!(
            encode(&[file("large", &vec![0; 1_048_577])]),
            Err(WorkspaceSnapshotError::FileTooLarge),
        );
        let total = (0..17)
            .map(|index| file(&format!("f{index}"), &vec![0; 1_000_000]))
            .collect::<Vec<_>>();
        assert_eq!(encode(&total), Err(WorkspaceSnapshotError::SnapshotTooLarge));
        let many = (0..129).map(|index| file(&format!("f{index}"), b"")).collect::<Vec<_>>();
        assert_eq!(encode(&many), Err(WorkspaceSnapshotError::TooManyFiles));
    }

    #[test]
    fn rejects_truncated_trailing_and_malformed_snapshots() {
        let encoded = encode(&[file("a", b"x")]).expect("encode");
        assert_eq!(decode(&encoded[..encoded.len() - 1]), Err(WorkspaceSnapshotError::InvalidFormat));
        let mut trailing = encoded.clone();
        trailing.push(0);
        assert_eq!(decode(&trailing), Err(WorkspaceSnapshotError::InvalidFormat));
        assert_eq!(decode(b"bad!\0\0"), Err(WorkspaceSnapshotError::InvalidFormat));
    }

    #[test]
    fn decoding_rejects_duplicate_paths() {
        let first = encode(&[file("a", b"x")]).expect("encode");
        let mut duplicate = first.clone();
        duplicate.extend_from_slice(&first[6..]);
        duplicate[4..6].copy_from_slice(&2_u16.to_be_bytes());
        assert_eq!(decode(&duplicate), Err(WorkspaceSnapshotError::DuplicatePath));
    }

    #[test]
    fn encoded_paths_remain_unique_across_file_ordering() {
        let paths = BTreeSet::from(["a".to_owned(), "b/c".to_owned()]);
        let files = vec![file("b/c", b"2"), file("a", b"1")];
        let round_trip = decode(&encode(&files).expect("encode")).expect("decode");
        assert_eq!(round_trip.into_iter().map(|entry| entry.path).collect::<BTreeSet<_>>(), paths);
    }
}
