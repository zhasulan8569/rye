use std::borrow::Cow;
use std::convert::Infallible;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::{fmt, fs};

use anyhow::{anyhow, bail, Context, Error};
use dialoguer::theme::{ColorfulTheme, Theme};
use once_cell::sync::Lazy;
use pep508_rs::{Requirement, VersionOrUrl};
use regex::{Captures, Regex};
use sha2::{Digest, Sha256};

static ENV_VAR_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\$\{([A-Z0-9_]+)\}").unwrap());

#[cfg(unix)]
pub use std::os::unix::fs::{symlink as symlink_file, symlink as symlink_dir};
#[cfg(windows)]
pub use std::os::windows::fs::symlink_file;

use crate::config::Config;
use crate::consts::VENV_BIN;

/// Returns the preferred theme for dialoguer
pub fn tui_theme() -> &'static dyn Theme {
    static THEME: Lazy<ColorfulTheme> = Lazy::new(ColorfulTheme::default);
    Lazy::force(&THEME) as &dyn Theme
}

#[cfg(windows)]
pub(crate) mod windows;

#[cfg(unix)]
pub(crate) mod unix;

pub(crate) mod ruff;
pub(crate) mod toml;

pub trait IoPathContext {
    type Out;

    /// Adds path information to an error.
    fn path_context<P: AsRef<Path>, D: fmt::Display>(self, p: P, msg: D) -> Self::Out;
}

impl<T, E: std::error::Error + Send + Sync + 'static> IoPathContext for Result<T, E> {
    type Out = Result<T, Error>;

    fn path_context<P: AsRef<Path>, D: fmt::Display>(self, p: P, msg: D) -> Self::Out {
        self.with_context(|| format!("{} (at '{}')", msg, p.as_ref().display()))
    }
}

#[cfg(windows)]
pub fn symlink_dir<P, Q>(original: P, link: Q) -> Result<(), std::io::Error>
where
    P: AsRef<Path>,
    Q: AsRef<Path>,
{
    if let Err(err) = std::os::windows::fs::symlink_dir(original.as_ref(), link.as_ref()) {
        if err.raw_os_error() == Some(1314) {
            junction::create(original.as_ref(), link.as_ref())
        } else {
            Err(err)
        }
    } else {
        Ok(())
    }
}

/// Given the path to a folder this adds or removes a cloud sync flag
/// on the folder.  Adding flags will return an error if it does not work,
/// removing flags is silently ignored.
///
/// Today this only supports dropbox and apple icloud.
pub fn mark_path_sync_ignore(venv: &Path, mark_ignore: bool) -> Result<(), Error> {
    #[cfg(unix)]
    {
        #[cfg(target_os = "macos")]
        const ATTRS: &[&str] = &["com.dropbox.ignored", "com.apple.fileprovider.ignore#P"];

        // xattrs need a namespace on Linux, and try this solution on every non-mac cfg(unix) system.
        #[cfg(not(target_os = "macos"))]
        const ATTRS: &[&str] = &["user.com.dropbox.ignored"];

        for flag in ATTRS {
            if mark_ignore {
                xattr::set(venv, flag, b"1")
                    .path_context(venv, "failed to write extended attribute")?;
            } else {
                xattr::remove(venv, flag).ok();
            }
        }
    }

    #[cfg(windows)]
    {
        let mut stream_path = venv.as_os_str().to_os_string();
        stream_path.push(":com.dropbox.ignored");
        if mark_ignore {
            fs::write(&stream_path, b"1").path_context(&stream_path, "failed to write stream")?;
        } else {
            fs::remove_file(stream_path).ok();
        }
    }

    Ok(())
}

#[derive(Debug)]
pub struct QuietExit(pub i32);

impl std::error::Error for QuietExit {}

impl fmt::Display for QuietExit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "exit with {}", self.0)
    }
}

/// Controls the fetch output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum CommandOutput {
    /// Regular output
    #[default]
    Normal,
    /// Extra verbose output
    Verbose,
    /// No output
    Quiet,
}

impl CommandOutput {
    /// Returns the preferred command output for those flags.
    pub fn from_quiet_and_verbose(quiet: bool, verbose: bool) -> CommandOutput {
        if quiet {
            CommandOutput::Quiet
        } else if verbose {
            CommandOutput::Verbose
        } else {
            CommandOutput::Normal
        }
    }

