use std::env::args_os;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{stdin, IsTerminal, Write};
use std::ops::Deref;
use std::os::unix::prelude::OsStrExt;
use std::path::PathBuf;
use std::str::FromStr;
use anyhow::{anyhow, Context};
use coarsetime::Duration;
use git2::{Repository, Status, StatusOptions};
use log::debug;
use termcolor::{Buffer, BufferWriter, Color, ColorChoice, ColorSpec, WriteColor};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const RUST_LOG_FILTER_ENVVAR: &str = "NUPROMPT_RUST_LOG";
const NO_GIT_ENVVAR: &str = "NUPROMPT_NO_GIT";
const PWD_ENVVAR: &str = "PWD";
const HOME_ENVVAR: &str = "HOME";

fn main() -> Result<(), anyhow::Error> {
    // we output some debug logs which can be turned on if needed.
    env_logger::init_from_env(env_logger::Env::default().filter(RUST_LOG_FILTER_ENVVAR));

    // we handle args in a very basic way since this is not intended to be an interactive or iterative
    // CLI UX.
    let n_args = args_os().len();
    let subcommand = args_os().nth(1);
    let pid_arg = args_os().nth(2);
    let extra_arg = args_os().nth(3);
    match subcommand {
        Some(p) if p.eq("bash") && n_args == 2 => {
            println!("PS0='$(nuprompt ps0 $$)'\nPROMPT_COMMAND='eval $(nuprompt ps1 $$ $?)'");
            Ok(())
        },
        Some(p) if p.eq("ps0") && n_args == 3 => ps0(pid_arg.unwrap().deref()).context("nuprompt ps0"),
        Some(p) if p.eq("ps1") && n_args == 4 => ps1(pid_arg.unwrap().deref(), extra_arg.unwrap().deref()).context("nuprompt ps1"),
        _ => Err(anyhow!("nuprompt {} must be executed as either 'nuprompt ps0 <pid>' or 'nuprompt ps1 <pid> <exit code>'", VERSION))
    }
}

fn ps0(raw_pid: &OsStr) -> Result<(), anyhow::Error> {
    write_start_time(raw_pid)?;
    Ok(())
}


fn ps1(raw_pid: &OsStr, exit_code: &OsStr) -> Result<(), anyhow::Error> {

    // expect a status code as the first positional arg
    let exit_code = Some(exit_code)
        .filter(|a| !a.is_empty() && !a.eq(&OsStr::new("0")));

    // cwd comes from libc or from the env var
    let possible_cwd = std::env::current_dir().ok()
        .or_else(|| std::env::var_os(PWD_ENVVAR).map(PathBuf::from));

    // try and read the start time of the previous command from a file on the system
    let elapsed: Option<Duration> = read_elapsed_time(raw_pid)
        .map_or_else(|e| {
            debug!("error reading elapsed time from pid file: {}", e);
            None
        }, Some);

    // try and parse git status if were in a repo
    let start_time = coarsetime::Instant::now();
    let (cwd, git_bits): (PathBuf, Option<GitBits>) = match possible_cwd {
        Some(p) => {
            match std::env::var_os(NO_GIT_ENVVAR) {
                Some(_) => (shorted_path_buf(p), None),
                None => {
                    debug!("looking for git repo from working directory: {:?}", p);
                    let ceil: &[PathBuf] = &[];
                    match Repository::open_ext(&p, git2::RepositoryOpenFlags::empty(), ceil) {
                        Ok(r) => (shorted_path_buf(p), Some(GitBits::from_repo(&r)?)),
                        Err(e) => {
                            debug!("could not open repository: {:?}", e);
                            (shorted_path_buf(p), None)
                        }
                    }
                }
            }
        },
        None => (PathBuf::new(), None),
    };
    debug!("scanned for git repo in {:?}", start_time.elapsed());

    // the username or uid:guid
    let username = users::get_current_username()
        .unwrap_or_else(|| OsString::from(format!("{}:{}", users::get_current_uid(), users::get_current_gid())));
    debug!("found user: {:?}", username);

    // prepare the buffered writer
    let buf_writer = BufferWriter::stdout(if stdin().is_terminal() { ColorChoice::Auto } else { ColorChoice::Never});
    let mut buffer = buf_writer.buffer();
    buffer.write_all(b"PS1='[")?;
    if let Some(exit_code) = exit_code {
        set_color_wrapped(&mut buffer, ColorSpec::new().set_fg(Some(Color::Red)))?;
        buffer.write_all(exit_code.as_bytes())?;
        buffer.write_all(b" ")?;
    }
    if let Some(elapsed) = elapsed {
        set_color_wrapped(&mut buffer, ColorSpec::new().set_fg(Some(Color::Cyan)))?;
        write!(buffer, "{:.2}s ", elapsed.as_f64())?;
    }
    set_color_wrapped(&mut buffer, ColorSpec::new().set_fg(Some(Color::Cyan)).set_bold(true).set_intense(true))?;
    buffer.write_all(username.as_bytes())?;
    buffer.write_all(b" ")?;
    if let Some(git_bits) = git_bits {
        set_color_wrapped(&mut buffer, ColorSpec::new().set_fg(Some(Color::Yellow)).set_intense(true))?;
        write_with_escaped_quote(git_bits.head_ref.as_bytes(), &mut buffer)?;
        set_color_wrapped(&mut buffer, &ColorSpec::default())?;
        git_bits.write_elements(&mut buffer)?;
        buffer.write_all(b" ")?;
    }
    set_color_wrapped(&mut buffer, &ColorSpec::default())?;
    write_with_escaped_quote(cwd.as_os_str().as_bytes(), &mut buffer)?;
    buffer.write_all(b" \xE2\x9F\xAB '")?;
    buf_writer.print(&buffer)?;
    Ok(())
}

