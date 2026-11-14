//! `firmware`: stage the firmware files the compiled Wi-Fi drivers can request.
//!
//! Nothing here is a hand list of files. Every driver object in the Linux archive carries its
//! `MODULE_FIRMWARE` lines in a `.modinfo` section as `<module>.firmware=<name>`, so the archive says what
//! the C can ask for. `firmware-chips.txt` in `ferrite-wifi-sys` narrows that to the chips the stick
//! supports, and each name is looked up in the host's firmware directory and copied under the same
//! relative path. A driver asks for the newest API it knows and falls back to older ones, so a name ending
//! in `-<n>.<ext>` resolves to the highest `n` the host has at or below it.

use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use object::read::archive::ArchiveFile;
use object::{Object as _, ObjectSection as _};
use snafu::{OptionExt, ResultExt, Snafu};

use crate::stubs::artifacts;

/// The package whose build script makes the Linux archive.
const SYS_CRATE: &str = "ferrite-wifi-sys";
/// The build the apps use, so the archive is the one the stick's image links.
const APP_CONFIG: &str = ".cargo/app.toml";
/// The section the kernel's `MODULE_INFO` writes into.
const MODINFO: &str = ".modinfo";
const FIRMWARE_TAG: &str = ".firmware=";
/// Compressed forms a distribution may ship a file in, and the tool that expands each.
const COMPRESSED: [(&str, &str); 2] = [("zst", "zstd"), ("xz", "xz")];
/// Largest API number tried below a requested one.
const MAX_API: u32 = 999;

#[derive(Debug, Snafu)]
pub enum FirmwareError {
    #[snafu(display("cannot {what} {}", path.display()))]
    Io { what: &'static str, path: PathBuf, source: io::Error },

    #[snafu(display("cannot run {program}"))]
    Spawn { program: String, source: io::Error },

    #[snafu(display("could not {what} ({status}):\n{detail}"))]
    Failed { what: String, status: String, detail: String },

    #[snafu(display("cannot read the Linux archive {}", path.display()))]
    Archive { path: PathBuf, source: object::Error },

    #[snafu(display("cannot find the Linux archive in the build of {SYS_CRATE}"))]
    NoArchive,
}

#[derive(Debug, Clone)]
pub struct FirmwareOptions {
    /// The workspace root, where the app build runs.
    pub root: PathBuf,
    /// The host's firmware directory.
    pub source: PathBuf,
    /// Where the files go: the stick's `\EFI\ferrite\firmware\`.
    pub dest: PathBuf,
    /// The chip list; its patterns select names from what the drivers request.
    pub chips: PathBuf,
    /// Stage every name the drivers request instead of the chip list's.
    pub all: bool,
    /// Resolve and report, copy nothing.
    pub list: bool,
}

/// Name patterns, `*` matching any run of characters, one per line with `#` comments.
#[derive(Debug, PartialEq, Eq)]
struct Chips(Vec<String>);

impl Chips {
    fn parse(text: &str) -> Self {
        let patterns = text
            .lines()
            .map(|line| line.split_once('#').map_or(line, |(pattern, _)| pattern).trim())
            .filter(|pattern| !pattern.is_empty())
            .map(str::to_owned);
        Self(patterns.collect())
    }

