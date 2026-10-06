use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Component, Path};

const MAGIC: &[u8; 4] = b"WNM1";
const MAX_FILES: usize = 128;
const MAX_PATH_BYTES: usize = 255;
const MAX_FILE_BYTES: usize = 1_048_576;
pub const MAX_SNAPSHOT_BYTES: usize = 16_777_216;
const TAR_BLOCK_BYTES: usize = 512;
pub const MAX_TAR_BYTES: usize = MAX_SNAPSHOT_BYTES + MAX_FILES * 1024 + TAR_BLOCK_BYTES * 2;
const MAX_DIRECTORY_ENTRIES: usize = 512;

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
    TooManyEntries,
    FileTooLarge,
    SnapshotTooLarge,
    UnsupportedFileType,
    Filesystem,
}

pub fn encode(files: &[WorkspaceFile]) -> Result<Vec<u8>, WorkspaceSnapshotError> {
    validate_files(files)?;

    let mut encoded = Vec::new();
    encoded.extend_from_slice(MAGIC);
    encoded.extend_from_slice(&(files.len() as u16).to_be_bytes());

    for file in files {
        let path = file.path.as_bytes();
        encoded.extend_from_slice(&(path.len() as u16).to_be_bytes());
        encoded.extend_from_slice(&(file.contents.len() as u32).to_be_bytes());
        encoded.extend_from_slice(path);
        encoded.extend_from_slice(&file.contents);

        if encoded.len() > MAX_SNAPSHOT_BYTES {
            return Err(WorkspaceSnapshotError::SnapshotTooLarge);
        }
    }

    Ok(encoded)
}

pub fn decode(encoded: &[u8]) -> Result<Vec<WorkspaceFile>, WorkspaceSnapshotError> {
    if encoded.len() > MAX_SNAPSHOT_BYTES {
        return Err(WorkspaceSnapshotError::SnapshotTooLarge);
    }

    if encoded.get(..MAGIC.len()) != Some(MAGIC) {
        return Err(WorkspaceSnapshotError::InvalidFormat);
    }

    let mut cursor = MAGIC.len();
    let count = read_u16(encoded, &mut cursor)? as usize;

    if count > MAX_FILES {
        return Err(WorkspaceSnapshotError::TooManyFiles);
    }

    let mut files = Vec::with_capacity(count);

    for _ in 0..count {
        let path_length = read_u16(encoded, &mut cursor)? as usize;
        let file_length = read_u32(encoded, &mut cursor)? as usize;

        if file_length > MAX_FILE_BYTES {
            return Err(WorkspaceSnapshotError::FileTooLarge);
        }

        let path_bytes = read_bytes(encoded, &mut cursor, path_length)?;
        let path = std::str::from_utf8(path_bytes)
            .map_err(|_| WorkspaceSnapshotError::InvalidPath)?
            .to_owned();
        let contents = read_bytes(encoded, &mut cursor, file_length)?.to_vec();

        files.push(WorkspaceFile { path, contents });
    }

    if cursor != encoded.len() {
        return Err(WorkspaceSnapshotError::InvalidFormat);
    }

    validate_files(&files)?;
    Ok(files)
}

pub fn encode_directory(root: &Path) -> Result<Vec<u8>, WorkspaceSnapshotError> {
    let metadata = fs::symlink_metadata(root).map_err(|_| WorkspaceSnapshotError::Filesystem)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(WorkspaceSnapshotError::UnsupportedFileType);
    }

    let root = root
        .canonicalize()
        .map_err(|_| WorkspaceSnapshotError::Filesystem)?;
    let mut files = Vec::new();
    let mut entry_count = 0;
    let mut snapshot_bytes = 6;
    collect_directory_files(
        &root,
        &root,
        &mut files,
        &mut entry_count,
        &mut snapshot_bytes,
    )?;
    files.sort_by(|left, right| left.path.cmp(&right.path));
    encode(&files)
}

