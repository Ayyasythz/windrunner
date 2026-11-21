use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};

const HEADER: usize = 8;
const SNAPSHOT_TMP: &str = "snapshot.tmp";

/// Append-only log plus snapshots. `wal-N` holds the records written after
/// `snapshot-N`; generation 0 has no snapshot.
pub struct Journal {
    dir: PathBuf,
    generation: u64,
    wal: BufWriter<File>,
}

pub struct Recovered {
    pub snapshot: Option<Vec<u8>>,
    pub records: Vec<Vec<u8>>,
    /// Bytes of an incomplete or corrupt record cut off the end of the log.
    pub dropped_bytes: u64,
}

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

fn write_frame(w: &mut impl Write, payload: &[u8]) -> io::Result<()> {
    let len = u32::try_from(payload.len())
        .ok()
        .filter(|&l| l > 0)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "payload empty or too large"))?;
    w.write_all(&len.to_le_bytes())?;
    w.write_all(&crc32fast::hash(payload).to_le_bytes())?;
    w.write_all(payload)
}

fn read_frames(bytes: &[u8]) -> (Vec<Vec<u8>>, usize) {
    let mut frames = Vec::new();
    let mut pos = 0;
    while let Some(header) = bytes.get(pos..pos + HEADER) {
        let len = u32::from_le_bytes(header[..4].try_into().unwrap()) as usize;
        let crc = u32::from_le_bytes(header[4..].try_into().unwrap());
        let Some(payload) = bytes.get(pos + HEADER..pos + HEADER + len) else {
            break;
        };
        // A zero-filled tail would otherwise parse as an endless run of valid
        // empty frames (the CRC of nothing is 0).
        if len == 0 || crc32fast::hash(payload) != crc {
            break;
        }
        frames.push(payload.to_vec());
        pos += HEADER + len;
    }
    (frames, pos)
}

fn generation_of(name: &str, prefix: &str, suffix: &str) -> Option<u64> {
    name.strip_prefix(prefix)?
        .strip_suffix(suffix)?
        .parse()
        .ok()
}

fn open_append(path: &Path) -> io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

impl Journal {
    pub fn open(dir: impl AsRef<Path>) -> io::Result<(Journal, Recovered)> {
        let dir = dir.as_ref().to_path_buf();
        fs::create_dir_all(&dir)?;

        let (mut snapshots, mut wals) = (Vec::new(), Vec::new());
        for entry in fs::read_dir(&dir)? {
            let name = entry?.file_name();
            let Some(name) = name.to_str() else { continue };
            if let Some(g) = generation_of(name, "snapshot-", ".bin") {
                snapshots.push(g);
            } else if let Some(g) = generation_of(name, "wal-", ".log") {
                wals.push(g);
            }
        }
        let generation = snapshots.iter().copied().max().unwrap_or(0);
        let mut journal_paths = JournalPaths { dir: &dir };

        let snapshot = if generation > 0 {
            let bytes = fs::read(journal_paths.snapshot(generation))?;
            let (mut frames, valid) = read_frames(&bytes);
            if frames.len() != 1 || valid != bytes.len() {
                return Err(invalid("snapshot is corrupt"));
            }
            frames.pop()
        } else {
            None
        };

        let wal_path = journal_paths.wal(generation);
        let (records, valid, total) = match fs::read(&wal_path) {
            Ok(bytes) => {
                let (frames, valid) = read_frames(&bytes);
                (frames, valid as u64, bytes.len() as u64)
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => (Vec::new(), 0, 0),
            Err(e) => return Err(e),
        };
        let file = open_append(&wal_path)?;
        if valid < total {
            file.set_len(valid)?;
            file.sync_all()?;
        }

        journal_paths.remove_stale(&snapshots, &wals, generation);
        let _ = fs::remove_file(dir.join(SNAPSHOT_TMP));

        let journal = Journal {
            dir,
            generation,
            wal: BufWriter::new(file),
        };
        let recovered = Recovered {
            snapshot,
            records,
            dropped_bytes: total - valid,
        };
        Ok((journal, recovered))
    }

    /// Buffered; the record is only durable after `sync`.
    pub fn append(&mut self, payload: &[u8]) -> io::Result<()> {
        write_frame(&mut self.wal, payload)
    }

    pub fn sync(&mut self) -> io::Result<()> {
        self.wal.flush()?;
        self.wal.get_ref().sync_data()
    }

    /// `payload` must capture the state after every record appended so far;
    /// those records are discarded.
    pub fn snapshot(&mut self, payload: &[u8]) -> io::Result<()> {
        self.sync()?;
        let paths = JournalPaths { dir: &self.dir };
        let next = self.generation + 1;

        let tmp = self.dir.join(SNAPSHOT_TMP);
        let mut file = File::create(&tmp)?;
        write_frame(&mut file, payload)?;
        file.sync_all()?;
        fs::rename(&tmp, paths.snapshot(next))?;
        // A rename is only durable once the directory entry itself is synced.
        #[cfg(unix)]
        File::open(&self.dir)?.sync_all()?;

        let wal = open_append(&paths.wal(next))?;
        let previous = self.generation;
        let _ = fs::remove_file(paths.wal(previous));
        let _ = fs::remove_file(paths.snapshot(previous));
        self.wal = BufWriter::new(wal);
        self.generation = next;
        Ok(())
    }
}

struct JournalPaths<'a> {
    dir: &'a Path,
}

