//! Vendor patch ledger checker: verify `vendor/README.md` against the vendored
//! sources.
//!
//! The vendor ledger records, per patched crate: version, numbered patches, and
//! the regression test that proves each patch. The vendored crates are excluded
//! from the workspace, so nothing else keeps the ledger honest: a test can be
//! renamed, a patch note rewritten, or a version bumped with the ledger left
//! stale, and CI would stay green.
//!
//! Checks (fatal unless noted):
//!
//! 1. *version* — every crate row's Version matches `vendor/<crate>/Cargo.toml`
//! 2. *cited test* — every backticked test citation resolves to a real test fn,
//!    test file, or module SOMEWHERE in `vendor/*`, `apps/pdfcraft`, or `crates/` — a row
//!    may cite a proof test that lives in a sibling vendored crate (a
//!    hayro-syntax patch is proven by hayro's raster tests) or in the app's own
//!    test suite
//! 3. *coverage* — every crate in `[patch.crates-io]` appears in the ledger
//! 4. *markers* — (warning) the ledger's highest patch number is backed by at
//!    least as many `pdfcraft patch` markers in the crate's source
//!
//! Citations resolve by their FINAL segment only: a test fn or a module the
//! citation itself names. The parent path is context, not proof — `mod tests`
//! exists in almost every crate, so accepting a parent module let
//! `raster::tests::deleted_fn` pass after the test was deleted.
//!
//! Usage: `cargo xtask vendor-ledger [--list]` (`--list` also prints every
//! citation checked). Exits non-zero on fatal findings.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Every Rust source of one vendored crate (or the app), concatenated once.
fn crate_text(root: &Path, crate_name: &str) -> String {
    let base = if crate_name == "pdfcraft" { root.join("apps/pdfcraft") } else { root.join("vendor").join(crate_name) };
    read_all_rs(&base)
}

/// Recursively concatenate every `.rs` file under `base` (read errors ignored,
/// as the sources may be mid-write during an agent run).
fn read_all_rs(base: &Path) -> String {
    let mut parts = Vec::new();
    collect_rs(base, &mut parts);
    parts.sort();
    let mut out = String::new();
    for path in parts {
        if let Ok(text) = fs::read_to_string(&path) {
            out.push_str(&text);
            out.push('\n');
        }
    }
    out
}

