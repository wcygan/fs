use anyhow::Result;
use clap::Parser;
use std::path::{Path, PathBuf};
use tokio::fs;
use tokio::sync::mpsc;

/// Our CLI configuration
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

    /// Show hidden files and directories (Unix: name starts with '.', Windows: hidden attribute set)
    #[arg(short = 'H', long, default_value_t = false)]
    pub show_hidden: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Top layer: parse CLI, call search_files(), print results
    let config = SearchConfig::parse();

    // Call our BFS-based search engine, which returns an mpsc::Receiver of PathBuf results
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
    // We'll send found paths through an mpsc channel
    let (tx, rx) = mpsc::channel(100);

    // Clone arguments we need in the spawned task
    let root = config.root_path.clone();
    let pattern = config.pattern.clone();
    let exts = config.extensions.clone();
    let show_hidden = config.show_hidden;
    
    // If max_depth is None, treat it as unlimited
    let max_depth = config.max_depth.unwrap_or(usize::MAX);

    // Spawn one task to perform BFS
    tokio::spawn(async move {
        if let Err(e) = crawl_bfs(
            &root,
            max_depth,
            &pattern,
            exts.as_deref(),
            show_hidden,
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

/// Performs a BFS over directories, adding sub-directories into a queue.
/// No recursion is used here.
async fn crawl_bfs(
    root_dir: &Path,
    max_depth: usize,
    pattern: &str,
    extensions: Option<&[String]>,
    show_hidden: bool,
    tx: &mpsc::Sender<Result<PathBuf>>,
) -> Result<()> {
    use std::collections::VecDeque;

    // We'll store (directory_path, depth)
    let mut queue = VecDeque::new();
    queue.push_back((root_dir.to_path_buf(), 0));

    while let Some((dir, depth)) = queue.pop_front() {
        // If we've reached beyond max_depth, skip listing this directory’s contents
        if depth > max_depth {
            continue;
        }

        // Attempt to read the directory
        let mut entries = match fs::read_dir(&dir).await {
            Ok(e) => e,
            Err(e) => {
                // For something like a permission error or non-existent dir, send as error and skip
                let _ = tx.send(Err(e.into())).await;
                continue;
            }
        };

        // Iterate over items in the directory
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            let metadata = match entry.metadata().await {
                Ok(m) => m,
                Err(e) => {
                    let _ = tx.send(Err(e.into())).await;
                    continue;
                }
            };

            // Optional: skip hidden files/directories if show_hidden == false
            if !show_hidden && is_hidden(&path) {
                continue;
            }

            // If it's a directory and we haven't hit max_depth, enqueue it
            if metadata.is_dir() {
                if depth < max_depth {
                    queue.push_back((path, depth + 1));
                }
            } else {
                // It's a file; check if it matches the user-specified criteria
                if file_matches(&path, pattern, extensions) {
                    // Send matching files back via the channel
                    tx.send(Ok(path)).await?;
                }
            }
        }
    }

    Ok(())
}

//
// Cross-platform hidden-file detection
//
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
    // 0x2 = FILE_ATTRIBUTE_HIDDEN
    // Also treat files that literally start with '.' as hidden for consistency with user expectation
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

/// Check if a file name or extension matches the user’s criteria.
fn file_matches(path: &Path, pattern: &str, extensions: Option<&[String]>) -> bool {
    let file_name = match path.file_name().and_then(|n| n.to_str()) {
        Some(name) => name,
        None => return false,
    };

    // 1) Naive '*' pattern match
    if pattern != "*" && !naive_pattern_match(file_name, pattern) {
        return false;
    }

    // 2) Extension filter check
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

/// Very naive pattern matching: '*' means "any substring", no '?' support here.
/// For robust matching, use the `glob` crate or `regex` crate.
fn naive_pattern_match(name: &str, pat: &str) -> bool {
    if pat == "*" {
        return true;
    }
    // If user typed something else, do a quick "contains" check as a placeholder
    name.contains(&pat.replace('*', ""))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc::Receiver;

    /// We'll use `tempfile` to create a temporary directory for testing
    /// so we don't leave files behind.
    use tempfile::tempdir;
    use std::fs as stdfs;

    /// Helper: collects all results from the receiver into a Vec<PathBuf>, sorted.
    async fn collect_results(mut rx: Receiver<Result<PathBuf>>) -> Vec<PathBuf> {
        let mut results = Vec::new();
        while let Some(res) = rx.recv().await {
            if let Ok(path) = res {
                results.push(path);
            }
        }
        results.sort();
        results
    }

    #[tokio::test]
    async fn test_search_basic() -> Result<()> {
        let tmp = tempdir()?;
        let tmp_path = tmp.path();

        // Create some files/folders:
        // tmp/
        //   sub/
        //     file1.txt
        //     file2.rs
        //   .hidden/
        //     secret.txt
        //   data.bin
        //   random.txt
        let sub_dir = tmp_path.join("sub");
        let hidden_dir = tmp_path.join(".hidden");
        stdfs::create_dir_all(&sub_dir)?;
        stdfs::create_dir_all(&hidden_dir)?;

        let file1 = sub_dir.join("file1.txt");
        let file2 = sub_dir.join("file2.rs");
        let file3 = tmp_path.join("data.bin");
        let file4 = tmp_path.join("random.txt");
        let hidden_file = hidden_dir.join("secret.txt");

        stdfs::write(&file1, "hello")?;
        stdfs::write(&file2, "world")?;
        stdfs::write(&file3, "data")?;
        stdfs::write(&file4, "stuff")?;
        stdfs::write(&hidden_file, "shh!")?;

        // Now we run our BFS search
        let config = SearchConfig {
            root_path: tmp_path.to_path_buf(),
            pattern: "*".into(),
            max_depth: None,
            extensions: None,
            show_hidden: false, // do NOT show hidden
        };

        let rx = search_files(&config).await;
        let found = collect_results(rx).await;

        // We expect to see file1.txt, file2.rs, data.bin, random.txt
        // We do NOT expect to see .hidden/secret.txt
        let mut expected = vec![file1, file2, file3, file4];
        expected.sort();

        assert_eq!(found, expected, "BFS search did not find the expected files");
        Ok(())
    }

    #[tokio::test]
    async fn test_search_with_hidden() -> Result<()> {
        let tmp = tempdir()?;
        let tmp_path = tmp.path();

        // Create a hidden file at root
        let hidden_file = tmp_path.join(".hidden.txt");
        stdfs::write(&hidden_file, "secret data")?;

        // BFS searching with show_hidden = true
        let config = SearchConfig {
            root_path: tmp_path.to_path_buf(),
            pattern: "*".into(),
            max_depth: None,
            extensions: None,
            show_hidden: true, // show hidden
        };
        let rx = search_files(&config).await;
        let found = collect_results(rx).await;

        // We expect to see .hidden.txt
        assert!(
            found.contains(&hidden_file),
            "Expected hidden file was not found when show_hidden = true"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_search_extensions() -> Result<()> {
        let tmp = tempdir()?;
        let tmp_path = tmp.path();

        // Create a couple of files with different extensions
        let hello_txt = tmp_path.join("hello.txt");
        let readme_md = tmp_path.join("README.md");
        let code_rs = tmp_path.join("main.rs");

        stdfs::write(&hello_txt, "hello")?;
        stdfs::write(&readme_md, "# readme")?;
        stdfs::write(&code_rs, "fn main() {}")?;

        // We'll only search for .rs and .txt
        let config = SearchConfig {
            root_path: tmp_path.to_path_buf(),
            pattern: "*".into(),
            max_depth: None,
            extensions: Some(vec!["rs".into(), "txt".into()]),
            show_hidden: true,
        };

        let rx = search_files(&config).await;
        let found = collect_results(rx).await;

        // Expect hello.txt and main.rs, but not README.md
        assert!(found.contains(&hello_txt), "Expected to find hello.txt");
        assert!(found.contains(&code_rs), "Expected to find main.rs");
        assert!(!found.contains(&readme_md), "Should not have found README.md");

        Ok(())
    }

    #[tokio::test]
    async fn test_search_pattern_match() -> Result<()> {
        let tmp = tempdir()?;
        let tmp_path = tmp.path();

        let file_abc = tmp_path.join("abc-file.txt");
        let file_xyz = tmp_path.join("xyz-file.txt");
        stdfs::write(&file_abc, "abc contents")?;
        stdfs::write(&file_xyz, "xyz contents")?;

        // Use a pattern with a wildcard, e.g. "abc*"
        let config = SearchConfig {
            root_path: tmp_path.to_path_buf(),
            pattern: "abc*".into(),
            max_depth: None,
            extensions: None,
            show_hidden: true,
        };
        let rx = search_files(&config).await;
        let found = collect_results(rx).await;

        // We expect to only see "abc-file.txt"
        assert_eq!(found, vec![file_abc]);
        Ok(())
    }

    #[tokio::test]
    async fn test_search_zero_depth() -> Result<()> {
        let tmp = tempdir()?;
        let tmp_path = tmp.path();

        // Create a subdirectory and a file in root
        let sub_dir = tmp_path.join("sub");
        stdfs::create_dir_all(&sub_dir)?;
        let file_root = tmp_path.join("root_file.txt");
        let file_sub = sub_dir.join("sub_file.txt");
        stdfs::write(&file_root, "root contents")?;
        stdfs::write(&file_sub, "sub contents")?;

        // max_depth = 0 => we only see files directly in the root directory
        let config = SearchConfig {
            root_path: tmp_path.to_path_buf(),
            pattern: "*".into(),
            max_depth: Some(0),
            extensions: None,
            show_hidden: true,
        };
        let rx = search_files(&config).await;
        let found = collect_results(rx).await;

        // We only see root_file.txt, not sub_file.txt
        assert_eq!(found, vec![file_root]);
        Ok(())
    }

    #[tokio::test]
    async fn test_non_existent_root() -> Result<()> {
        // Provide a path that doesn't exist
        let non_existent = PathBuf::from("X:/thispathdoesnotexist12345");

        let config = SearchConfig {
            root_path: non_existent,
            pattern: "*".into(),
            max_depth: None,
            extensions: None,
            show_hidden: true,
        };
        let rx = search_files(&config).await;

        // We'll collect results; in this case, we expect an error, so no successful paths,
        // but we should at least ensure the BFS doesn't crash.
        let mut results = Vec::new();
        let mut errors = 0usize;

        let mut channel = rx;
        while let Some(msg) = channel.recv().await {
            match msg {
                Ok(path) => results.push(path),
                Err(_) => errors += 1,
            }
        }

        // For a non-existent directory, we expect at least 1 error and no found paths
        assert!(errors >= 1, "Expected at least one error for non-existent dir");
        assert!(results.is_empty(), "Expected no successful paths for a non-existent dir");
        Ok(())
    }
}