    pub fn quieter(self) -> CommandOutput {
        match self {
            CommandOutput::Normal => CommandOutput::Quiet,
            CommandOutput::Verbose => CommandOutput::Normal,
            CommandOutput::Quiet => CommandOutput::Quiet,
        }
    }
}

/// Given a path checks if that path is executable.
///
/// On windows this function is a bit magical because if `foo` is passed
/// as path this will return true if `foo.exe` exists.
pub fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::prelude::MetadataExt;
        path.metadata().map_or(false, |x| x.mode() & 0o111 != 0)
    }
    #[cfg(windows)]
    {
        ["exe", "bat", "cmd"]
            .iter()
            .any(|x| path.with_extension(x).is_file())
    }
}

/// Given a path to a script, returns a human readable short name of the script
pub fn get_short_executable_name(path: &Path) -> String {
    #[cfg(unix)]
    {
        path.file_name().unwrap().to_string_lossy().to_string()
    }
    #[cfg(windows)]
    {
        let short_name = path.file_name().unwrap().to_string_lossy().to_lowercase();
        for ext in [".exe", ".bat", ".cmd"] {
            if let Some(base_name) = short_name.strip_suffix(ext) {
                return base_name.into();
            }
        }
        short_name
    }
}

/// Formats a Python requirement.
pub fn format_requirement(req: &Requirement) -> impl fmt::Display + '_ {
    struct Helper<'x>(&'x Requirement);

    impl<'x> fmt::Display for Helper<'x> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "{}", self.0.name)?;
            if let Some(extras) = &self.0.extras {
                write!(f, "[{}]", extras.join(","))?;
            }
            if let Some(version_or_url) = &self.0.version_or_url {
                match version_or_url {
                    VersionOrUrl::VersionSpecifier(version_specifier) => {
                        let version_specifier: Vec<String> =
                            version_specifier.iter().map(ToString::to_string).collect();
                        write!(f, "{}", version_specifier.join(", "))?;
                    }
                    VersionOrUrl::Url(url) => {
                        // retain `{` and `}` for interpolation in URLs
                        write!(
                            f,
                            " @ {}",
                            url.to_string().replace("%7B", "{").replace("%7D", "}")
                        )?;
                    }
                }
            }
            if let Some(marker) = &self.0.marker {
                write!(f, " ; {}", marker)?;
            }
            Ok(())
        }
    }

    Helper(req)
}

/// Helper to expand envvars
pub fn expand_env_vars<F>(string: &str, mut f: F) -> Cow<'_, str>
where
    F: for<'a> FnMut(&'a str) -> Option<String>,
{
    ENV_VAR_RE.replace_all(string, |m: &Captures| f(&m[1]).unwrap_or_default())
}

#[derive(Copy, Clone, Debug)]
enum ArchiveFormat {
    TarGz,
    TarBz2,
    TarZstd,
    Zip,
}

impl ArchiveFormat {
    pub fn peek(bytes: &[u8]) -> Option<ArchiveFormat> {
        let mut buf = [0u8; 1];
        if zstd::stream::read::Decoder::with_buffer(bytes)
            .map_or(false, |x| x.single_frame().read(&mut buf).is_ok())
        {
            Some(ArchiveFormat::TarZstd)
        } else if flate2::bufread::GzDecoder::new(bytes).header().is_some() {
            Some(ArchiveFormat::TarGz)
        } else if bzip2::bufread::BzDecoder::new(bytes).read(&mut buf).is_ok() {
            Some(ArchiveFormat::TarBz2)
        } else if zip::read::ZipArchive::new(Cursor::new(bytes)).is_ok() {
            Some(ArchiveFormat::Zip)
        } else {
            None
        }
    }

    pub fn make_decoder<'a>(self, bytes: &'a [u8]) -> Result<Box<dyn Read + 'a>, Error> {
        Ok(match self {
            ArchiveFormat::TarGz => Box::new(flate2::bufread::GzDecoder::new(bytes)) as Box<_>,
            ArchiveFormat::TarBz2 => Box::new(bzip2::bufread::BzDecoder::new(bytes)) as Box<_>,
            ArchiveFormat::TarZstd => {
                Box::new(zstd::stream::read::Decoder::with_buffer(bytes)?) as Box<_>
            }
            ArchiveFormat::Zip => return Err(anyhow!("zip cannot be decoded with read")),
        })
    }
}