/// write some raw bytes but make sure we escape any single quotes.
fn write_with_escaped_quote(input: &[u8], mut w: impl Write) -> Result<(), std::io::Error> {
    for (i, x) in input.split(|u| *u == b'\'').enumerate() {
        if i > 0 {
            w.write_all(b"'\\''")?;
        }
        w.write_all(x)?;
    }
    Ok(())
}

/// Emit a color change wrapped in readline's `\[` `\]` non-printing markers so
/// bash computes the prompt width correctly; without them readline counts the
/// escape bytes as visible characters and corrupts line-wrapping and history
/// recall. termcolor emits bare SGR sequences and knows nothing about readline,
/// so the wrapping is the caller's responsibility. No-op when color is disabled,
/// to avoid emitting empty `\[\]` pairs.
fn set_color_wrapped(buffer: &mut Buffer, spec: &ColorSpec) -> Result<(), std::io::Error> {
    if !buffer.supports_color() {
        return Ok(());
    }
    buffer.write_all(b"\\[")?;
    buffer.set_color(spec)?;
    buffer.write_all(b"\\]")?;
    Ok(())
}

fn prev_start_file_path(raw_pid: &OsStr) -> PathBuf {
    std::env::temp_dir().join(format!("NUPROMPT_{}_prev_start", raw_pid.to_string_lossy()))
}

fn read_elapsed_time(raw_pid: &OsStr) -> Result<Duration, anyhow::Error> {
    let tf = prev_start_file_path(raw_pid);
    let contents = fs::read(&tf)?;
    fs::remove_file(&tf)?;
    let ticks = u64::from_be_bytes(contents[..8].try_into()?);
    let now_ticks = coarsetime::Instant::now().as_ticks();
    debug!("read start time from pid file: {} now={}", ticks, now_ticks);
    Ok(Duration::from_ticks(now_ticks - ticks))
}

fn write_start_time(raw_pid: &OsStr) -> Result<(), anyhow::Error>{
    let tf = prev_start_file_path(raw_pid);
    let now_ticks = coarsetime::Instant::now().as_ticks();
    fs::write(&tf, now_ticks.to_be_bytes())?;
    debug!("wrote start time to pid file: now={}", now_ticks);
    Ok(())
}

/// GitBits holds the result of scanning the git repo for current status.
struct GitBits {
    head_ref: String,
    index_modified: bool,
    worktree_modified: bool,
    untracked_files: bool,
}

impl GitBits {

    fn from_repo(r: &Repository) -> Result<GitBits, anyhow::Error> {
        let short_ref = r.head()
            .map(|h| h.shorthand().unwrap().to_owned())
            .unwrap_or_else(|e| {
                debug!("error reading head ref: {}", e);
               String::from("NO HEAD")
            });
        let mut gb = GitBits{
            head_ref: short_ref,
            index_modified: false,
            worktree_modified: false,
            untracked_files: false,
        };
        let statuses = r.statuses(Some(StatusOptions::new()
            .include_ignored(false)
            .include_untracked(true)
            .exclude_submodules(true)
            .include_unreadable(false)))?;
        let wt_modified: Status = Status::WT_MODIFIED | Status::WT_DELETED | Status::WT_TYPECHANGE | Status::WT_RENAMED;
        let index_modified: Status = Status::INDEX_NEW | Status::INDEX_MODIFIED | Status::INDEX_TYPECHANGE | Status::INDEX_RENAMED | Status::INDEX_DELETED;
        for x in statuses.iter() {
            debug!("git status {:?}: {:?}", x.path(), x.status());
            let st = x.status();
            if st.intersects(wt_modified) {
                gb.worktree_modified = true;
            }
            if st.intersects(index_modified) {
                gb.index_modified = true;
            }
            if st.contains(Status::WT_NEW) {
                gb.untracked_files = true;
            }
        }
        Ok(gb)
    }