impl JournalPaths<'_> {
    fn snapshot(&self, generation: u64) -> PathBuf {
        self.dir.join(format!("snapshot-{generation:020}.bin"))
    }

    fn wal(&self, generation: u64) -> PathBuf {
        self.dir.join(format!("wal-{generation:020}.log"))
    }

    fn remove_stale(&mut self, snapshots: &[u64], wals: &[u64], current: u64) {
        for &g in snapshots.iter().filter(|&&g| g < current) {
            let _ = fs::remove_file(self.snapshot(g));
        }
        for &g in wals.iter().filter(|&&g| g < current) {
            let _ = fs::remove_file(self.wal(g));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reopen(dir: &Path) -> (Journal, Recovered) {
        Journal::open(dir).unwrap()
    }

    fn file_names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn records_survive_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let (mut journal, recovered) = reopen(dir.path());
        assert!(recovered.snapshot.is_none() && recovered.records.is_empty());
        journal.append(b"one").unwrap();
        journal.append(b"two").unwrap();
        journal.sync().unwrap();
        drop(journal);

        let (mut journal, recovered) = reopen(dir.path());
        assert_eq!(recovered.records, vec![b"one".to_vec(), b"two".to_vec()]);
        journal.append(b"three").unwrap();
        drop(journal);
        assert_eq!(reopen(dir.path()).1.records.len(), 3);
    }

    #[test]
    fn torn_or_corrupt_tail_is_cut_off() {
        let dir = tempfile::tempdir().unwrap();
        let (mut journal, _) = reopen(dir.path());
        journal.append(b"one").unwrap();
        journal.append(b"two").unwrap();
        drop(journal);
        let wal = dir.path().join(format!("wal-{:020}.log", 0));

        let len = fs::metadata(&wal).unwrap().len();
        OpenOptions::new()
            .write(true)
            .open(&wal)
            .unwrap()
            .set_len(len - 1)
            .unwrap();
        let (mut journal, recovered) = reopen(dir.path());
        assert_eq!(recovered.records, vec![b"one".to_vec()]);
        assert_eq!(recovered.dropped_bytes, HEADER as u64 + 2);
        journal.append(b"again").unwrap();
        drop(journal);

        let mut bytes = fs::read(&wal).unwrap();
        *bytes.last_mut().unwrap() ^= 0xff;
        bytes.extend_from_slice(&[0; 64]);
        fs::write(&wal, bytes).unwrap();
        let (_, recovered) = reopen(dir.path());
        assert_eq!(recovered.records, vec![b"one".to_vec()]);
    }

    #[test]
    fn snapshot_replaces_older_records() {
        let dir = tempfile::tempdir().unwrap();
        let (mut journal, _) = reopen(dir.path());
        journal.append(b"one").unwrap();
        journal.snapshot(b"state-1").unwrap();
        journal.append(b"two").unwrap();
        journal.snapshot(b"state-2").unwrap();
        journal.append(b"three").unwrap();
        drop(journal);

        assert_eq!(
            file_names(dir.path()),
            vec![
                format!("snapshot-{:020}.bin", 2),
                format!("wal-{:020}.log", 2)
            ]
        );
        let (_, recovered) = reopen(dir.path());
        assert_eq!(recovered.snapshot.as_deref(), Some(&b"state-2"[..]));
        assert_eq!(recovered.records, vec![b"three".to_vec()]);
    }

    #[test]
    fn corrupt_snapshot_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let (mut journal, _) = reopen(dir.path());
        journal.snapshot(b"state").unwrap();
        drop(journal);
        let path = dir.path().join(format!("snapshot-{:020}.bin", 1));
        let mut bytes = fs::read(&path).unwrap();
        *bytes.last_mut().unwrap() ^= 0xff;
        fs::write(&path, bytes).unwrap();
        assert!(Journal::open(dir.path()).is_err());
    }
}