    fn allows(&self, name: &str) -> bool {
        self.0.iter().any(|pattern| glob(pattern, name))
    }
}

/// Whether `text` matches `pattern`, whose `*` stands for any run of characters.
fn glob(pattern: &str, text: &str) -> bool {
    let Some((head, rest)) = pattern.split_once('*') else { return pattern == text };
    let Some(text) = text.strip_prefix(head) else { return false };
    (0..=text.len()).filter(|at| text.is_char_boundary(*at)).any(|at| glob(rest, &text[at..]))
}

/// The firmware names in one `.modinfo` section: its NUL-separated `<module>.firmware=<name>` entries.
fn modinfo_names(section: &[u8]) -> impl Iterator<Item = &str> {
    section
        .split(|byte| *byte == 0)
        .filter_map(|entry| std::str::from_utf8(entry).ok())
        .filter_map(|entry| entry.split_once(FIRMWARE_TAG))
        .filter(|(module, _)| !module.is_empty() && !module.contains('='))
        .map(|(_, name)| name)
}

/// Every firmware name the objects of the archive in `bytes` list.
fn requested_names(bytes: &[u8]) -> Result<BTreeSet<String>, object::Error> {
    let mut names = BTreeSet::new();
    for member in ArchiveFile::parse(bytes)?.members() {
        let data = member?.data(bytes)?;
        let Ok(file) = object::File::parse(data) else { continue };
        if let Some(section) = file.section_by_name(MODINFO) {
            names.extend(modinfo_names(section.data()?).map(str::to_owned));
        }
    }
    Ok(names)
}

/// The names to try for `name`, best first: itself, then, for `<prefix>-<n>.<ext>`, each lower `n` and last
/// the unnumbered `<prefix>.<ext>`.
fn candidates(name: &str) -> Vec<String> {
    let mut found = vec![name.to_owned()];
    let (stem, ext) = name.rsplit_once('.').map_or((name, ""), |(stem, ext)| (stem, ext));
    let digits = stem.len() - stem.trim_end_matches(|c: char| c.is_ascii_digit()).len();
    let prefix = &stem[..stem.len() - digits];
    let number = stem[stem.len() - digits..].parse::<u32>().ok().filter(|n| (1..=MAX_API).contains(n));
    if let (Some(number), Some(bare)) = (number, prefix.strip_suffix('-')) {
        found.extend((1..number).rev().map(|n| format!("{prefix}{n}.{ext}")));
        found.push(format!("{bare}.{ext}"));
    }
    found
}

/// A firmware file on the host.
#[derive(Debug, PartialEq, Eq)]
struct Found {
    /// The name it is staged under: the file's, without a compression suffix.
    name: String,
    path: PathBuf,
}

/// The best file for `name` below `source`, plain or compressed.
fn resolve(source: &Path, name: &str) -> Option<Found> {
    candidates(name).into_iter().find_map(|candidate| {
        let plain = source.join(&candidate);
        let compressed = COMPRESSED.iter().map(|(ext, _)| source.join(format!("{candidate}.{ext}")));
        std::iter::once(plain).chain(compressed).find(|path| path.is_file()).map(|path| Found { name: candidate, path })
    })
}

/// The contents of `path`, expanded when its suffix names a compression.
fn read_expanded(path: &Path) -> Result<Vec<u8>, FirmwareError> {
    let ext = path.extension().and_then(|ext| ext.to_str());
    let Some((_, tool)) = COMPRESSED.iter().find(|(known, _)| Some(*known) == ext) else {
        return fs::read(path).context(IoSnafu { what: "read", path });
    };
    let mut command = Command::new(tool);
    command.arg("-dc").arg(path);
    let output = command.output().context(SpawnSnafu { program: *tool })?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return FailedSnafu { what: format!("expand {}", path.display()), status: output.status.to_string(), detail }
            .fail();
    }
    Ok(output.stdout)
}

/// `bytes` as MiB with one decimal.
struct Size(u64);

impl fmt::Display for Size {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{} MiB", self.0 >> 20, ((self.0 & 0xf_ffff) * 10) >> 20)
    }
}

/// Build the apps' Linux archive (a no-op once the app is built) and read its path from cargo's messages.
fn archive_path(root: &Path) -> Result<PathBuf, FirmwareError> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut command = Command::new(cargo);
    command.current_dir(root).args(["build", "--message-format=json-render-diagnostics"]);
    command.args(["--config", APP_CONFIG, "-p", SYS_CRATE]);
    let output = command.output().context(SpawnSnafu { program: "cargo" })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let lines: Vec<&str> = stderr.lines().collect();
        let detail = lines[lines.len().saturating_sub(20)..].join("\n");
        return FailedSnafu { what: format!("build {SYS_CRATE}"), status: output.status.to_string(), detail }.fail();
    }
    artifacts::archive_path(&String::from_utf8_lossy(&output.stdout), SYS_CRATE).context(NoArchiveSnafu)
}

