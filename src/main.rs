use std::env::args_os;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{stdin, IsTerminal, Write};
use std::ops::Deref;
use std::os::unix::prelude::OsStrExt;
use std::path::PathBuf;
use std::str::FromStr;
use anyhow::{anyhow, Context};
use gix::bstr::ByteSlice;
use coarsetime::Duration;
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
                    // gix::discover walks up parent directories to find the repo,
                    // matching git2's Repository::open_ext upward discovery.
                    match gix::discover(&p) {
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
    //
    // We gate colour on stdin rather than stdout. Our stdout is always the pipe of the enclosing
    // `eval $(nuprompt ps1 $$ $?)` command substitution, so `stdout().is_terminal()` would always be
    // false and force colour off - the escape codes we emit are meant to end up inside the resulting
    // PS1 string, so we do want them when the shell is interactive. stdin (like stderr) is inherited
    // straight from the shell and is normally a terminal in an interactive session, which is the
    // signal we actually care about here (it is only a proxy - e.g. `bash -i` with stdin redirected
    // would still disable colour). This mirrors termcolor's own recommended pattern of downgrading Auto to
    // Never when stdin is not a terminal. ColorChoice::Auto still honours NO_COLOR and TERM=dumb for
    // us (termcolor only consults those, not the stream's tty status, when building a Buffer).
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

/// Parse the stored tick count from the prev-start file contents. The file holds a u64 encoded as
/// 8 big-endian bytes, but a truncated or empty file must not panic on the slice.
fn parse_stored_ticks(contents: &[u8]) -> Result<u64, anyhow::Error> {
    let bytes = contents
        .get(..8)
        .ok_or_else(|| anyhow!("prev start file too short: {} bytes", contents.len()))?;
    Ok(u64::from_be_bytes(bytes.try_into()?))
}

fn read_elapsed_time(raw_pid: &OsStr) -> Result<Duration, anyhow::Error> {
    let tf = prev_start_file_path(raw_pid);
    let contents = fs::read(&tf)?;
    fs::remove_file(&tf)?;
    let ticks = parse_stored_ticks(&contents)?;
    let now_ticks = coarsetime::Instant::now().as_ticks();
    debug!("read start time from pid file: {} now={}", ticks, now_ticks);
    // saturating_sub guards against a stored tick from the future or a clock reset, which would
    // otherwise underflow (panic in debug, bogus huge duration in release).
    Ok(Duration::from_ticks(now_ticks.saturating_sub(ticks)))
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

    fn from_repo(r: &gix::Repository) -> Result<GitBits, anyhow::Error> {
        // Reproduce git2's head().shorthand() semantics exactly:
        //   * a normal branch  -> the short ref name (e.g. "main")
        //   * a detached HEAD  -> "HEAD" (git2 resolves HEAD to a direct ref)
        //   * an unborn branch -> "NO HEAD" (git2 returns an error here)
        // shorten() can hold non-UTF-8 bytes, in which case we fall back to "?"
        // rather than panic, mirroring the old shorthand().unwrap_or("?").
        let head_ref = match r.head() {
            Ok(h) => match h.kind {
                gix::head::Kind::Symbolic(reference) => {
                    reference.name.shorten().to_str().unwrap_or("?").to_owned()
                }
                gix::head::Kind::Detached { .. } => String::from("HEAD"),
                gix::head::Kind::Unborn(_) => String::from("NO HEAD"),
            },
            Err(e) => {
                debug!("error reading head ref: {}", e);
                String::from("NO HEAD")
            }
        };
        let mut gb = GitBits{
            head_ref,
            index_modified: false,
            worktree_modified: false,
            untracked_files: false,
        };
        // Status scan reduced to three booleans, matching the old git2 mapping:
        //   index_modified    <- any HEAD-vs-index (staged) change
        //   worktree_modified <- any index-vs-worktree modification/rewrite
        //   untracked_files   <- any untracked (not ignored) file on disk
        // Submodules are excluded (Ignore::All) and ignored files are not listed,
        // matching exclude_submodules(true) + include_ignored(false).
        // The old git2 code passed include_untracked(true) explicitly, which
        // overrode any git config. gix instead honours status.showUntrackedFiles,
        // and a value of "no" makes status() tear down the directory walk (setting
        // dirwalk_options to None). Once that happens, untracked_files() below is a
        // documented no-op, so re-establish a default walk first to keep output
        // identical regardless of the user's config.
        let iter = r.status(gix::progress::Discard)?
            .index_worktree_options_mut(|opts| {
                if opts.dirwalk_options.is_none() {
                    opts.dirwalk_options = r.dirwalk_options().ok();
                }
            })
            .untracked_files(gix::status::UntrackedFiles::Files)
            .index_worktree_submodules(gix::status::Submodule::Given {
                ignore: gix::submodule::config::Ignore::All,
                check_dirty: false,
            })
            .into_iter(None)?;
        for item in iter {
            let item = item?;
            debug!("git status: {:?}", item);
            match item {
                gix::status::Item::TreeIndex(_) => gb.index_modified = true,
                gix::status::Item::IndexWorktree(iw) => match iw {
                    gix::status::index_worktree::Item::Modification { .. } => {
                        gb.worktree_modified = true;
                    }
                    gix::status::index_worktree::Item::Rewrite { .. } => {
                        gb.worktree_modified = true;
                    }
                    gix::status::index_worktree::Item::DirectoryContents { entry, .. } => {
                        if entry.status == gix::dir::entry::Status::Untracked {
                            gb.untracked_files = true;
                        }
                    }
                },
            }
            // Only the three booleans matter, so stop walking as soon as all are
            // set. This is the hot path (the prompt renders on every command) and
            // large dirty repos can otherwise enumerate thousands of entries.
            if gb.index_modified && gb.worktree_modified && gb.untracked_files {
                break;
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

    #[test]
    fn parse_stored_ticks_valid() {
        let bytes = 42u64.to_be_bytes();
        assert_eq!(parse_stored_ticks(&bytes).unwrap(), 42);
    }

    #[test]
    fn parse_stored_ticks_extra_bytes_ignored() {
        let mut bytes = 7u64.to_be_bytes().to_vec();
        bytes.push(0xFF);
        assert_eq!(parse_stored_ticks(&bytes).unwrap(), 7);
    }

    #[test]
    fn parse_stored_ticks_empty_errs() {
        assert!(parse_stored_ticks(&[]).is_err());
    }

    #[test]
    fn parse_stored_ticks_truncated_errs() {
        // fewer than 8 bytes must error, not panic on the slice.
        assert!(parse_stored_ticks(&[0, 1, 2, 3]).is_err());
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

    // --- GitBits::from_repo integration tests ---------------------------------
    //
    // gix is pre-1.0 with a churning status API, so these lock in the exact
    // head-ref and three-boolean semantics we depend on against future upgrades.
    // They shell out to the real `git` to build repos, then open them with gix.

    use std::path::Path;
    use std::process::Command;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Unique scratch directory, git-initialised, with deterministic identity so
    /// commits work without relying on the host's git config.
    fn tmp_repo() -> PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "nuprompt-test-{}-{}",
            std::process::id(),
            n
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q", "-b", "main"]);
        git(&dir, &["config", "user.name", "t"]);
        git(&dir, &["config", "user.email", "t@t"]);
        dir
    }

    fn git(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .current_dir(dir)
            .args(args)
            .status()
            .expect("git must be on PATH")
            .success();
        assert!(ok, "git {:?} failed", args);
    }

    fn write(dir: &Path, name: &str, contents: &str) {
        fs::write(dir.join(name), contents).unwrap();
    }

    fn bits(dir: &Path) -> GitBits {
        let repo = gix::discover(dir).expect("discover repo");
        GitBits::from_repo(&repo).expect("from_repo")
    }

    #[test]
    fn from_repo_clean_has_branch_and_no_flags() {
        let d = tmp_repo();
        git(&d, &["commit", "-q", "--allow-empty", "-m", "init"]);
        let b = bits(&d);
        assert_eq!(b.head_ref, "main");
        assert!(!b.index_modified && !b.worktree_modified && !b.untracked_files);
    }

    #[test]
    fn from_repo_staged_only_sets_index_modified() {
        let d = tmp_repo();
        git(&d, &["commit", "-q", "--allow-empty", "-m", "init"]);
        write(&d, "f", "a");
        git(&d, &["add", "f"]);
        let b = bits(&d);
        assert!(b.index_modified);
        assert!(!b.worktree_modified);
        assert!(!b.untracked_files);
    }

    #[test]
    fn from_repo_worktree_modified_sets_worktree_flag() {
        let d = tmp_repo();
        write(&d, "f", "a");
        git(&d, &["add", "f"]);
        git(&d, &["commit", "-q", "-m", "init"]);
        write(&d, "f", "a-changed");
        let b = bits(&d);
        assert!(!b.index_modified);
        assert!(b.worktree_modified);
        assert!(!b.untracked_files);
    }

    #[test]
    fn from_repo_untracked_sets_untracked_flag() {
        let d = tmp_repo();
        git(&d, &["commit", "-q", "--allow-empty", "-m", "init"]);
        write(&d, "new", "x");
        let b = bits(&d);
        assert!(!b.index_modified);
        assert!(!b.worktree_modified);
        assert!(b.untracked_files);
    }

    #[test]
    fn from_repo_all_three_flags() {
        let d = tmp_repo();
        write(&d, "tracked", "a");
        git(&d, &["add", "tracked"]);
        git(&d, &["commit", "-q", "-m", "init"]);
        write(&d, "staged", "s");
        git(&d, &["add", "staged"]);
        write(&d, "tracked", "a-changed");
        write(&d, "untracked", "u");
        let b = bits(&d);
        assert!(b.index_modified && b.worktree_modified && b.untracked_files);
    }

    #[test]
    fn from_repo_ignored_files_are_excluded() {
        let d = tmp_repo();
        write(&d, ".gitignore", "ign\n");
        git(&d, &["add", ".gitignore"]);
        git(&d, &["commit", "-q", "-m", "init"]);
        write(&d, "ign", "junk");
        let b = bits(&d);
        assert!(!b.untracked_files, "ignored file must not count as untracked");
        assert!(!b.index_modified && !b.worktree_modified);
    }

    #[test]
    fn from_repo_detached_head_reads_as_head() {
        let d = tmp_repo();
        git(&d, &["commit", "-q", "--allow-empty", "-m", "init"]);
        git(&d, &["checkout", "-q", "--detach", "HEAD"]);
        assert_eq!(bits(&d).head_ref, "HEAD");
    }

    #[test]
    fn from_repo_unborn_head_reads_as_no_head() {
        let d = tmp_repo();
        // freshly init'd, no commit yet -> unborn branch
        assert_eq!(bits(&d).head_ref, "NO HEAD");
    }

    #[test]
    fn from_repo_discovers_from_subdirectory() {
        let d = tmp_repo();
        git(&d, &["commit", "-q", "--allow-empty", "-m", "init"]);
        write(&d, "untracked", "u");
        let nested = d.join("a").join("b");
        fs::create_dir_all(&nested).unwrap();
        let b = bits(&nested);
        assert_eq!(b.head_ref, "main");
        assert!(b.untracked_files);
    }
}