fn collect_directory_files(
    root: &Path,
    directory: &Path,
    files: &mut Vec<WorkspaceFile>,
    entry_count: &mut usize,
    snapshot_bytes: &mut usize,
) -> Result<(), WorkspaceSnapshotError> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(directory).map_err(|_| WorkspaceSnapshotError::Filesystem)? {
        let entry = entry.map_err(|_| WorkspaceSnapshotError::Filesystem)?;
        *entry_count += 1;
        if *entry_count > MAX_DIRECTORY_ENTRIES {
            return Err(WorkspaceSnapshotError::TooManyEntries);
        }
        entries.push(entry);
    }
    entries.sort_by_key(|entry| entry.file_name().to_string_lossy().into_owned());

    for entry in entries {
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|_| WorkspaceSnapshotError::Filesystem)?;
        if file_type.is_symlink() {
            return Err(WorkspaceSnapshotError::UnsupportedFileType);
        }
        if file_type.is_dir() {
            collect_directory_files(root, &path, files, entry_count, snapshot_bytes)?;
            continue;
        }
        if !file_type.is_file() {
            return Err(WorkspaceSnapshotError::UnsupportedFileType);
        }
        if files.len() == MAX_FILES {
            return Err(WorkspaceSnapshotError::TooManyFiles);
        }

        let relative_path = snapshot_relative_path(root, &path)?;
        if !valid_path(&relative_path) {
            return Err(WorkspaceSnapshotError::InvalidPath);
        }

        let mut contents = Vec::new();
        File::open(&path)
            .map_err(|_| WorkspaceSnapshotError::Filesystem)?
            .take(MAX_FILE_BYTES as u64 + 1)
            .read_to_end(&mut contents)
            .map_err(|_| WorkspaceSnapshotError::Filesystem)?;
        if contents.len() > MAX_FILE_BYTES {
            return Err(WorkspaceSnapshotError::FileTooLarge);
        }

        *snapshot_bytes = snapshot_bytes
            .checked_add(6 + relative_path.len() + contents.len())
            .ok_or(WorkspaceSnapshotError::SnapshotTooLarge)?;
        if *snapshot_bytes > MAX_SNAPSHOT_BYTES {
            return Err(WorkspaceSnapshotError::SnapshotTooLarge);
        }
        files.push(WorkspaceFile {
            path: relative_path,
            contents,
        });
    }
    Ok(())
}

fn snapshot_relative_path(root: &Path, path: &Path) -> Result<String, WorkspaceSnapshotError> {
    path.strip_prefix(root)
        .map_err(|_| WorkspaceSnapshotError::InvalidPath)?
        .components()
        .map(|component| match component {
            Component::Normal(value) => value
                .to_str()
                .map(str::to_owned)
                .ok_or(WorkspaceSnapshotError::InvalidPath),
            _ => Err(WorkspaceSnapshotError::InvalidPath),
        })
        .collect::<Result<Vec<_>, _>>()
        .map(|components| components.join("/"))
}

pub fn encode_tar(files: &[WorkspaceFile]) -> Result<Vec<u8>, WorkspaceSnapshotError> {
    validate_files(files)?;

    let mut snapshot_bytes = 6_usize;
    let mut archive = Vec::new();

    for file in files {
        let path_bytes = file.path.len();
        snapshot_bytes = snapshot_bytes
            .checked_add(6 + path_bytes + file.contents.len())
            .ok_or(WorkspaceSnapshotError::SnapshotTooLarge)?;
        if snapshot_bytes > MAX_SNAPSHOT_BYTES {
            return Err(WorkspaceSnapshotError::SnapshotTooLarge);
        }

        let (name, prefix) = split_ustar_path(&file.path)?;
        let mut header = [0_u8; TAR_BLOCK_BYTES];
        header[..name.len()].copy_from_slice(name.as_bytes());
        write_octal(&mut header, 100, 8, 0o644)?;
        write_octal(&mut header, 108, 8, 0)?;
        write_octal(&mut header, 116, 8, 0)?;
        write_octal(&mut header, 124, 12, file.contents.len() as u64)?;
        write_octal(&mut header, 136, 12, 0)?;
        header[148..156].fill(b' ');
        header[156] = b'0';
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        header[345..345 + prefix.len()].copy_from_slice(prefix.as_bytes());

        let checksum = header.iter().map(|byte| u64::from(*byte)).sum::<u64>();
        let checksum = format!("{checksum:06o}\0 ");
        header[148..156].copy_from_slice(checksum.as_bytes());
        archive.extend_from_slice(&header);
        archive.extend_from_slice(&file.contents);

        let padding = (TAR_BLOCK_BYTES - file.contents.len() % TAR_BLOCK_BYTES) % TAR_BLOCK_BYTES;
        archive.resize(archive.len() + padding, 0);
    }

    archive.resize(archive.len() + TAR_BLOCK_BYTES * 2, 0);
    if archive.len() > MAX_TAR_BYTES {
        return Err(WorkspaceSnapshotError::SnapshotTooLarge);
    }

    Ok(archive)
}