/// Stage the files; the summary is printed and the problems are warnings, since a card whose files are
/// absent from this host is not a reason to stop staging the others.
pub fn run(options: &FirmwareOptions) -> Result<(), FirmwareError> {
    let archive = archive_path(&options.root)?;
    let bytes = fs::read(&archive).context(IoSnafu { what: "read", path: &archive })?;
    let requested = requested_names(&bytes).context(ArchiveSnafu { path: &archive })?;

    let chips_text = fs::read_to_string(&options.chips).context(IoSnafu { what: "read", path: &options.chips })?;
    let chips = Chips::parse(&chips_text);
    let wanted: Vec<&String> = requested.iter().filter(|name| options.all || chips.allows(name)).collect();
    println!(
        "firmware: the compiled drivers request {} names, {} selected{}",
        requested.len(),
        wanted.len(),
        if options.all { " (all)" } else { "" }
    );

    let (mut staged, mut added, mut total, mut missing) = (BTreeSet::new(), 0_u64, 0_u64, 0_usize);
    for name in wanted {
        let Some(found) = resolve(&options.source, name) else {
            eprintln!("warning: {name} is not in {}", options.source.display());
            missing += 1;
            continue;
        };
        if found.name != *name {
            eprintln!(
                "warning: {name} is not in {}; using {} (the driver may refuse an older API)",
                options.source.display(),
                found.name
            );
        }
        if !staged.insert(found.name.clone()) {
            continue;
        }
        let bytes = read_expanded(&found.path)?;
        let target = options.dest.join(&found.name);
        let unchanged = fs::metadata(&target).is_ok_and(|meta| meta.len() == bytes.len() as u64);
        total += bytes.len() as u64;
        if !unchanged {
            added += bytes.len() as u64;
        }
        println!(
            "firmware: {} {} ({})",
            if unchanged { "kept" } else { "staged" },
            found.name,
            Size(bytes.len() as u64)
        );
        if !options.list && !unchanged {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).context(IoSnafu { what: "create", path: parent })?;
            }
            fs::write(&target, &bytes).context(IoSnafu { what: "write", path: &target })?;
        }
    }
    println!(
        "firmware: {} files, {} in {}, {} added by this run, {missing} names absent from the host",
        staged.len(),
        Size(total),
        options.dest.display(),
        Size(added),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use assert2::check;
    use rstest::{fixture, rstest};

    use super::*;

    #[rstest]
    #[case::exact("iwlwifi-cc-a0-*", "iwlwifi-cc-a0-77.ucode", true)]
    #[case::other_family("iwlwifi-cc-a0-*", "iwlwifi-Qu-b0-hr-b0-77.ucode", false)]
    #[case::literal("regulatory.db", "regulatory.db", true)]
    #[case::literal_is_not_a_prefix("regulatory.db", "regulatory.db.p7s", false)]
    #[case::middle("mediatek/*MT7922*", "mediatek/WIFI_RAM_CODE_MT7922_1.bin", true)]
    #[case::directory_matters("rtw89/*", "mediatek/x.bin", false)]
    fn globs_match_names(#[case] pattern: &str, #[case] name: &str, #[case] want: bool) {
        check!(glob(pattern, name) == want);
    }

    #[test]
    fn chip_lists_skip_comments_and_blank_lines() {
        let chips = Chips::parse("# AX200\niwlwifi-cc-a0-*  # trailing\n\nregulatory.db\n");
        check!(chips == Chips(vec!["iwlwifi-cc-a0-*".into(), "regulatory.db".into()]));
        check!(chips.allows("regulatory.db"));
        check!(!chips.allows("iwlwifi-9000-pu-b0-jf-b0-46.ucode"));
    }

    #[rstest]
    #[case::kernel_format(b"iwlwifi.firmware=iwlwifi-cc-a0-77.ucode\0iwlwifi.license=GPL\0", &["iwlwifi-cc-a0-77.ucode"])]
    #[case::several(b"a.firmware=x.bin\0b.firmware=y/z.bin\0", &["x.bin", "y/z.bin"])]
    #[case::other_tags(b"iwlwifi.description=firmware=nope\0iwlwifi.parm=a:b\0", &[])]
    fn modinfo_entries_name_firmware(#[case] section: &[u8], #[case] want: &[&str]) {
        check!(modinfo_names(section).collect::<Vec<_>>() == want);
    }

    #[rstest]
    #[case::numbered("iwlwifi-ty-a0-gf-a0-3.ucode", &["iwlwifi-ty-a0-gf-a0-3.ucode", "iwlwifi-ty-a0-gf-a0-2.ucode", "iwlwifi-ty-a0-gf-a0-1.ucode", "iwlwifi-ty-a0-gf-a0.ucode"])]
    #[case::rtw89("rtw89/rtw8852c_fw-2.bin", &["rtw89/rtw8852c_fw-2.bin", "rtw89/rtw8852c_fw-1.bin", "rtw89/rtw8852c_fw.bin"])]
    #[case::pnvm_has_no_api("iwlwifi-ty-a0-gf-a0.pnvm", &["iwlwifi-ty-a0-gf-a0.pnvm"])]
    #[case::underscore_number("mediatek/WIFI_RAM_CODE_MT7961_1.bin", &["mediatek/WIFI_RAM_CODE_MT7961_1.bin"])]
    fn older_apis_follow_the_requested_name(#[case] name: &str, #[case] want: &[&str]) {
        check!(candidates(name) == want);
    }

    /// A directory under the system temp dir that is removed when the test ends.
    struct Dir(PathBuf);

    impl Dir {
        fn file(&self, name: &str, bytes: &[u8]) {
            let path = self.0.join(name);
            fs::create_dir_all(path.parent().expect("a file has a parent")).expect("create the directory");
            fs::write(path, bytes).expect("write the file");
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[fixture]
    fn host() -> Dir {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("ferrite-firmware-test-{}-{n}", std::process::id()));
        fs::create_dir_all(&dir).expect("create the host directory");
        Dir(dir)
    }

    #[rstest]
    #[case::exact(&["a-9.ucode", "a-8.ucode"], "a-9.ucode", Some("a-9.ucode"))]
    #[case::falls_back_to_the_newest_older(&["a-8.ucode", "a-6.ucode"], "a-9.ucode", Some("a-8.ucode"))]
    #[case::never_newer(&["a-10.ucode"], "a-9.ucode", None)]
    #[case::compressed_is_found_under_its_plain_name(&["a-9.ucode.zst"], "a-9.ucode", Some("a-9.ucode"))]
    #[case::absent(&[], "a-9.ucode", None)]
    fn the_host_file_is_the_best_at_or_below_the_name(
        host: Dir,
        #[case] files: &[&str],
        #[case] name: &str,
        #[case] want: Option<&str>,
    ) {
        for file in files {
            host.file(file, b"x");
        }
        check!(resolve(&host.0, name).map(|found| found.name) == want.map(str::to_owned));
    }

    #[rstest]
    fn plain_files_are_read_as_they_are(host: Dir) {
        host.file("a.bin", b"\x01\x02");
        check!(read_expanded(&host.0.join("a.bin")).ok() == Some(vec![1, 2]));
    }

    #[rstest]
    fn an_object_in_an_archive_yields_its_firmware_names() {
        let mut object = object::write::Object::new(
            object::BinaryFormat::Elf,
            object::Architecture::X86_64,
            object::Endianness::Little,
        );
        let section = object.add_section(vec![], MODINFO.as_bytes().to_vec(), object::SectionKind::Note);
        object.append_section_data(section, b"mod.firmware=x/y.bin\0mod.license=GPL\0", 1);
        let elf = object.write().expect("write the object");
        let mut archive = b"!<arch>\n".to_vec();
        archive.extend_from_slice(
            format!("{:<16}{:<12}{:<6}{:<6}{:<8}{:<10}`\n", "m.o/", 0, 0, 0, 644, elf.len()).as_bytes(),
        );
        archive.extend_from_slice(&elf);
        if elf.len() % 2 == 1 {
            archive.push(b'\n');
        }
        check!(requested_names(&archive).ok() == Some(BTreeSet::from(["x/y.bin".to_owned()])));
    }

    #[rstest]
    #[case::mib(3 << 20, "3.0 MiB")]
    #[case::fraction((5 << 20) + (1 << 19), "5.5 MiB")]
    fn sizes_print_in_mib(#[case] bytes: u64, #[case] want: &str) {
        check!(Size(bytes).to_string() == want);
    }
}