/// Collect Rust file paths below a source directory, ignoring unreadable entries.
fn collect_rs(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rs(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

/// `(test fn names, module names, file basenames)` across `vendor/*`, `apps/`
/// and `crates/`.
struct Index {
    fns: BTreeSet<String>,
    mods: BTreeSet<String>,
    files: BTreeSet<String>,
}

/// Build the process-local name index from every source tree cited by the ledger.
fn build_index(root: &Path) -> Index {
    let mut bases: Vec<PathBuf> = Vec::new();
    if let Ok(vendor) = fs::read_dir(root.join("vendor")) {
        let mut crates: Vec<PathBuf> = vendor.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
        crates.sort();
        bases.extend(crates);
    }
    bases.push(root.join("apps/pdfcraft"));
    bases.push(root.join("crates"));

    let mut index = Index { fns: BTreeSet::new(), mods: BTreeSet::new(), files: BTreeSet::new() };
    for base in bases {
        if !base.is_dir() {
            continue;
        }
        let mut files = Vec::new();
        collect_rs(&base, &mut files);
        for path in files {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                index.files.insert(name.to_owned());
            }
            let Ok(text) = fs::read_to_string(&path) else { continue };
            for ident in identifiers_after_keywords(&text, &["fn"]) {
                index.fns.insert(ident.ident);
            }
            for ident in identifiers_after_keywords(&text, &["mod"]) {
                index.mods.insert(ident.ident);
            }
        }
    }
    index
}

struct Ident {
    ident: String,
}

/// Finds `fn name(` / `mod name` occurrences. `fn` requires an opening paren
/// (a fn pointer type or a bare `fn` mention is not a definition); `mod` is
/// matched bare, which also admits `mod tests` inside `cfg` attributes — the
/// Python checker's `\bmod\s+(ident)\b` did the same.
fn identifiers_after_keywords(text: &str, keywords: &[&str]) -> Vec<Ident> {
    let mut out = Vec::new();
    for keyword in keywords {
        let mut rest = text;
        while let Some(pos) = find_keyword(rest, keyword) {
            let Some(after) = rest.get(pos..).and_then(|tail| tail.strip_prefix(keyword)) else { break };
            let trimmed = after.trim_start();
            if let Some(ident) = next_ident(trimmed) {
                let valid = *keyword != "fn" || trimmed.get(ident.len()..).is_some_and(after_ident_paren);
                if valid {
                    out.push(Ident { ident: ident.to_owned() });
                }
            }
            rest = after;
        }
    }
    out
}

/// Find an ASCII keyword outside a longer ASCII identifier, returning a byte boundary.
fn find_keyword(haystack: &str, keyword: &str) -> Option<usize> {
    if keyword.is_empty() {
        return None;
    }
    let mut from = 0usize;
    while let Some(rel) = haystack.get(from..)?.find(keyword) {
        let pos = from.checked_add(rel)?;
        let before_ok = pos == 0 || haystack.as_bytes().get(pos - 1).is_none_or(|b| !is_ident_byte(*b));
        let after = pos.checked_add(keyword.len())?;
        let after_ok = haystack.as_bytes().get(after).is_none_or(|b| !is_ident_byte(*b));
        if before_ok && after_ok {
            return Some(pos);
        }
        from = after;
    }
    None
}

/// Whether a byte can continue an ASCII Rust identifier.
fn is_ident_byte(b: u8) -> bool {
    b == b'_' || b.is_ascii_alphanumeric()
}

/// Read an ASCII identifier at the start, without skipping punctuation or prose.
fn next_ident(s: &str) -> Option<&str> {
    let first = *s.as_bytes().first()?;
    if first != b'_' && !first.is_ascii_alphabetic() {
        return None;
    }
    let len = s.bytes().take_while(|b| is_ident_byte(*b)).count();
    s.get(..len)
}

/// Require `(` immediately after a name, allowing whitespace and balanced generics.
/// The generic scan is bounded to 64 characters, as in the original lightweight scan.
fn after_ident_paren(after: &str) -> bool {
    let rest = after.trim_start();
    if rest.starts_with('(') {
        return true;
    }
    if !rest.starts_with('<') {
        return false;
    }
    let mut depth = 0usize;
    for (pos, ch) in rest.char_indices().take(64) {
        match ch {
            '<' => depth = depth.saturating_add(1),
            // A return-type arrow in `F: Fn() -> T` does not close generics.
            '>' if rest.get(..pos).is_some_and(|prefix| prefix.ends_with('-')) => {}
            '>' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return rest.get(pos.saturating_add(ch.len_utf8())..).is_some_and(|tail| tail.trim_start().starts_with('('));
                }
            }
            _ => {}
        }
    }
    false
}

/// A backticked token from the ledger: `ok | missing | skip`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Ok,
    Missing,
    Skip,
}

/// Identify CamelCase type paths, which are prose rather than test citations.
fn is_upper_first(ident: &str) -> bool {
    ident.chars().next().is_some_and(|c| c.is_ascii_uppercase())
}

/// Resolve a file citation by basename across the indexed source trees.
fn file_exists(name: &str, index: &Index) -> bool {
    let base = name.rsplit('/').next().unwrap_or(name);
    index.files.contains(base)
}

