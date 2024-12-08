use std::env::args_os;
use std::error::Error;
use std::ffi::OsString;
use std::io::{stdout, BufWriter, Write};
use std::os::unix::prelude::OsStrExt;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Instant;
use git2::{Repository, Status, StatusOptions};
use log::debug;

fn main() -> Result<(), Box<dyn Error>> {
    // we output some debug logs which can be turned on if needed.
    env_logger::init_from_env(env_logger::Env::default().filter("NUPROMPT_RUST_LOG"));
    
    // expect a status code as the first positional arg
    let exit_code = args_os().nth(1).filter(|a| !a.eq("0"));
    
    // cwd comes from libc or from the env var
    let possible_cwd = std::env::current_dir().ok()
        .or_else(|| std::env::var_os("PWD").map(PathBuf::from));
    
    // try and parse git status if were in a repo
    let start_time = Instant::now();
    let (cwd, git_bits): (PathBuf, Option<GitBits>) = match possible_cwd {
        Some(p) => {
            debug!("looking for git repo from working directory: {:?}", p);
            let ceil: &[PathBuf] = &[];
            match Repository::open_ext(&p, git2::RepositoryOpenFlags::empty(), ceil) {
                Ok(r) => (shorted_path_buf(p), Some(GitBits::from_repo(&r)?)),
                Err(e) => {
                    debug!("could not open repository: {:?}", e);
                    (shorted_path_buf(p), None)
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
    let mut stdout_lock = stdout().lock();
    let mut buf_writer = BufWriter::new(&mut stdout_lock);
    
    buf_writer.write_all(b"PS1='[")?;
    if let Some(exit_code) = exit_code {
        buf_writer.write_all(exit_code.as_bytes())?;
        buf_writer.write_all(b" ")?;
    }
    buf_writer.write_all(username.as_bytes())?;
    buf_writer.write_all(b" ")?;
    if let Some(git_bits) = git_bits {
        write_with_escaped_quote(git_bits.head_ref.as_bytes(), &mut buf_writer)?;
        git_bits.write_elements(&mut buf_writer)?;
        buf_writer.write_all(b" ")?;
    }
    
    write_with_escaped_quote(cwd.as_os_str().as_bytes(), &mut buf_writer)?;
    buf_writer.write_all(b" > '")?;
    buf_writer.flush()?;
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

/// GitBits holds the result of scanning the git repo for current status.
struct GitBits {
    head_ref: String,
    index_modified: bool,
    worktree_modified: bool,
    untracked_files: bool,
}

impl GitBits {

    fn from_repo(r: &Repository) -> Result<GitBits, anyhow::Error> {
        let short_ref = r.head()?.shorthand().unwrap().to_owned();
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
    match std::env::var("HOME").map(PathBuf::from) {
        Ok(h) if input.starts_with(&h) => PathBuf::from_str("~").unwrap().join(input.strip_prefix(h).unwrap()),
        _ => input,
    }
}