/// Unpacks a tarball or zip archive.
///
/// Today this assumes that the tarball is zstd compressed which happens
/// to be what the indygreg python builds use.
pub fn unpack_archive(contents: &[u8], dst: &Path, strip_components: usize) -> Result<(), Error> {
    let format = ArchiveFormat::peek(contents).ok_or_else(|| anyhow!("unknown archive"))?;

    if matches!(format, ArchiveFormat::Zip) {
        let mut archive = zip::read::ZipArchive::new(Cursor::new(contents))?;
        for i in 0..archive.len() {
            let mut file = archive.by_index(i)?;
            let name = file
                .enclosed_name()
                .ok_or_else(|| anyhow!("Invalid file path in zip"))?;
            let mut components = name.components();
            for _ in 0..strip_components {
                components.next();
            }
            let path = dst.join(components.as_path());
            if path != Path::new("") && path.strip_prefix(dst).is_ok() {
                if file.name().ends_with('/') {
                    fs::create_dir_all(&path).path_context(&path, "failed to create directory")?;
                } else {
                    if let Some(p) = path.parent() {
                        if !p.exists() {
                            fs::create_dir_all(p).path_context(p, "failed to create directory")?;
                        }
                    }
                    std::io::copy(
                        &mut file,
                        &mut fs::File::create(&path)
                            .path_context(&path, "failed to create file")?,
                    )?;
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    if let Some(mode) = file.unix_mode() {
                        fs::set_permissions(&path, fs::Permissions::from_mode(mode))
                            .path_context(&path, "failed to set permissions")?;
                    }
                }
            }
        }
    } else {
        let mut archive = tar::Archive::new(format.make_decoder(contents)?);
        for entry in archive.entries()? {
            let mut entry = entry?;
            let name = entry.path()?;
            let mut components = name.components();
            for _ in 0..strip_components {
                components.next();
            }
            let path = dst.join(components.as_path());

            // only unpack if it's save to do so
            if path != Path::new("") && path.strip_prefix(dst).is_ok() {
                if let Some(dir) = path.parent() {
                    fs::create_dir_all(dir).ok();
                }
                entry.unpack(&path)?;
            }
        }
    }

    Ok(())
}

/// Spawns a command exec style.
pub fn exec_spawn(cmd: &mut Command) -> Result<Infallible, Error> {
    // this is technically only necessary on windows
    crate::disable_ctrlc_handler();

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = cmd.exec();
        Err(err.into())
    }
    #[cfg(windows)]
    {
        cmd.stdin(Stdio::inherit());
        let status = cmd.status()?;
        std::process::exit(status.code().unwrap())
    }
}

/// Attaches standard proxy environment variables to a process.
pub fn set_proxy_variables(cmd: &mut Command) {
    let config = Config::current();
    if let Some(proxy) = config.https_proxy_url() {
        cmd.env("https_proxy", proxy);
    }
    if let Some(proxy) = config.http_proxy_url() {
        cmd.env("http_proxy", proxy);
    }
}

/// Given a virtualenv returns the path to the python interpreter.
pub fn get_venv_python_bin(venv_path: &Path) -> PathBuf {
    let mut py = venv_path.join(VENV_BIN);
    py.push("python");
    #[cfg(windows)]
    {
        py.set_extension("exe");
    }
    py
}