/// Classification of one backticked citation (final-segment resolution).
fn classify(cite: &str, index: &Index) -> Verdict {
    let cite = cite.trim();
    // prose, command, URL: anything with whitespace, a command prefix, a flag,
    // or a URL scheme is not a symbol citation.
    if cite.is_empty()
        || cite.chars().any(char::is_whitespace)
        || cite.starts_with("--")
        || cite.starts_with("cargo ")
        || cite.starts_with("rustc ")
        || cite.starts_with("http://")
        || cite.starts_with("https://")
    {
        return Verdict::Skip;
    }
    let base = cite.split("::").next().unwrap_or(cite);
    if base.ends_with(".rs") {
        if !file_exists(base, index) {
            return Verdict::Missing;
        }
        if !cite.contains("::") {
            return Verdict::Ok;
        }
    }
    if cite.contains("::") {
        let last = cite.rsplit("::").next().unwrap_or(cite);
        if is_upper_first(last) {
            return Verdict::Skip; // a type path from prose (e.g. WgpuError::CreateRenderer)
        }
        if index.fns.contains(last) || index.mods.contains(last) {
            return Verdict::Ok;
        }
        return Verdict::Missing;
    }
    if index.fns.contains(cite) || file_exists(cite, index) {
        return Verdict::Ok;
    }
    Verdict::Skip // a bare word that is not a test — prose
}

/// A bare snake_case name in the Test column refers to a function or module.
fn classify_test(cite: &str, index: &Index) -> Verdict {
    let cite = cite.trim();
    let verdict = classify(cite, index);
    if verdict == Verdict::Skip
        && cite.starts_with(|c: char| c.is_ascii_lowercase())
        && cite.contains('_')
        && cite.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    {
        return if index.mods.contains(cite) { Verdict::Ok } else { Verdict::Missing };
    }
    verdict
}

/// `name = { path = "vendor/<dir>"` entries of `[patch.crates-io]`:
/// (crate name, vendored directory).
fn patched_crates(root: &Path) -> Result<BTreeMap<String, String>> {
    let cargo = fs::read_to_string(root.join("Cargo.toml")).context("read root Cargo.toml")?;
    let mut patched = BTreeMap::new();
    let mut in_patch_section = false;
    for line in cargo.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_patch_section = trimmed == "[patch.crates-io]";
            continue;
        }
        if !in_patch_section || trimmed.starts_with('#') || trimmed.is_empty() {
            continue;
        }
        let Some((name, rest)) = trimmed.split_once('=') else { continue };
        let name = name.trim().to_owned();
        let Some(dir_start) = rest.find("vendor/") else { continue };
        let after = &rest[dir_start + "vendor/".len()..];
        let dir: String = after.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect();
        if dir.is_empty() {
            continue;
        }
        patched.insert(name, dir);
    }
    Ok(patched)
}

/// The manifest version of a vendored crate.
fn manifest_version(manifest: &Path) -> Option<String> {
    let text = fs::read_to_string(manifest).ok()?;
    let mut in_package = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_package = trimmed == "[package]";
            continue;
        }
        if !in_package {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("version") {
            let rest = rest.trim_start();
            if let Some(value) = rest.strip_prefix('=') {
                let value = value.trim().trim_matches('"').trim();
                return Some(value.to_owned());
            }
        }
    }
    None
}

/// One parsed ledger table row: (crate, version, patches cell, tests cell).
struct Row {
    crate_name: String,
    version: String,
    patches: String,
    tests: String,
}

/// Markdown table rows with at least five cells. The header row is filtered
/// later by the patched-crate membership, exactly as the Python checker did.
fn ledger_rows(ledger: &str) -> Vec<Row> {
    let mut rows = Vec::new();
    for line in ledger.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with('|') {
            continue;
        }
        let cells: Vec<&str> = trimmed.split('|').collect();
        // `| a | b | c | d | e |` splits into ["", a, b, c, d, e, ""] — seven
        // elements for a five-column row. Shorter lines (a stray
        // `|`-prefixed fragment inside a fenced block) are not rows.
        if cells.len() < 7 {
            continue;
        }
        let inner = &cells[1..cells.len() - 1];
        if inner.len() < 4 {
            continue;
        }
        if inner.iter().all(|c| {
            let c = c.trim();
            c.is_empty() || c.chars().all(|ch| ch == '-' || ch == ':' || ch == ' ')
        }) {
            continue; // separator row
        }
        let crate_cell = inner[0].trim();
        // `[a-z0-9_-]+`, as the Python checker's row regex required.
        if crate_cell.is_empty() || !crate_cell.chars().all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-' || ch == '_') {
            continue;
        }
        rows.push(Row {
            crate_name: crate_cell.to_owned(),
            version: inner[1].trim().trim_matches('`').trim().to_owned(),
            patches: inner[3].trim().to_owned(),
            // The tests group runs from the fifth cell to the END of the
            // line, absorbing any further pipes (the Python checker's
            // `(.*?)\s*\|\s*$` anchored the last group at the final pipe).
            // Four-column rows are not parsed: patches and test citations
            // must be kept in separate cells when consolidating old entries.
            tests: inner[4..].join("|").trim().to_owned(),
        });
    }
    rows
}