    fn write_elements(&self, mut w: impl Write) -> Result<(), std::io::Error> {
        if self.index_modified || self.worktree_modified || self.untracked_files {
            w.write_all(b":")?;
            if self.index_modified {
                w.write_all(b"s")?;
            }
            if self.worktree_modified {
                w.write_all(b"d")?;
            }
            if self.untracked_files {
                w.write_all(b"u")?;
            }
        }
        Ok(())
    }

}


/// Replace a prefix of $HOME with ~ in the given path.
fn shorted_path_buf(input: PathBuf) -> PathBuf {
    shorted_path_buf_with_home(input, std::env::var(HOME_ENVVAR).ok())
}

/// Replace a prefix of the given home directory with ~ in the given path. Split from
/// `shorted_path_buf` so the logic can be tested without touching the HOME env var.
fn shorted_path_buf_with_home(input: PathBuf, home: Option<String>) -> PathBuf {
    match home.map(PathBuf::from) {
        Some(h) if input.starts_with(&h) => PathBuf::from_str("~").unwrap().join(input.strip_prefix(h).unwrap()),
        _ => input,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn escaped(input: &[u8]) -> String {
        let mut buf: Vec<u8> = Vec::new();
        write_with_escaped_quote(input, &mut buf).unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn test_write_with_escaped_quote() {
        assert_eq!(escaped(b""), "");
        assert_eq!(escaped(b"no quotes here"), "no quotes here");
        assert_eq!(escaped(b"it's"), "it'\\''s");
        assert_eq!(escaped(b"a'b'c"), "a'\\''b'\\''c");
        assert_eq!(escaped(b"'lead"), "'\\''lead");
        assert_eq!(escaped(b"trail'"), "trail'\\''");
        assert_eq!(escaped(b"'"), "'\\''");
    }

    fn shorted(input: &str, home: Option<&str>) -> String {
        shorted_path_buf_with_home(PathBuf::from(input), home.map(String::from))
            .to_string_lossy()
            .into_owned()
    }

    #[test]
    fn test_shorted_path_buf_with_home() {
        // prefix replaced with ~
        assert_eq!(shorted("/home/ben/projects", Some("/home/ben")), "~/projects");
        // exact home: prefix stripped leaves an empty remainder, so ~ joins with "" -> "~/"
        assert_eq!(shorted("/home/ben", Some("/home/ben")), "~/");
        // non-matching path left unchanged
        assert_eq!(shorted("/etc/passwd", Some("/home/ben")), "/etc/passwd");
        // no home set leaves path unchanged
        assert_eq!(shorted("/home/ben/x", None), "/home/ben/x");
    }

    fn elements(index_modified: bool, worktree_modified: bool, untracked_files: bool) -> String {
        let gb = GitBits {
            head_ref: String::from("main"),
            index_modified,
            worktree_modified,
            untracked_files,
        };
        let mut buf: Vec<u8> = Vec::new();
        gb.write_elements(&mut buf).unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn test_git_bits_write_elements() {
        // nothing written when all false
        assert_eq!(elements(false, false, false), "");
        // single flags
        assert_eq!(elements(true, false, false), ":s");
        assert_eq!(elements(false, true, false), ":d");
        assert_eq!(elements(false, false, true), ":u");
        // pairs (order is always s, d, u)
        assert_eq!(elements(true, true, false), ":sd");
        assert_eq!(elements(true, false, true), ":su");
        assert_eq!(elements(false, true, true), ":du");
        // all three
        assert_eq!(elements(true, true, true), ":sdu");
    }

    const ESC: u8 = 0x1b;

    /// When color is enabled, the emitted SGR sequence must be wrapped in
    /// readline's `\[` `\]` markers, and every ESC byte must fall inside them.
    #[test]
    fn set_color_wrapped_wraps_escapes() {
        let mut buffer = Buffer::ansi();
        set_color_wrapped(&mut buffer, ColorSpec::new().set_fg(Some(Color::Cyan)).set_bold(true))
            .unwrap();
        let out = buffer.as_slice();

        // sanity: termcolor actually emitted a non-printing sequence
        assert!(out.contains(&ESC), "expected an ANSI escape in {out:?}");
        assert!(out.starts_with(b"\\["), "must open with \\[: {out:?}");
        assert!(out.ends_with(b"\\]"), "must close with \\]: {out:?}");

        // no ESC byte may leak outside the \[ ... \] markers
        let inner = &out[2..out.len() - 2];
        assert!(!inner.contains(&b'\\'), "no stray markers inside: {inner:?}");
        assert!(!out[..2].contains(&ESC) && !out[out.len() - 2..].contains(&ESC));
    }

    /// With color disabled the helper emits nothing, so we never leave empty
    /// `\[\]` pairs in the prompt.
    #[test]
    fn set_color_wrapped_noop_without_color() {
        let mut buffer = Buffer::no_color();
        set_color_wrapped(&mut buffer, ColorSpec::new().set_fg(Some(Color::Cyan))).unwrap();
        assert!(buffer.as_slice().is_empty());
    }
}