pub fn decode_tar(archive: &[u8]) -> Result<Vec<WorkspaceFile>, WorkspaceSnapshotError> {
    if archive.len() > MAX_TAR_BYTES
        || !archive.chunks_exact(TAR_BLOCK_BYTES).remainder().is_empty()
    {
        return Err(WorkspaceSnapshotError::InvalidFormat);
    }

    let mut cursor = 0;
    let mut files = Vec::new();
    while cursor + TAR_BLOCK_BYTES <= archive.len() {
        let header = &archive[cursor..cursor + TAR_BLOCK_BYTES];
        if header.iter().all(|byte| *byte == 0) {
            if archive.len() - cursor < TAR_BLOCK_BYTES * 2
                || archive[cursor..].iter().any(|byte| *byte != 0)
            {
                return Err(WorkspaceSnapshotError::InvalidFormat);
            }
            validate_files(&files)?;
            return Ok(files);
        }
        validate_tar_header(header)?;

        let name = tar_string(&header[..100])?;
        let prefix = tar_string(&header[345..500])?;
        let mut path = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        if let Some(relative) = path.strip_prefix("./") {
            path = relative.to_owned();
        }
        if files.len() == MAX_FILES {
            return Err(WorkspaceSnapshotError::TooManyFiles);
        }
        let size = tar_octal(&header[124..136])?;
        if size > MAX_FILE_BYTES as u64 {
            return Err(WorkspaceSnapshotError::FileTooLarge);
        }
        let size = size as usize;
        let data_start = cursor + TAR_BLOCK_BYTES;
        let data_end = data_start
            .checked_add(size)
            .ok_or(WorkspaceSnapshotError::InvalidFormat)?;
        let padded_end = data_start
            .checked_add(size.div_ceil(TAR_BLOCK_BYTES) * TAR_BLOCK_BYTES)
            .ok_or(WorkspaceSnapshotError::InvalidFormat)?;
        let contents = archive
            .get(data_start..data_end)
            .ok_or(WorkspaceSnapshotError::InvalidFormat)?
            .to_vec();
        let padding = archive
            .get(data_end..padded_end)
            .ok_or(WorkspaceSnapshotError::InvalidFormat)?;
        if padding.iter().any(|byte| *byte != 0) {
            return Err(WorkspaceSnapshotError::InvalidFormat);
        }

        files.push(WorkspaceFile { path, contents });
        cursor = padded_end;
    }
    Err(WorkspaceSnapshotError::InvalidFormat)
}

fn validate_tar_header(header: &[u8]) -> Result<(), WorkspaceSnapshotError> {
    let posix_ustar = &header[257..263] == b"ustar\0" && &header[263..265] == b"00";
    let busybox_ustar = &header[257..265] == b"ustar  \0";
    if header[156] != b'0' || !(posix_ustar || busybox_ustar) {
        return Err(WorkspaceSnapshotError::UnsupportedFileType);
    }

    let expected = tar_octal(&header[148..156])?;
    let actual = header
        .iter()
        .enumerate()
        .map(|(index, byte)| {
            if (148..156).contains(&index) {
                u64::from(b' ')
            } else {
                u64::from(*byte)
            }
        })
        .sum::<u64>();
    if expected != actual {
        return Err(WorkspaceSnapshotError::InvalidFormat);
    }
    Ok(())
}

fn tar_string(field: &[u8]) -> Result<String, WorkspaceSnapshotError> {
    let end = field
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(field.len());
    if field[end..].iter().any(|byte| *byte != 0) {
        return Err(WorkspaceSnapshotError::InvalidFormat);
    }
    std::str::from_utf8(&field[..end])
        .map(str::to_owned)
        .map_err(|_| WorkspaceSnapshotError::InvalidPath)
}