pub fn is_inside_git_work_tree(dir: &PathBuf) -> bool {
    Command::new("git")
        .arg("rev-parse")
        .arg("--is-inside-work-tree")
        .current_dir(dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// Returns a success exit status.
pub fn success_status() -> ExitStatus {
    #[cfg(windows)]
    {
        use std::os::windows::process::ExitStatusExt;
        ExitStatus::from_raw(0)
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        ExitStatus::from_raw(0)
    }
}

/// Takes a bytes slice and compares it to a given string checksum.
pub fn check_checksum(content: &[u8], checksum: &str) -> Result<(), Error> {
    let mut hasher = Sha256::new();
    hasher.update(content);
    let digest = hasher.finalize();
    let digest = hex::encode(digest);
    if !digest.eq_ignore_ascii_case(checksum) {
        bail!("hash mismatch: expected {} got {}", checksum, digest);
    }
    Ok(())
}

pub fn escape_string(s: String) -> String {
    s.trim().replace(['\\', '"'], "")
}

pub fn copy_dir<T: AsRef<Path>>(from: T, to: T, options: &CopyDirOptions) -> Result<(), Error> {
    let from = from.as_ref();
    let to = to.as_ref();

    if from.is_dir() {
        for entry in fs::read_dir(from)
            .path_context(from, "failed to enumerate directory")?
            .filter_map(|e| e.ok())
        {
            let entry_path = entry.path();
            if options.exclude.iter().any(|dir| *dir == entry_path) {
                continue;
            }

            let destination = to.join(entry.file_name());
            if entry.file_type()?.is_dir() {
                fs::create_dir_all(&destination)
                    .path_context(&destination, "failed to create directory")?;
                copy_dir(entry.path(), destination, options)?;
            } else {
                fs::copy(entry.path(), &destination)
                    .path_context(entry.path(), "failed to copy file")?;
            }
        }
    }
    Ok(())
}

pub struct CopyDirOptions {
    /// Exclude paths
    pub exclude: Vec<PathBuf>,
}

/// Update the cloud synchronization marker for the given path
/// based on the config flag.
pub fn update_venv_sync_marker(output: CommandOutput, venv_path: &Path) {
    if let Err(err) = mark_path_sync_ignore(venv_path, Config::current().venv_mark_sync_ignore()) {
        if output != CommandOutput::Quiet && Config::current().venv_mark_sync_ignore() {
            warn!(
                "unable to mark virtualenv {} ignored for cloud sync: {}",
                venv_path.display(),
                err
            );
        }
    }
}

#[test]
fn test_quiet_exit_display() {
    let quiet_exit = QuietExit(0);
    assert_eq!("exit with 0", format!("{}", quiet_exit));
}

#[cfg(test)]
mod test_format_requirement {
    use super::{format_requirement, Requirement};

    #[test]
    fn test_format_requirement_simple() {
        let req: Requirement = "foo>=1.0.0".parse().unwrap();
        assert_eq!("foo>=1.0.0", format_requirement(&req).to_string());
    }

    #[test]
    fn test_format_requirement_complex() {
        let req: Requirement = "foo[extra1,extra2]>=1.0.0,<2.0.0; python_version<'3.8'"
            .parse()
            .unwrap();
        assert_eq!(
            "foo[extra1,extra2]>=1.0.0, <2.0.0 ; python_version < '3.8'",
            format_requirement(&req).to_string()
        );
    }
    #[test]
    fn test_format_requirement_file_path() {
        // this support is just for generating dependencies.  Parsing such requirements
        // is only partially supported as expansion has to happen before parsing.
        let req: Requirement = "foo @ file:///${PROJECT_ROOT}/foo".parse().unwrap();
        assert_eq!(
            format_requirement(&req).to_string(),
            "foo @ file:///${PROJECT_ROOT}/foo"
        );
    }
}

#[cfg(test)]
mod test_command_output {
    use super::CommandOutput;

    #[test]
    fn test_command_output_defaults() {
        assert_eq!(CommandOutput::Normal, CommandOutput::default());
    }

    #[test]
    fn test_command_output_from_quiet_and_verbose() {
        let quiet = true;
        let verbose = true;

        assert_eq!(
            CommandOutput::Quiet,
            CommandOutput::from_quiet_and_verbose(quiet, false)
        );
        assert_eq!(
            CommandOutput::Verbose,
            CommandOutput::from_quiet_and_verbose(false, verbose)
        );
        assert_eq!(
            CommandOutput::Normal,
            CommandOutput::from_quiet_and_verbose(false, false)
        );
        assert_eq!(
            CommandOutput::Quiet,
            CommandOutput::from_quiet_and_verbose(quiet, verbose)
        ); // Quiet takes precedence over verbose
    }
}

#[cfg(test)]
mod test_expand_env_vars {
    use super::expand_env_vars;

    #[test]
    fn test_expand_env_vars_no_expansion() {
        let input = "This string has no env vars";
        let output = expand_env_vars(input, |_| None);
        assert_eq!(input, output);
    }

    #[test]
    fn test_expand_env_vars_with_expansion() {
        let input = "This string has an env var: ${EXAMPLE_VAR}";
        let output = expand_env_vars(input, |var| {
            if var == "EXAMPLE_VAR" {
                Some("Example value".to_string())
            } else {
                None
            }
        });
        assert_eq!("This string has an env var: Example value", output);
    }
}

#[cfg(test)]
mod test_is_inside_git_work_tree {
    use std::path::PathBuf;

    use super::is_inside_git_work_tree;
    #[test]
    fn test_is_inside_git_work_tree_true() {
        assert!(is_inside_git_work_tree(&PathBuf::from(".")));
    }

    #[test]
    fn test_is_inside_git_work_tree_false() {
        assert!(!is_inside_git_work_tree(&PathBuf::from("/")));
    }
}
