//! A text log kept in memory and saved to `\EFI\ferrite\<name>.txt` on the boot volume.

use alloc::format;
use alloc::string::String;

use uefi::fs::{FileSystem, PathBuf};
use uefi::{CString16, boot, cstr16, println};

/// Whether log lines are also printed on the firmware console.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Echo {
    Console,
    /// For apps that own the screen (the ratatui UI).
    Silent,
}

pub struct BootLog {
    fs: Option<FileSystem>,
    path: Option<PathBuf>,
    display: String,
    text: String,
    echo: Echo,
}

impl BootLog {
    /// Open `\EFI\ferrite\<name>.txt`. Without a writable boot volume the log
    /// still collects lines in memory and echoes them.
    #[must_use]
    pub fn open(name: &str, echo: Echo) -> Self {
        let display = format!("\\EFI\\ferrite\\{name}.txt");
        let mut fs = boot::get_image_file_system(boot::image_handle()).ok().map(FileSystem::new);
        if let Some(fs) = fs.as_mut() {
            let _ = fs.create_dir_all(cstr16!("\\EFI\\ferrite"));
        }
        Self {
            fs,
            path: CString16::try_from(display.as_str()).ok().map(PathBuf::from),
            display,
            text: String::new(),
            echo,
        }
    }

    /// Append a line. The file is saved according to the flush policy, not on every line.
    pub fn line(&mut self, s: &str) {
        self.text.push_str(s);
        self.text.push('\n');
        if self.echo == Echo::Console {
            println!("{s}");
        }
        self.flush();
    }

    /// Append a line and save the file now: use before anything that may hang or reset.
    pub fn milestone(&mut self, s: &str) {
        self.line(s);
        self.flush();
    }

    /// Save the file if the flush policy says it is due. Call from long-running loops.
    pub fn poll(&mut self) {}

    /// Save the file now. Returns whether it was written.
    pub fn flush(&mut self) -> bool {
        match (self.fs.as_mut(), self.path.as_ref()) {
            (Some(fs), Some(path)) => fs.write(path, self.text.as_bytes()).is_ok(),
            _ => false,
        }
    }

    /// Path shown to the user, e.g. `\EFI\ferrite\dbcping.txt`.
    #[must_use]
    pub fn display_path(&self) -> &str {
        &self.display
    }
}