fn tar_octal(field: &[u8]) -> Result<u64, WorkspaceSnapshotError> {
    let end = field
        .iter()
        .position(|byte| *byte == 0 || *byte == b' ')
        .unwrap_or(field.len());
    if end == 0
        || field[end..].iter().any(|byte| *byte != 0 && *byte != b' ')
        || field[..end]
            .iter()
            .any(|byte| !(b'0'..=b'7').contains(byte))
    {
        return Err(WorkspaceSnapshotError::InvalidFormat);
    }
    field[..end].iter().try_fold(0_u64, |value, digit| {
        value
            .checked_mul(8)
            .and_then(|value| value.checked_add(u64::from(digit - b'0')))
            .ok_or(WorkspaceSnapshotError::InvalidFormat)
    })
}

fn validate_files(files: &[WorkspaceFile]) -> Result<(), WorkspaceSnapshotError> {
    if files.len() > MAX_FILES {
        return Err(WorkspaceSnapshotError::TooManyFiles);
    }

    let mut paths = BTreeSet::new();

    for file in files {
        if !valid_path(&file.path) {
            return Err(WorkspaceSnapshotError::InvalidPath);
        }
        if file.contents.len() > MAX_FILE_BYTES {
            return Err(WorkspaceSnapshotError::FileTooLarge);
        }
        if paths.contains(file.path.as_str()) {
            return Err(WorkspaceSnapshotError::DuplicatePath);
        }

        let components = file.path.split('/').collect::<Vec<_>>();
        let mut prefix = String::new();

        for component in components.iter().take(components.len() - 1) {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(component);

            if paths.contains(prefix.as_str()) {
                return Err(WorkspaceSnapshotError::InvalidPath);
            }
        }

        paths.insert(file.path.as_str());

        if paths.iter().any(|path| {
            path.len() > file.path.len()
                && path.starts_with(&file.path)
                && path.as_bytes().get(file.path.len()) == Some(&b'/')
        }) {
            return Err(WorkspaceSnapshotError::InvalidPath);
        }
    }

    Ok(())
}

fn valid_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= MAX_PATH_BYTES
        && !path.starts_with('/')
        && !path.contains(['\\', ':'])
        && !path.chars().any(char::is_control)
        && path
            .split('/')
            .all(|component| !component.is_empty() && component != "." && component != "..")
}

fn split_ustar_path(path: &str) -> Result<(&str, &str), WorkspaceSnapshotError> {
    if path.len() <= 100 {
        return Ok((path, ""));
    }

    path.match_indices('/')
        .rev()
        .find_map(|(index, _)| {
            let prefix = &path[..index];
            let name = &path[index + 1..];
            (prefix.len() <= 155 && name.len() <= 100).then_some((name, prefix))
        })
        .ok_or(WorkspaceSnapshotError::InvalidPath)
}

fn write_octal(
    header: &mut [u8; TAR_BLOCK_BYTES],
    offset: usize,
    width: usize,
    value: u64,
) -> Result<(), WorkspaceSnapshotError> {
    let digits = format!("{value:0width$o}", width = width - 1);
    if digits.len() >= width {
        return Err(WorkspaceSnapshotError::SnapshotTooLarge);
    }
    header[offset..offset + digits.len()].copy_from_slice(digits.as_bytes());
    header[offset + width - 1] = 0;
    Ok(())
}

fn read_u16(encoded: &[u8], cursor: &mut usize) -> Result<u16, WorkspaceSnapshotError> {
    let bytes = read_bytes(encoded, cursor, 2)?;
    Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
}

