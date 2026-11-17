use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

/// Agent communication log of one task. Each write goes straight to the file.
pub struct Log {
    file: fs::File,
    /// Byte range of the streamed message not yet finished by `finish`.
    streamed: Option<(u64, u64)>,
}

impl Log {
    pub fn open(path: &Path) -> Result<Self, String> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(Self { file, streamed: None })
    }

    pub fn write(&mut self, s: &str) {
        let _ = self.file.write_all(s.as_bytes());
    }

    fn len(&self) -> u64 {
        self.file.metadata().map_or(0, |m| m.len())
    }

    /// Writes a streamed text delta. The message stays open until `finish`.
    pub fn delta(&mut self, s: &str) {
        let start = self.streamed.map_or_else(|| self.len(), |(start, _)| start);
        self.write(s);
        self.streamed = Some((start, self.len()));
    }

    /// Ends the streamed message with its final `text`. Replaces the streamed text when nothing was written after it.
    /// Without a streamed message, appends `text` as a line. Empty text is skipped.
    pub fn finish(&mut self, text: &str) {
        match self.streamed.take() {
            // Other output after the stream stays. Then the final text is appended.
            Some((start, end)) if self.len() == end => {
                let _ = self.file.set_len(start);
            }
            None if text.is_empty() => return,
            _ => {}
        }
        self.write(&format!("{text}\n"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{env, process};

    #[test]
    fn finish_replaces_streamed_text_with_final_text() {
        let path = env::temp_dir().join(format!("dl-log-{}.log", process::id()));
        let _ = fs::remove_file(&path);
        let mut log = Log::open(&path).unwrap();
        log.write("=== step ===\n");
        log.delta("Hel");
        log.delta("lo");
        log.finish("Hello world");
        log.finish("");
        assert_eq!(fs::read_to_string(&path).unwrap(), "=== step ===\nHello world\n");
        fs::remove_file(path).unwrap();
    }
}
