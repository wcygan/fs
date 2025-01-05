use anyhow::Result;
use clap::Parser;
use std::path::{Path, PathBuf};
use tokio::fs;
use tokio::sync::mpsc;

#[derive(Parser, Debug)]
#[command(author, version, about = "A file system search tool that supports .gitignore")]
pub struct SearchConfig {
    /// The root directory to start the search from
    #[arg(default_value = ".")]
    pub root_path: PathBuf,

    /// Search pattern to match against file names (use '*' wildcard; naive only)
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

    /// By default, we read .gitignore in the root directory and ignore those paths.
    /// If set, we do NOT ignore them (i.e., we include gitignored files).
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

/// Creates an mpsc channel and spawns the BFS task.
async fn search_files(config: &SearchConfig) -> mpsc::Receiver<Result<PathBuf>> {
    let (tx, rx) = mpsc::channel(100);

    let root = config.root_path.clone();
    let pattern = config.pattern.clone();
    let exts = config.extensions.clone();
    let show_hidden = config.show_hidden;
    let max_depth = config.max_depth.unwrap_or(usize::MAX);
    let include_gitignored = config.include_gitignored;

    // Build the Gitignore matcher (only from root/.gitignore)
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

/// Performs BFS without recursion, respecting .gitignore, hidden, patterns, etc.
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
    let mut queue = VecDeque::new();
    queue.push_back((root_dir.to_path_buf(), 0));

    while let Some((dir, depth)) = queue.pop_front() {
        if depth > max_depth {
            continue;
        }

        let mut entries = match fs::read_dir(&dir).await {
            Ok(e) => e,
            Err(e) => {
                // e.g., permission denied or path doesn't exist
                let _ = tx.send(Err(e.into())).await;
                continue;
            }
        };

        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();

            // If user does NOT want to include gitignored, skip if matched
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

            // hidden check
            if !show_hidden && is_hidden(&path) {
                continue;
            }

            // BFS queue subdirectories
            if metadata.is_dir() {
                if depth < max_depth {
                    queue.push_back((path, depth + 1));
                }
            } else {
                // If it's a file, check pattern / extension
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
        return None;
    }

    let mut builder = GitignoreBuilder::new(root_dir);
    if builder.add(gitignore_path).is_some() {
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

/// Cross-platform hidden detection
#[cfg(unix)]
fn is_hidden(path: &Path) -> bool {
    match path.file_name() {
        Some(name) => name.to_str().map(|s| s.starts_with('.')).unwrap_or(false),
        None => false,
    }
}

#[cfg(windows)]
fn is_hidden(path: &Path) -> bool {
    use std::os::windows::prelude::MetadataExt;

    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
        if name.starts_with('.') {
            return true;
        }
    }
    match path.metadata() {
        Ok(meta) => (meta.file_attributes() & 0x2) != 0, // FILE_ATTRIBUTE_HIDDEN = 0x2
        Err(_) => false,
    }
}

/// Pattern and extension checks
fn file_matches(path: &Path, pattern: &str, extensions: Option<&[String]>) -> bool {
    let file_name = match path.file_name().and_then(|n| n.to_str()) {
        Some(name) => name,
        None => return false,
    };

    if pattern != "*" && !naive_pattern_match(file_name, pattern) {
        return false;
    }

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

/// Naive '*' pattern => substring match
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

    /// Collect all successful PathBuf results
    async fn collect_results(mut rx: Receiver<Result<PathBuf>>) -> Vec<PathBuf> {
        let mut v = Vec::new();
        while let Some(item) = rx.recv().await {
            if let Ok(path) = item {
                v.push(path);
            }
        }
        v.sort();
        v
    }

    // -- 1) BASIC TESTS --

    /// Basic BFS search with no .gitignore
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
            show_hidden: false,
            include_gitignored: false,
        };
        let found = collect_results(search_files(&config).await).await;

        let mut expected = vec![file1, file2, file3];
        expected.sort();
        assert_eq!(found, expected);

        Ok(())
    }

    /// Searching an empty directory yields no files
    #[tokio::test]
    async fn test_empty_dir() -> Result<()> {
        let tmp = tempdir()?;
        let tmp_path = tmp.path();

        let config = SearchConfig {
            root_path: tmp_path.to_path_buf(),
            pattern: "*".into(),
            max_depth: None,
            extensions: None,
            show_hidden: false,
            include_gitignored: false,
        };
        let found = collect_results(search_files(&config).await).await;
        assert_eq!(found.len(), 0, "Expected no files in empty directory");
        Ok(())
    }

    /// If the directory doesn't exist, we should get an error, but no results
    #[tokio::test]
    async fn test_non_existent_directory() -> Result<()> {
        let non_existent = PathBuf::from("X:/some-non-existent-1234");
        let config = SearchConfig {
            root_path: non_existent,
            pattern: "*".into(),
            max_depth: None,
            extensions: None,
            show_hidden: true,
            include_gitignored: false,
        };
        let rx = search_files(&config).await;

        let mut files = Vec::new();
        let mut errors = 0usize;

        let mut chan = rx;
        while let Some(msg) = chan.recv().await {
            match msg {
                Ok(path) => files.push(path),
                Err(_) => errors += 1,
            }
        }

        assert_eq!(files.len(), 0);
        assert!(errors >= 1, "Should have at least one error from non-existent dir");

        Ok(())
    }

    // -- 2) HIDDEN FILES --

    /// We skip hidden files by default, show them if show_hidden = true
    #[tokio::test]
    async fn test_hidden_files() -> Result<()> {
        let tmp = tempdir()?;
        let tmp_path = tmp.path();

        // Create hidden file
        let hidden_file = tmp_path.join(".hidden.txt");
        stdfs::write(&hidden_file, "secret")?;

        // By default, show_hidden = false => we won't see it
        let config = SearchConfig {
            root_path: tmp_path.to_path_buf(),
            pattern: "*".into(),
            max_depth: None,
            extensions: None,
            show_hidden: false,
            include_gitignored: false,
        };
        let found = collect_results(search_files(&config).await).await;
        assert!(
            !found.contains(&hidden_file),
            "Should NOT see hidden_file when show_hidden is false"
        );

        // If we set show_hidden = true => we do see it
        let config2 = SearchConfig {
            show_hidden: true,
            ..config
        };
        let found2 = collect_results(search_files(&config2).await).await;
        assert!(
            found2.contains(&hidden_file),
            "Expected to see hidden_file when show_hidden is true"
        );

        Ok(())
    }

    // -- 3) EXTENSIONS --

    #[tokio::test]
    async fn test_search_extensions() -> Result<()> {
        let tmp = tempdir()?;
        let tmp_path = tmp.path();

        let file_txt = tmp_path.join("hello.txt");
        let file_md = tmp_path.join("readme.md");
        stdfs::write(&file_txt, "hello")?;
        stdfs::write(&file_md, "# readme")?;

        let config = SearchConfig {
            root_path: tmp_path.to_path_buf(),
            pattern: "*".into(),
            max_depth: None,
            extensions: Some(vec!["txt".into()]),
            show_hidden: true,
            include_gitignored: false,
        };
        let found = collect_results(search_files(&config).await).await;

        // Expect to see only hello.txt
        assert!(found.contains(&file_txt), "Expected to see .txt file");
        assert!(!found.contains(&file_md), "Should NOT see .md file");
        Ok(())
    }

    // -- 4) MAX DEPTH --

    #[tokio::test]
    async fn test_max_depth() -> Result<()> {
        let tmp = tempdir()?;
        let tmp_path = tmp.path();

        // Create a multi-level directory
        // root
        //   level1/
        //     level2/
        //       file.txt
        let level1 = tmp_path.join("level1");
        let level2 = level1.join("level2");
        stdfs::create_dir_all(&level2)?;

        let file_txt = level2.join("deep_file.txt");
        stdfs::write(&file_txt, "deep")?;

        // max_depth = 1 => we see items in root, but not in level1/level2
        let config = SearchConfig {
            root_path: tmp_path.to_path_buf(),
            pattern: "*".into(),
            max_depth: Some(1),
            extensions: None,
            show_hidden: true,
            include_gitignored: false,
        };
        let found = collect_results(search_files(&config).await).await;
        assert!(!found.contains(&file_txt), "Should not see file at depth 2");

        // max_depth = 2 => we can see file_txt
        let config2 = SearchConfig {
            max_depth: Some(2),
            ..config
        };
        let found2 = collect_results(search_files(&config2).await).await;
        assert!(found2.contains(&file_txt), "Should see file at depth 2");
        Ok(())
    }

    // -- 5) GITIGNORE SCENARIOS --

    /// .gitignore ignores *.log by default
    #[tokio::test]
    async fn test_gitignore_default() -> Result<()> {
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
            include_gitignored: false,
        };
        let found = collect_results(search_files(&config).await).await;

        // We expect to see notes.txt, but NOT debug.log
        assert!(found.contains(&file_txt), "Expected to find notes.txt");
        assert!(
            !found.contains(&file_log),
            "Should NOT find debug.log if it's .gitignored"
        );

        Ok(())
    }

