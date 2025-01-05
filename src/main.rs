use anyhow::Result;
use clap::Parser;
use std::path::{Path, PathBuf};
use tokio::fs;
use tokio::sync::mpsc;

#[derive(Parser, Debug)]
#[command(author, version, about = "A file system search tool")]
pub struct SearchConfig {
    /// The root directory to start the search from
    #[arg(default_value = ".")]
    pub root_path: PathBuf,

    /// Search pattern to match against file names (use '*' or '?' as wildcards, or adapt to real globs)
    #[arg(short, long, default_value = "*")]
    pub pattern: String,

    /// Maximum depth to search (unlimited if not provided)
    #[arg(short, long)]
    pub max_depth: Option<usize>,

    /// Only search files with these extensions (comma-separated)
    #[arg(short, long, value_delimiter = ',')]
    pub extensions: Option<Vec<String>>,

    /// Show hidden files and directories (Unix: name starts with '.', Windows: hidden attribute)
    #[arg(short = 'H', long, default_value_t = false)]
    pub show_hidden: bool,

    /// By default, we read .gitignore in the root directory and ignore those paths.
    /// If this is set, we do NOT ignore them (i.e., we include gitignored files too).
    #[arg(long, default_value_t = false)]
    pub include_gitignored: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Parse CLI
    let config = SearchConfig::parse();

    // Start BFS-based search, get a channel of results
    let mut rx = search_files(&config).await;

    // Drain the channel and print out each path
    while let Some(path_result) = rx.recv().await {
        match path_result {
            Ok(path) => println!("Found: {}", path.display()),
            Err(e)   => eprintln!("Error: {e}"),
        }
    }

    Ok(())
}

/// Top-level async entry to our crawler/search engine.
/// Spawns a task that uses BFS to traverse the filesystem.
async fn search_files(config: &SearchConfig) -> mpsc::Receiver<Result<PathBuf>> {
    let (tx, rx) = mpsc::channel(100);

    // Clone arguments for the spawned task
    let root = config.root_path.clone();
    let pattern = config.pattern.clone();
    let exts = config.extensions.clone();
    let show_hidden = config.show_hidden;
    let max_depth = config.max_depth.unwrap_or(usize::MAX);
    let include_gitignored = config.include_gitignored;

    // Build the Gitignore matcher (only from root/.gitignore)
    // If there's no .gitignore, or if it fails to parse, we fallback gracefully.
    let gitignore = build_gitignore(&root);

    tokio::spawn(async move {
        if let Err(e) = crawl_bfs(
            &root,
            max_depth,
            &pattern,
            exts.as_deref(),
            show_hidden,
            include_gitignored,
            &gitignore,
            &tx,
        )
        .await
        {
            let _ = tx.send(Err(e)).await;
        }
        drop(tx);
    });

    rx
}

/// Perform a BFS over directories without using recursion.
async fn crawl_bfs(
    root_dir: &Path,
    max_depth: usize,
    pattern: &str,
    extensions: Option<&[String]>,
    show_hidden: bool,
    include_gitignored: bool,
    gitignore: &Option<ignore::gitignore::Gitignore>,
    tx: &mpsc::Sender<Result<PathBuf>>,
) -> Result<()> {
    use std::collections::VecDeque;

    // We'll store (directory_path, depth)
    let mut queue = VecDeque::new();
    queue.push_back((root_dir.to_path_buf(), 0));

    while let Some((dir, depth)) = queue.pop_front() {
        if depth > max_depth {
            continue;
        }

        let mut entries = match fs::read_dir(&dir).await {
            Ok(e) => e,
            Err(e) => {
                // If e.g. the directory doesn't exist or we have no permission, send an error and skip
                let _ = tx.send(Err(e.into())).await;
                continue;
            }
        };

        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();

            // If the user does not want to include .gitignore'd files,
            // skip anything that is matched by .gitignore
            if !include_gitignored && is_gitignored(&path, gitignore) {
                continue;
            }

            let metadata = match entry.metadata().await {
                Ok(m) => m,
                Err(e) => {
                    let _ = tx.send(Err(e.into())).await;
                    continue;
                }
            };

            if !show_hidden && is_hidden(&path) {
                continue;
            }

            if metadata.is_dir() {
                if depth < max_depth {
                    queue.push_back((path, depth + 1));
                }
            } else {
                if file_matches(&path, pattern, extensions) {
                    tx.send(Ok(path)).await?;
                }
            }
        }
    }

    Ok(())
}

/// Build a Gitignore object from "root_dir/.gitignore", if it exists.
fn build_gitignore(root_dir: &Path) -> Option<ignore::gitignore::Gitignore> {
    use ignore::gitignore::GitignoreBuilder;

    let gitignore_path = root_dir.join(".gitignore");
    if !gitignore_path.is_file() {
        // No .gitignore in root, or not a file
        return None;
    }
    let mut builder = GitignoreBuilder::new(root_dir);
    if builder.add(gitignore_path).is_some() {
        // If we fail to parse, fallback to ignoring
        return None;
    }
    match builder.build() {
        Ok(gi) => Some(gi),
        Err(_) => None,
    }
}

/// Check if path is matched by the .gitignore (and thus should be ignored).
fn is_gitignored(path: &Path, gitignore: &Option<ignore::gitignore::Gitignore>) -> bool {
    if let Some(ref gi) = gitignore {
        let matched = gi.matched_path_or_any_parents(path, path.is_dir());
        matched.is_ignore()
    } else {
        false
    }
}

/// Cross-platform hidden-file detection
#[cfg(unix)]
fn is_hidden(path: &Path) -> bool {
    // On Unix, hidden if file name starts with '.'
    match path.file_name() {
        Some(name) => name.to_str().map(|s| s.starts_with('.')).unwrap_or(false),
        None => false,
    }
}

