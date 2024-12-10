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
use termcolor::{BufferWriter, Color, ColorChoice, ColorSpec, WriteColor};

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
        buffer.set_color(ColorSpec::new().set_fg(Some(Color::Red)))?;
        buffer.write_all(exit_code.as_bytes())?;
        buffer.write_all(b" ")?;
    }
    if let Some(elapsed) = elapsed {
        buffer.set_color(ColorSpec::new().set_fg(Some(Color::Cyan)))?;
        write!(buffer, "{:.2}s ", elapsed.as_f64())?;
    }
    buffer.set_color(ColorSpec::new().set_fg(Some(Color::Cyan)).set_bold(true).set_intense(true))?;
    buffer.write_all(username.as_bytes())?;
    buffer.write_all(b" ")?;
    if let Some(git_bits) = git_bits {
        buffer.set_color(ColorSpec::new().set_fg(Some(Color::Yellow)).set_intense(true))?;
        write_with_escaped_quote(git_bits.head_ref.as_bytes(), &mut buffer)?;
        buffer.set_color(&ColorSpec::default())?;
        git_bits.write_elements(&mut buffer)?;
        buffer.write_all(b" ")?;
    }
    buffer.set_color(&ColorSpec::default())?;
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
    match std::env::var(HOME_ENVVAR).map(PathBuf::from) {
        Ok(h) if input.starts_with(&h) => PathBuf::from_str("~").unwrap().join(input.strip_prefix(h).unwrap()),
        _ => input,
    }
}
