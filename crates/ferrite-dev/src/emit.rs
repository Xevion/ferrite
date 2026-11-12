//! Timestamped line output to stdout and an optional file, and splitting a byte stream into lines.

use std::fs::File;
use std::io::{self, Write};
use std::path::Path;

/// The wall clock as `HH:MM:SS.mmm`.
pub fn wall_clock() -> String {
    jiff::Zoned::now().strftime("%T%.3f").to_string()
}

/// Where captured lines go. A sink that fails is reported once and dropped.
pub struct Emitter {
    sinks: Vec<Box<dyn Write + Send>>,
    stamp: fn() -> String,
}

impl Emitter {
    pub fn new() -> Self {
        Self { sinks: Vec::new(), stamp: wall_clock }
    }

    #[must_use]
    pub fn with_stdout(self) -> Self {
        self.with_sink(io::stdout())
    }

    pub fn with_file(self, path: &Path) -> io::Result<Self> {
        Ok(self.with_sink(File::create(path)?))
    }

    #[must_use]
    pub fn with_sink(mut self, sink: impl Write + Send + 'static) -> Self {
        self.sinks.push(Box::new(sink));
        self
    }

    #[must_use]
    pub fn with_stamp(mut self, stamp: fn() -> String) -> Self {
        self.stamp = stamp;
        self
    }

    /// One line prefixed with the current time.
    pub fn line(&mut self, text: &str) {
        let line = format!("{} {text}\n", (self.stamp)());
        self.write(line.as_bytes());
    }

    /// One log record from the target, one stamped line per line of text it holds. The record
    /// carries no newline of its own; the line ending is added here.
    pub fn record(&mut self, bytes: &[u8]) {
        String::from_utf8_lossy(bytes).lines().for_each(|line| self.line(line));
    }

    /// One line exactly as given.
    pub fn plain(&mut self, text: &str) {
        self.write(format!("{text}\n").as_bytes());
    }

    fn write(&mut self, bytes: &[u8]) {
        self.sinks.retain_mut(|sink| match sink.write_all(bytes).and_then(|()| sink.flush()) {
            Ok(()) => true,
            Err(e) => {
                eprintln!("ferrite-dev: dropping an output after a write failure: {e}");
                false
            },
        });
    }
}

impl Default for Emitter {
    fn default() -> Self {
        Self::new()
    }
}

/// Cuts a byte stream into lines at `\n`, dropping a trailing `\r`.
#[derive(Debug, Default)]
pub struct LineBuffer {
    partial: Vec<u8>,
}

impl LineBuffer {
    pub fn push(&mut self, bytes: &[u8], mut on_line: impl FnMut(&str)) {
        for &byte in bytes {
            if byte == b'\n' {
                self.take(&mut on_line);
            } else {
                self.partial.push(byte);
            }
        }
    }

    /// Hand over a final line that never got its newline.
    pub fn finish(&mut self, mut on_line: impl FnMut(&str)) {
        if !self.partial.is_empty() {
            self.take(&mut on_line);
        }
    }

    fn take(&mut self, on_line: &mut impl FnMut(&str)) {
        if self.partial.last() == Some(&b'\r') {
            self.partial.pop();
        }
        on_line(&String::from_utf8_lossy(&self.partial));
        self.partial.clear();
    }
}

#[cfg(test)]
pub mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use assert2::check;
    use parking_lot::Mutex;
    use rstest::rstest;

    use super::*;

    /// A sink that keeps what it was given, for reading back after the emitter is done with it.
    #[derive(Clone, Default)]
    pub struct Shared(Arc<Mutex<Vec<u8>>>);

    impl Shared {
        pub fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock()).into_owned()
        }

        pub fn bytes(&self) -> Vec<u8> {
            self.0.lock().clone()
        }
    }

    impl Write for Shared {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn lines(chunks: &[&str]) -> Vec<String> {
        let mut out = Vec::new();
        let mut buffer = LineBuffer::default();
        for chunk in chunks {
            buffer.push(chunk.as_bytes(), |line| out.push(line.to_owned()));
        }
        buffer.finish(|line| out.push(line.to_owned()));
        out
    }

    #[rstest]
    #[case::whole(&["a\nb\n"], &["a", "b"])]
    #[case::split_mid_line(&["he", "llo\nwor", "ld\n"], &["hello", "world"])]
    #[case::crlf(&["a\r\nb\r\n"], &["a", "b"])]
    #[case::blank_line(&["a\n\nb\n"], &["a", "", "b"])]
    #[case::unterminated_tail(&["a\nlast"], &["a", "last"])]
    #[case::crlf_split_between_bytes(&["a\r", "\nb\n"], &["a", "b"])]
    fn lines_are_cut_at_newlines(#[case] chunks: &[&str], #[case] want: &[&str]) {
        check!(lines(chunks) == want);
    }

    #[test]
    fn emitter_stamps_lines_and_writes_every_sink() {
        let (a, b) = (Shared::default(), Shared::default());
        let mut emitter = Emitter::new().with_sink(a.clone()).with_sink(b.clone()).with_stamp(|| "12:00:00.001".into());
        emitter.line("--- link up ---");
        emitter.plain("raw");
        check!(a.text() == "12:00:00.001 --- link up ---\nraw\n");
        check!(b.text() == a.text());
    }

    #[test]
    fn a_failing_sink_is_dropped_and_the_others_keep_working() {
        struct Broken(Arc<AtomicUsize>);
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Err(io::Error::other("full"))
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let attempts = Arc::new(AtomicUsize::new(0));
        let good = Shared::default();
        let mut emitter = Emitter::new().with_sink(Broken(attempts.clone())).with_sink(good.clone());
        emitter.plain("one");
        emitter.plain("two");
        check!(attempts.load(Ordering::SeqCst) == 1);
        check!(good.text() == "one\ntwo\n");
    }

    #[test]
    fn wall_clock_has_millisecond_shape() {
        let stamp = wall_clock();
        let bytes = stamp.as_bytes();
        check!(bytes.len() == 12, "{stamp}");
        check!((bytes[2], bytes[5], bytes[8]) == (b':', b':', b'.'), "{stamp}");
    }
}