#[cfg(windows)]
fn is_hidden(path: &Path) -> bool {
    use std::os::windows::prelude::MetadataExt;

    // On Windows, hidden if the hidden attribute is set
    // (FILE_ATTRIBUTE_HIDDEN = 0x2).  
    // We also treat files that literally start with '.' as hidden for user convenience.
    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
        if name.starts_with('.') {
            return true;
        }
    }

    match path.metadata() {
        Ok(meta) => (meta.file_attributes() & 0x2) != 0,
        Err(_) => false,
    }
}

/// Check if a file name or extension matches the user’s pattern and extension filters.
fn file_matches(path: &Path, pattern: &str, extensions: Option<&[String]>) -> bool {
    let file_name = match path.file_name().and_then(|n| n.to_str()) {
        Some(name) => name,
        None => return false,
    };

    // 1) Naive '*' pattern match
    if pattern != "*" && !naive_pattern_match(file_name, pattern) {
        return false;
    }

    // 2) Extension filter
    if let Some(exts) = extensions {
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            if !exts.iter().any(|allowed| allowed.eq_ignore_ascii_case(ext)) {
                return false;
            }
        } else {
            // If there's no extension but user wants some, skip
            return false;
        }
    }

    true
}

/// Very naive wildcard: '*' means "any substring".  
/// For robust matching, consider the `glob` or `regex` crate.
fn naive_pattern_match(name: &str, pat: &str) -> bool {
    if pat == "*" {
        return true;
    }
    name.contains(&pat.replace('*', ""))
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use std::fs as stdfs;
    use tempfile::tempdir;
    use tokio::sync::mpsc::Receiver;

    /// Collect all successful results from the receiver into a sorted Vec.
    async fn collect_results(mut rx: Receiver<Result<PathBuf>>) -> Vec<PathBuf> {
        let mut results = Vec::new();
        while let Some(item) = rx.recv().await {
            if let Ok(path) = item {
                results.push(path);
            }
        }
        results.sort();
        results
    }

    #[tokio::test]
    async fn test_gitignore_default() -> Result<()> {
        // By default, .gitignore is read, and anything that matches is excluded.
        let tmp = tempdir()?;
        let tmp_path = tmp.path();

        // Create .gitignore that ignores *.log
        let gitignore_path = tmp_path.join(".gitignore");
        stdfs::write(&gitignore_path, "*.log\n")?;

        // Create files
        let file_txt = tmp_path.join("notes.txt");
        let file_log = tmp_path.join("debug.log");
        stdfs::write(&file_txt, "hello")?;
        stdfs::write(&file_log, "some logs")?;

        let config = SearchConfig {
            root_path: tmp_path.to_path_buf(),
            pattern: "*".into(),
            max_depth: None,
            extensions: None,
            show_hidden: true,
            include_gitignored: false, // default: do NOT include .gitignore’d
        };

        let rx = search_files(&config).await;
        let found = collect_results(rx).await;

        // We expect to see notes.txt, but NOT debug.log
        assert!(
            found.contains(&file_txt),
            "Expected to find notes.txt, but didn't."
        );
        assert!(
            !found.contains(&file_log),
            "Should NOT have found debug.log if .gitignore is in effect."
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_include_gitignored() -> Result<()> {
        // If the user sets --include-gitignored, we ignore the .gitignore instructions.
        let tmp = tempdir()?;
        let tmp_path = tmp.path();

        // Create .gitignore that ignores *.log
        let gitignore_path = tmp_path.join(".gitignore");
        stdfs::write(&gitignore_path, "*.log\n")?;

        // Create files
        let file_txt = tmp_path.join("notes.txt");
        let file_log = tmp_path.join("debug.log");
        stdfs::write(&file_txt, "hello")?;
        stdfs::write(&file_log, "some logs")?;

        let config = SearchConfig {
            root_path: tmp_path.to_path_buf(),
            pattern: "*".into(),
            max_depth: None,
            extensions: None,
            show_hidden: true,
            include_gitignored: true, // override .gitignore
        };

        let rx = search_files(&config).await;
        let found = collect_results(rx).await;

        // We expect to see BOTH notes.txt AND debug.log
        assert!(
            found.contains(&file_txt),
            "Expected to find notes.txt, but didn't."
        );
        assert!(
            found.contains(&file_log),
            "Expected to find debug.log, but didn't."
        );

        Ok(())
    }

    // We can still reuse the tests from before, verifying BFS, hidden-file logic, etc.
    // A few are shown below; you could copy the entire suite from the previous example.

    #[tokio::test]
    async fn test_search_basic() -> Result<()> {
        let tmp = tempdir()?;
        let tmp_path = tmp.path();

        // Create sub dir + files
        let sub_dir = tmp_path.join("sub");
        stdfs::create_dir_all(&sub_dir)?;
        let file1 = sub_dir.join("file1.txt");
        let file2 = sub_dir.join("file2.rs");
        let file3 = tmp_path.join("data.bin");

        stdfs::write(&file1, "hello")?;
        stdfs::write(&file2, "world")?;
        stdfs::write(&file3, "data")?;

        let config = SearchConfig {
            root_path: tmp_path.to_path_buf(),
            pattern: "*".into(),
            max_depth: None,
            extensions: None,
            show_hidden: false, // do NOT show hidden
            include_gitignored: false,
        };

        let rx = search_files(&config).await;
        let found = collect_results(rx).await;

        let mut expected = vec![file1, file2, file3];
        expected.sort();
        assert_eq!(found, expected);

        Ok(())
    }
}