fn read_u32(encoded: &[u8], cursor: &mut usize) -> Result<u32, WorkspaceSnapshotError> {
    let bytes = read_bytes(encoded, cursor, 4)?;
    Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn read_bytes<'a>(
    encoded: &'a [u8],
    cursor: &mut usize,
    length: usize,
) -> Result<&'a [u8], WorkspaceSnapshotError> {
    let end = cursor
        .checked_add(length)
        .ok_or(WorkspaceSnapshotError::InvalidFormat)?;
    let bytes = encoded
        .get(*cursor..end)
        .ok_or(WorkspaceSnapshotError::InvalidFormat)?;
    *cursor = end;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::{
        decode, decode_tar, encode, encode_directory, encode_tar, WorkspaceFile,
        WorkspaceSnapshotError, TAR_BLOCK_BYTES,
    };

    static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    fn file(path: &str, contents: &[u8]) -> WorkspaceFile {
        WorkspaceFile {
            path: path.into(),
            contents: contents.to_vec(),
        }
    }

    fn temporary_directory() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "wardnm-snapshot-{}-{}",
            std::process::id(),
            NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).expect("create test directory");
        path
    }

    #[test]
    fn encodes_and_decodes_a_bounded_snapshot() {
        let files = vec![
            file("src/main.rs", b"fn main() {}"),
            file("README.md", b"demo"),
        ];

        assert_eq!(
            decode(&encode(&files).expect("encode")).expect("decode"),
            files
        );
    }

    #[test]
    fn decodes_the_bounded_tar_archive_used_for_workspace_transfer() {
        let files = vec![
            file("src/main.rs", b"fn main() {}"),
            file("README.md", b"demo"),
        ];
        let archive = encode_tar(&files).expect("encode archive");

        assert_eq!(decode_tar(&archive).expect("decode archive"), files);
    }

    #[test]
    fn rejects_tar_archives_outside_the_supported_format() {
        let archive = encode_tar(&[file("result.txt", b"ok")]).expect("encode archive");
        let mut corrupt = archive.clone();
        corrupt[0] ^= 1;
        assert_eq!(
            decode_tar(&corrupt),
            Err(WorkspaceSnapshotError::InvalidFormat)
        );

        let mut oversized = archive;
        oversized.extend_from_slice(&[0; 1]);
        assert_eq!(
            decode_tar(&oversized),
            Err(WorkspaceSnapshotError::InvalidFormat)
        );
    }

    #[test]
    fn decodes_the_busybox_ustar_header_format() {
        let mut archive = encode_tar(&[file("result.txt", b"ok")]).expect("encode archive");
        archive[257..265].copy_from_slice(b"ustar  \0");
        archive[148..156].fill(b' ');
        let checksum = archive[..TAR_BLOCK_BYTES]
            .iter()
            .map(|byte| u64::from(*byte))
            .sum::<u64>();
        archive[148..156].copy_from_slice(format!("{checksum:06o}\0 ").as_bytes());

        assert_eq!(
            decode_tar(&archive).expect("decode archive"),
            [file("result.txt", b"ok")]
        );
    }

    #[test]
    fn rejects_noncanonical_or_escaping_paths() {
        for path in [
            "",
            "/etc/passwd",
            "../secret",
            "src/../secret",
            "a//b",
            "a\\b",
            "C:/secret",
        ] {
            assert_eq!(
                encode(&[file(path, b"x")]),
                Err(WorkspaceSnapshotError::InvalidPath)
            );
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
        assert_eq!(
            encode(&total),
            Err(WorkspaceSnapshotError::SnapshotTooLarge)
        );
        let many = (0..129)
            .map(|index| file(&format!("f{index}"), b""))
            .collect::<Vec<_>>();
        assert_eq!(encode(&many), Err(WorkspaceSnapshotError::TooManyFiles));
    }

    #[test]
    fn rejects_truncated_trailing_and_malformed_snapshots() {
        let encoded = encode(&[file("a", b"x")]).expect("encode");
        assert_eq!(
            decode(&encoded[..encoded.len() - 1]),
            Err(WorkspaceSnapshotError::InvalidFormat)
        );
        let mut trailing = encoded.clone();
        trailing.push(0);
        assert_eq!(
            decode(&trailing),
            Err(WorkspaceSnapshotError::InvalidFormat)
        );
        assert_eq!(
            decode(b"bad!\0\0"),
            Err(WorkspaceSnapshotError::InvalidFormat)
        );
    }

    #[test]
    fn decoding_rejects_duplicate_paths() {
        let first = encode(&[file("a", b"x")]).expect("encode");
        let mut duplicate = first.clone();
        duplicate.extend_from_slice(&first[6..]);
        duplicate[4..6].copy_from_slice(&2_u16.to_be_bytes());
        assert_eq!(
            decode(&duplicate),
            Err(WorkspaceSnapshotError::DuplicatePath)
        );
    }

    #[test]
    fn decoding_rejects_invalid_paths_and_declared_bounds() {
        let mut invalid_path = encode(&[file("a", b"x")]).expect("encode");
        invalid_path[12] = b'.';
        assert_eq!(
            decode(&invalid_path),
            Err(WorkspaceSnapshotError::InvalidPath)
        );

        let mut invalid_utf8 = encode(&[file("a", b"x")]).expect("encode");
        invalid_utf8[12] = 0xff;
        assert_eq!(
            decode(&invalid_utf8),
            Err(WorkspaceSnapshotError::InvalidPath)
        );

        let mut too_many = b"WNM1".to_vec();
        too_many.extend_from_slice(&129_u16.to_be_bytes());
        assert_eq!(decode(&too_many), Err(WorkspaceSnapshotError::TooManyFiles));

        let mut oversized_file = b"WNM1".to_vec();
        oversized_file.extend_from_slice(&1_u16.to_be_bytes());
        oversized_file.extend_from_slice(&1_u16.to_be_bytes());
        oversized_file.extend_from_slice(&1_048_577_u32.to_be_bytes());
        assert_eq!(
            decode(&oversized_file),
            Err(WorkspaceSnapshotError::FileTooLarge)
        );

        assert_eq!(
            decode(&vec![0; 16_777_217]),
            Err(WorkspaceSnapshotError::SnapshotTooLarge)
        );
    }

    #[test]
    fn encoded_paths_remain_unique_across_file_ordering() {
        let paths = BTreeSet::from(["a".to_owned(), "b/c".to_owned()]);
        let files = vec![file("b/c", b"2"), file("a", b"1")];
        let round_trip = decode(&encode(&files).expect("encode")).expect("decode");
        assert_eq!(
            round_trip
                .into_iter()
                .map(|entry| entry.path)
                .collect::<BTreeSet<_>>(),
            paths
        );
    }

    #[test]
    fn encodes_a_deterministic_regular_file_tar_archive() {
        let files = [file("src/main.rs", b"rust")];
        let archive = encode_tar(&files).expect("encode tar");

        assert_eq!(archive.len(), 2048);
        assert_eq!(&archive[..11], b"src/main.rs");
        assert_eq!(archive[156], b'0');
        assert_eq!(&archive[257..263], b"ustar\0");
        assert_eq!(&archive[512..516], b"rust");
        assert!(archive[516..1024].iter().all(|byte| *byte == 0));
        assert!(archive[1024..].iter().all(|byte| *byte == 0));

        let expected_checksum = u64::from_str_radix(
            std::str::from_utf8(&archive[148..154]).expect("checksum digits"),
            8,
        )
        .expect("octal checksum");
        let actual_checksum = archive[..512]
            .iter()
            .enumerate()
            .map(|(index, byte)| {
                if (148..156).contains(&index) {
                    b' ' as u64
                } else {
                    u64::from(*byte)
                }
            })
            .sum::<u64>();
        assert_eq!(expected_checksum, actual_checksum);
        assert_eq!(archive, encode_tar(&files).expect("repeat encode"));
    }

    #[test]
    fn encodes_long_paths_with_ustar_prefix_and_rejects_unrepresentable_paths() {
        let name = "a".repeat(100);
        let path = format!("src/{name}");
        let archive = encode_tar(&[file(&path, b"")]).expect("long path");
        assert_eq!(&archive[..100], name.as_bytes());
        assert_eq!(&archive[345..348], b"src");

        let unrepresentable = format!("{}/{}", "a".repeat(101), "b".repeat(101));
        assert_eq!(
            encode_tar(&[file(&unrepresentable, b"")]),
            Err(WorkspaceSnapshotError::InvalidPath)
        );
    }

    #[test]
    fn encodes_directory_files_in_deterministic_path_order() {
        let directory = temporary_directory();
        fs::create_dir(directory.join("a")).expect("create nested directory");
        fs::write(directory.join("a/z.js"), b"last").expect("write nested file");
        fs::write(directory.join("a.js"), b"first").expect("write root file");

        let encoded = encode_directory(&directory).expect("encode directory");
        let files = decode(&encoded).expect("decode directory snapshot");

        assert_eq!(files, [file("a.js", b"first"), file("a/z.js", b"last")]);
        assert_eq!(
            encoded,
            encode_directory(&directory).expect("repeat encode")
        );
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinks_in_workspace_directories() {
        use std::os::unix::fs::symlink;

        let directory = temporary_directory();
        fs::write(directory.join("target"), b"data").expect("write target");
        symlink("target", directory.join("link")).expect("create symlink");

        assert_eq!(
            encode_directory(&directory),
            Err(WorkspaceSnapshotError::UnsupportedFileType)
        );
        fs::remove_dir_all(directory).expect("remove test directory");
    }
}