/// Backticked spans of a text (the contents, in order).
fn backticks(text: &str) -> Vec<&str> {
    let mut cites = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find('`') {
        let after = &rest[open + 1..];
        match after.find('`') {
            Some(close) => {
                cites.push(&after[..close]);
                rest = &after[close + 1..];
            }
            None => break,
        }
    }
    cites
}

/// The highest explicit `(n)` patch number (1..=500), including numbers after gaps.
/// Parentheses must contain only digits: `(4 GiB)` is prose, not patch four.
fn highest_patch_number(patches_cell: &str) -> usize {
    let mut top = 0usize;
    let mut rest = patches_cell;
    while let Some(open) = rest.find('(') {
        let Some(after) = rest.get(open.saturating_add(1)..) else { break };
        let digits = after.bytes().take_while(u8::is_ascii_digit).count();
        if after.get(digits..).is_some_and(|tail| tail.starts_with(')'))
            && let Some(number) = after.get(..digits).and_then(|s| s.parse::<usize>().ok()).filter(|n| (1..=500).contains(n))
        {
            top = top.max(number);
        }
        rest = after;
    }
    top
}

/// `cargo xtask vendor-ledger [--list]`.
pub fn run(args: &[String]) -> Result<()> {
    let show_all = args.iter().any(|a| a == "--list");
    let root = crate::gates::root();
    let ledger_path = root.join("vendor/README.md");
    let ledger = fs::read_to_string(&ledger_path).context("read vendor/README.md")?;
    let patched = patched_crates(&root)?;
    let index = build_index(&root);

    let mut fatal: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut checked = 0usize;
    let mut highest_by_crate = BTreeMap::<String, usize>::new();

    for row in ledger_rows(&ledger) {
        let Some(dir) = patched.get(&row.crate_name) else { continue };
        let manifest = root.join("vendor").join(dir).join("Cargo.toml");
        if manifest.is_file() {
            match manifest_version(&manifest) {
                Some(got) if got != row.version => {
                    fatal.push(format!("version drift: ledger says {} {}, manifest says {}", row.crate_name, row.version, got));
                }
                None => fatal.push(format!("no [package] version found in {}", manifest.display())),
                Some(_) => {}
            }
        }
        for cite in backticks(&row.tests) {
            let verdict = classify_test(cite, &index);
            checked += 1;
            if show_all {
                println!("[{:?}] {}: {}", verdict, row.crate_name, cite);
            }
            if verdict == Verdict::Missing {
                fatal.push(format!("cited test not found: {}: `{}`", row.crate_name, cite));
            }
        }
        let top = highest_patch_number(&row.patches);
        let highest = highest_by_crate.entry(row.crate_name).or_default();
        *highest = (*highest).max(top);
    }

    // Prose around the table also cites tests; scan every non-table line so
    // notes above the first heading are checked too (table cells were already
    // checked per row above).
    let prose: String = ledger.lines().filter(|l| !l.trim_start().starts_with('|')).collect::<Vec<_>>().join("\n");
    for cite in backticks(&prose) {
        if classify(cite, &index) == Verdict::Missing {
            fatal.push(format!("cited test not found (prose): `{cite}`"));
        }
    }

    for (crate_name, dir) in &patched {
        if let Some(&top) = highest_by_crate.get(crate_name).filter(|&&n| n > 0) {
            let markers = crate_text(&root, dir).to_lowercase().matches("pdfcraft patch").count();
            if markers < top {
                warnings.push(format!("{crate_name}: highest ledger patch is {top}, but the source has only {markers} 'PdfCraft patch' markers"));
            }
        }

        let has_row = ledger.lines().any(|l| {
            let t = l.trim_start();
            t.starts_with('|') && {
                let first = t.split('|').nth(1).map(str::trim);
                first.is_some_and(|c| c == crate_name)
            }
        });
        if !has_row {
            fatal.push(format!("crate {crate_name} is in [patch.crates-io] but absent from the vendor ledger"));
        }
    }

    for w in &warnings {
        println!("warning: {w}");
    }
    for f in &fatal {
        println!("error: {f}");
    }
    println!("vendor ledger: {checked} citations checked, {} fatal, {} warning(s)", fatal.len(), warnings.len());
    if fatal.is_empty() { Ok(()) } else { Err(anyhow::anyhow!("vendor ledger check failed with {} fatal finding(s)", fatal.len())) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake index: two fns (one under tests), the ubiquitous `tests` module,
    /// one real submodule, one file — and deliberately NO `deleted_regression`.
    fn fake_index() -> Index {
        Index {
            fns: BTreeSet::from(["replacement".to_owned(), "offsets_survive_a_roundtrip".to_owned()]),
            mods: BTreeSet::from(["tests".to_owned(), "raster".to_owned()]),
            files: BTreeSet::from(["lib.rs".to_owned(), "raster.rs".to_owned()]),
        }
    }

    #[test]
    fn bare_test_names_are_required_only_in_the_test_column() {
        let mut index = fake_index();
        index.mods.insert("existing_tests".into());
        assert_eq!(classify_test("offsets_survive_a_roundtrip", &index), Verdict::Ok);
        assert_eq!(classify_test("existing_tests", &index), Verdict::Ok);
        assert_eq!(classify_test("missing_review_regression", &index), Verdict::Missing);
        assert_eq!(classify("missing_review_regression", &index), Verdict::Skip);
        assert_eq!(classify_test("cargo test -p a_crate", &index), Verdict::Skip);
    }

    #[test]
    fn citation_classifier_matches_the_python_semantics() {
        let index = fake_index();
        let cases: &[(&str, &str, &str)] = &[
            ("ok", "tests::replacement", "the cited test fn exists"),
            ("ok", "raster::tests::replacement", "final fn exists under its module path"),
            ("ok", "raster::tests", "a module the citation itself names"),
            ("ok", "raster.rs", "a file citation"),
            ("ok", "offsets_survive_a_roundtrip", "a bare fn name"),
            ("skip", "cargo test -p foo", "a command, not a citation"),
            ("skip", "https://example.com/a", "a URL"),
            ("skip", "some prose word", "a bare word that is not a symbol"),
            // the negative cases the parent-module fallback used to accept
            ("missing", "tests::deleted_regression", "parent module exists, final fn does not"),
            ("missing", "raster::tests::deleted_fn", "deep path, final fn deleted after a rename"),
            ("missing", "absent_module::deleted_regression", "nothing on the path exists"),
            ("missing", "nope.rs", "a file that does not exist"),
            ("missing", "raster::tests::replacement::typos", "a typo past a real prefix"),
            ("skip", "WgpuError::CreateRenderer", "a CamelCase type path quoted in prose"),
        ];
        for (expected, cite, why) in cases {
            let verdict = classify(cite, &index);
            let expected = match *expected {
                "ok" => Verdict::Ok,
                "missing" => Verdict::Missing,
                _ => Verdict::Skip,
            };
            assert_eq!(verdict, expected, "cite {cite:?} ({why})");
        }
    }

    #[test]
    fn backtick_extraction_handles_adjacent_spans() {
        assert_eq!(backticks("`a` and `b`"), vec!["a", "b"]);
        assert_eq!(backticks("no ticks"), Vec::<&str>::new());
        assert_eq!(backticks("unterminated `tick"), Vec::<&str>::new());
    }

    #[test]
    fn highest_patch_number_ignores_prose_numbers() {
        // Later patches still count, but a prose size is not a patch number.
        assert_eq!(highest_patch_number("(1) x; (2) y; (3) z; (5) w (4 GiB)"), 5);
        assert_eq!(highest_patch_number("(1) (4 GiB)"), 1);
        assert_eq!(highest_patch_number("(1) (2) (3)"), 3);
        assert_eq!(highest_patch_number("no numbers"), 0);
    }

    #[test]
    fn patched_crates_parses_the_patch_table() {
        let dir = tempfile_dir();
        let cargo = r#"
[package]
name = "pdfcraft"

[patch.crates-io]
lopdf = { path = "vendor/lopdf" }
hayro-syntax = { path = "vendor/hayro-syntax" }

[patch.crates-io.other]
ignored = { path = "vendor/ignored" }
"#;
        std::fs::write(dir.path().join("Cargo.toml"), cargo).unwrap();
        let patched = patched_crates(dir.path()).unwrap();
        assert_eq!(patched.get("lopdf").map(String::as_str), Some("lopdf"));
        assert_eq!(patched.get("hayro-syntax").map(String::as_str), Some("hayro-syntax"));
        assert!(!patched.contains_key("ignored"), "a non crates-io patch section must not leak in");
    }

    #[test]
    fn ledger_rows_skip_separators_and_short_tables() {
        // The header row parses like any row and is filtered later by
        // patched-crate membership — exactly as the Python checker did
        // (`if crate not in patched: continue`).
        let rows = ledger_rows(
            "| crate | version | licence | patches | tests |\n|---|---|---|---|---|\n| lopdf | 0.45.0 | MIT | (1) x | `a_test` |\n| two-col | row |\n",
        );
        assert_eq!(rows.len(), 2, "separator rows and short tables are dropped; the header parses");
        assert_eq!(rows[1].crate_name, "lopdf");
        assert_eq!(rows[1].version, "0.45.0");
        assert_eq!(rows[1].patches, "(1) x");
        assert_eq!(rows[1].tests, "`a_test`");
    }

    #[test]
    fn function_scan_handles_unicode_prose_without_panicking() {
        let dir = tempfile_dir();
        let source = dir.path().join("crates/example/src");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("lib.rs"), "// fn — the parser\nfn replacement() {}\n").unwrap();
        let index = build_index(dir.path());
        assert_eq!(index.fns, BTreeSet::from(["replacement".to_owned()]));
    }

    #[test]
    fn file_qualified_citation_requires_its_final_symbol() {
        let index = fake_index();
        assert_eq!(classify("lib.rs::tests::deleted_fn", &index), Verdict::Missing);
        assert_eq!(classify("lib.rs::tests::replacement", &index), Verdict::Ok);
        assert_eq!(classify("lib.rs::tests", &index), Verdict::Ok);
        assert_eq!(classify("lib.rs", &index), Verdict::Ok);
        assert_eq!(classify("missing.rs::tests::replacement", &index), Verdict::Missing);
    }

    #[test]
    fn patch_numbers_after_a_gap_still_count() {
        assert_eq!(highest_patch_number("(1) first; (3) later (4 GiB)"), 3);
        let patches = (1..=34).map(|n| format!("({n}) patch; ")).collect::<String>();
        assert_eq!(highest_patch_number(&format!("{patches}(41) x; (42) y; (43) z")), 43);
    }

    #[test]
    fn function_scan_rejects_pointers_and_prose_but_keeps_definitions() {
        let source = "type F = fn(usize) -> u8; // the fn handles things (see x)\n\
                      fn replacement() {}\nfn generic<T: Into<Vec<u8>>>(value: T) {}\n\
                      fn callback<F: Fn(usize) -> u8>(f: F) {}\n\
                      fn _private () {}\nfn 1invalid() {}\nfn broken<T> prose () {}";
        let names: Vec<_> = identifiers_after_keywords(source, &["fn"]).into_iter().map(|i| i.ident).collect();
        assert_eq!(names, ["replacement", "generic", "callback", "_private"]);
        assert!(identifiers_after_keywords(source, &[""]).is_empty());
    }

    #[test]
    fn function_scan_handles_multibyte_punctuation_and_whitespace() {
        for prefix in ["—", "é", "中", "🦀", "(usize) ->", "123"] {
            let source = format!("fn {prefix} ignored ();\nfn\u{2003}replacement() {{}}");
            let names: Vec<_> = identifiers_after_keywords(&source, &["fn"]).into_iter().map(|i| i.ident).collect();
            assert_eq!(names, ["replacement"], "prefix {prefix}");
        }
    }

    #[test]
    fn patch_number_parsing_rejects_prose_overflow_and_out_of_range_values() {
        assert_eq!(highest_patch_number("(500) (0) (501) (9999999999999999999999999999)"), 500);
        assert_eq!(highest_patch_number("(4 GiB) (42x) (43 (44)"), 44);
        assert_eq!(highest_patch_number("— (３) (1) (1)"), 1);
        assert_eq!(highest_patch_number("() (4 GiB) (43"), 0);
    }

    #[test]
    fn hayro_syntax_ledger_keeps_patches_15_through_21_and_their_proofs() {
        let root = crate::gates::root();
        let ledger = std::fs::read_to_string(root.join("vendor/README.md")).unwrap();
        let rows = ledger_rows(&ledger);
        let index = build_index(&root);
        let proofs = [
            (15, "content::tests::windows_keep_every_instruction_at_every_split"),
            (16, "crypto::tests::aes256_passwords_open_as_typed_and_in_their_saslprep_form"),
            (17, "raster::tests::group4_fax_with_rows_zero_decodes_to_the_image_height"),
            (18, "filter::lzw_flate::tests::back_reference_longer_than_its_distance_repeats_its_source"),
            (19, "raster::tests::group4_fax_with_rows_below_its_height_decodes_to_the_image_height"),
            (20, "raster::tests::space_before_the_line_break_after_stream_still_renders"),
            (21, "raster::tests::a_jp2_image_in_a_refused_enumerated_colour_space_is_drawn"),
        ];
        for (number, proof) in proofs {
            assert!(
                rows.iter().any(|row| row.crate_name == "hayro-syntax" && row.patches.contains(&format!("({number}) ")) && row.tests.contains(proof)),
                "patch {number} must keep its description and proof"
            );
            assert_eq!(classify(proof, &index), Verdict::Ok, "patch {number}: {proof}");
        }
    }

    // -- helpers ---------------------------------------------------------

    /// A throwaway directory (no external dep: a name-collision-prone tempdir
    /// under the system temp is fine for a test that cleans up after itself).
    fn tempfile_dir() -> tempfile_guard::TempDir {
        tempfile_guard::TempDir::new()
    }

    /// Minimal scoped temp directory so the test has no dependency beyond std.
    mod tempfile_guard {
        use std::path::PathBuf;
        use std::sync::atomic::{AtomicU64, Ordering};

        static COUNTER: AtomicU64 = AtomicU64::new(0);

        pub struct TempDir(PathBuf);
        impl TempDir {
            pub fn new() -> Self {
                let n = COUNTER.fetch_add(1, Ordering::Relaxed);
                let dir = std::env::temp_dir().join(format!("pdfcraft-xtask-vendor-ledger-{}-{n}", std::process::id()));
                std::fs::create_dir_all(&dir).unwrap();
                Self(dir)
            }
            pub fn path(&self) -> &std::path::Path {
                &self.0
            }
        }
        impl Drop for TempDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }
}