    /// If --include-gitignored is set, we see all files
    #[tokio::test]
    async fn test_include_gitignored() -> Result<()> {
        let tmp = tempdir()?;
        let tmp_path = tmp.path();

        // .gitignore
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
            include_gitignored: true, // override ignoring
        };
        let found = collect_results(search_files(&config).await).await;

        assert!(found.contains(&file_txt));
        assert!(
            found.contains(&file_log),
            "Expected to see log file, ignoring .gitignore"
        );
        Ok(())
    }

    /// Multiple lines in .gitignore, plus blank lines and comments
    #[tokio::test]
    async fn test_gitignore_multi_line() -> Result<()> {
        let tmp = tempdir()?;
        let tmp_path = tmp.path();
    
        // IMPORTANT: Remove leading spaces so the ignore crate parses them correctly
        let content = r#"# This is a comment
*.log
secret_*

# blank line above
*.tmp
"#;
        let gitignore_path = tmp_path.join(".gitignore");
        stdfs::write(&gitignore_path, content)?;
    
        // Create files
        let f_log = tmp_path.join("debug.log");
        let f_secret = tmp_path.join("secret_file.txt");
        let f_tmp = tmp_path.join("random.tmp");
        let f_txt = tmp_path.join("notes.txt");
    
        stdfs::write(&f_log, "log")?;
        stdfs::write(&f_secret, "secret")?;
        stdfs::write(&f_tmp, "tmp data")?;
        stdfs::write(&f_txt, "notes")?;
    
        let config = SearchConfig {
            root_path: tmp_path.to_path_buf(),
            pattern: "*".into(),
            max_depth: None,
            extensions: None,
            show_hidden: true,
            include_gitignored: false,
        };
        let found = collect_results(search_files(&config).await).await;
    
        // Debug output
        println!("Found files: {:?}", found);
        println!("Gitignore content:\n{}", content);
        
        // We expect to see only notes.txt and the .gitignore itself
        let expected: Vec<_> = vec![
            tmp_path.join(".gitignore"),
            tmp_path.join("notes.txt"),
        ];
        
        assert_eq!(
            found.len(),
            expected.len(),
            "Expected exactly {} files, found {}",
            expected.len(),
            found.len()
        );
        for path in &expected {
            assert!(found.contains(path), "Expected to find: {}", path.display());
        }
        
        // Additional specific checks
        assert!(!found.contains(&f_log), "Should ignore *.log");
        assert!(!found.contains(&f_secret), "Should ignore secret_*");
        assert!(!found.contains(&f_tmp), "Should ignore *.tmp");
    
        Ok(())
    }

    /// .gitignore that doesn't exist => no ignoring
    #[tokio::test]
    async fn test_no_gitignore_file() -> Result<()> {
        let tmp = tempdir()?;
        let tmp_path = tmp.path();

        // There's no .gitignore
        let file_log = tmp_path.join("debug.log");
        stdfs::write(&file_log, "some logs")?;

        // Should see debug.log
        let config = SearchConfig {
            root_path: tmp_path.to_path_buf(),
            pattern: "*".into(),
            max_depth: None,
            extensions: None,
            show_hidden: true,
            include_gitignored: false,
        };
        let found = collect_results(search_files(&config).await).await;
        assert!(found.contains(&file_log));
        Ok(())
    }

    // -- 6) PATTERN SPECIFICS --

    #[tokio::test]
    async fn test_pattern_substring() -> Result<()> {
        let tmp = tempdir()?;
        let tmp_path = tmp.path();

        let abc = tmp_path.join("abc-file.txt");
        let xyz = tmp_path.join("xyz-file.txt");
        stdfs::write(&abc, "abc")?;
        stdfs::write(&xyz, "xyz")?;

        // Pattern "abc*" => naive substring check => matches "abc-file.txt"
        let config = SearchConfig {
            root_path: tmp_path.to_path_buf(),
            pattern: "abc*".into(),
            max_depth: None,
            extensions: None,
            show_hidden: true,
            include_gitignored: false,
        };
        let found = collect_results(search_files(&config).await).await;

        assert!(found.contains(&abc));
        assert!(!found.contains(&xyz));
        Ok(())
    }

    // -- 7) PERMISSION ERRORS --

    #[tokio::test]
    async fn test_permission_denied() -> Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let tmp = tempdir()?;
            let tmp_path = tmp.path();

            // We create an unreadable directory
            let locked_dir = tmp_path.join("locked");
            stdfs::create_dir_all(&locked_dir)?;

            // Then remove read permissions
            let mut perms = stdfs::metadata(&locked_dir)?.permissions();
            perms.set_mode(0o000); // no permissions
            stdfs::set_permissions(&locked_dir, perms)?;

            // BFS should return an error for locked_dir, but it won't crash
            let config = SearchConfig {
                root_path: tmp_path.to_path_buf(),
                pattern: "*".into(),
                max_depth: None,
                extensions: None,
                show_hidden: true,
                include_gitignored: false,
            };
            let rx = search_files(&config).await;

            let mut files_found = Vec::new();
            let mut errors = 0;
            let mut channel = rx;
            while let Some(item) = channel.recv().await {
                match item {
                    Ok(p) => files_found.push(p),
                    Err(_) => errors += 1,
                }
            }

            // We didn't create any files, so no found paths
            // We do expect at least 1 error from locked_dir
            assert_eq!(files_found.len(), 0);
            assert!(
                errors >= 1,
                "Expected at least one error from permission-denied directory"
            );

            // Reset permissions so tempdir can clean up
            let mut perms2 = stdfs::metadata(&locked_dir)?.permissions();
            perms2.set_mode(0o755);
            stdfs::set_permissions(&locked_dir, perms2)?;
        }

        // On Windows, setting read-only to a directory doesn't yield the same error pattern.
        // We'll skip this scenario on Windows or handle with other approaches.
        Ok(())
    }
    
    use quickcheck::{Arbitrary, Gen, QuickCheck, TestResult};
    
    /// Arbitrary string generator for QuickCheck
    /// This example just generates ASCII strings of modest length;
    /// tweak as needed for your use cases.
    #[derive(Clone, Debug)]
    struct RandomString(pub String);

    impl Arbitrary for RandomString {
        fn arbitrary(g: &mut Gen) -> Self {
            // Generate ASCII characters only, up to 50 in length
            let size = usize::arbitrary(g) % 50;
            let s: String = (0..size)
                .map(|_| {
                    let c = u8::arbitrary(g) % 128; // ASCII range
                    c as char
                })
                .collect();
            RandomString(s)
        }
    }

    /// Property: If the pattern is "*", then naive_pattern_match() should
    /// always return true for any input string.
    #[test]
    fn prop_star_matches_all_strings() {
            fn prop(s: RandomString) -> TestResult {
                let pat = "*";
                let matched = naive_pattern_match(&s.0, pat);
                // This should *always* be true
                TestResult::from_bool(matched)
            }
            QuickCheck::new().quickcheck(prop as fn(RandomString) -> TestResult);
        }

    /// Property: If the pattern does not contain '*', then `naive_pattern_match`
    /// is effectively `string.contains(pat)`.
    #[test]
    fn prop_substring_equivalent() {
        fn inner(s: RandomString, pat: RandomString) -> TestResult {
            // We artificially remove '*' from `pat` to test substring logic
            let pat_no_star = pat.0.replace('*', "");
            let direct_contains = s.0.contains(&pat_no_star);
            let our_match = naive_pattern_match(&s.0, &pat_no_star);

            TestResult::from_bool(direct_contains == our_match)
        }
        QuickCheck::new().quickcheck(inner as fn(RandomString, RandomString) -> TestResult);
    }
}
