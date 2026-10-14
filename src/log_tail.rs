use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

pub struct LogTail {
    path: PathBuf,
    offset: u64,
    partial: Vec<u8>,
}

impl LogTail {
    pub fn from_end(path: &Path) -> io::Result<LogTail> {
        let offset = std::fs::metadata(path)?.len();
        Ok(LogTail {
            path: path.to_path_buf(),
            offset,
            partial: Vec::new(),
        })
    }

    pub fn lines(&mut self) -> io::Result<Vec<String>> {
        let size = std::fs::metadata(&self.path)?.len();
        if size < self.offset {
            self.offset = 0;
            self.partial.clear();
        }
        let mut file = File::open(&self.path)?;
        file.seek(SeekFrom::Start(self.offset))?;
        let mut data = Vec::new();
        file.read_to_end(&mut data)?;
        self.offset += data.len() as u64;
        let mut buffer = std::mem::take(&mut self.partial);
        buffer.extend_from_slice(&data);
        let mut lines = Vec::new();
        let mut start = 0;
        for (index, byte) in buffer.iter().enumerate() {
            if *byte == b'\n' {
                lines.push(String::from_utf8_lossy(&buffer[start..index]).into_owned());
                start = index + 1;
            }
        }
        self.partial = buffer[start..].to_vec();
        Ok(lines)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn tail_reads_only_new_complete_lines_and_survives_truncation() {
        let dir = std::env::temp_dir().join(format!("claude-nonstop-tail-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("claude-nonstop.log");
        std::fs::write(&path, "old line\n").unwrap();
        let mut tail = LogTail::from_end(&path).unwrap();
        assert!(tail.lines().unwrap().is_empty());
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(b"first\nsecond part").unwrap();
        assert_eq!(tail.lines().unwrap(), vec!["first".to_owned()]);
        file.write_all(b" done\n").unwrap();
        assert_eq!(tail.lines().unwrap(), vec!["second part done".to_owned()]);
        std::fs::write(&path, "fresh\n").unwrap();
        assert_eq!(tail.lines().unwrap(), vec!["fresh".to_owned()]);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
