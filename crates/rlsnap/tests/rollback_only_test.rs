//! Safety invariant: the only transaction terminator this crate's source
//! ever issues is `ROLLBACK`. This test greps `src/` for the literal string
//! `COMMIT` and requires every occurrence to be inside a comment line (i.e.
//! never part of a string literal the code could send to Postgres).

use std::path::Path;

fn rs_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.is_dir() {
            rs_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn commit_never_appears_outside_a_comment() {
    let src_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rs_files(&src_dir, &mut files);
    assert!(!files.is_empty(), "expected to find .rs files under src/");

    for file in files {
        let text = std::fs::read_to_string(&file).unwrap();
        for (lineno, line) in text.lines().enumerate() {
            if line.contains("COMMIT") {
                let trimmed = line.trim_start();
                assert!(
                    trimmed.starts_with("//"),
                    "found \"COMMIT\" outside a comment at {}:{}: {line}",
                    file.display(),
                    lineno + 1
                );
            }
        }
    }
}
