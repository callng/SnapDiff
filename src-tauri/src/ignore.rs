use globset::{GlobBuilder, GlobMatcher};
use std::{fs, io::ErrorKind, path::Path};

struct Rule {
    matcher: GlobMatcher,
    negated: bool,
    directory_only: bool,
    basename: bool,
}

pub struct IgnoreMatcher {
    rules: Vec<Rule>,
    pub effective_patterns: Vec<String>,
    pub has_negations: bool,
}

impl IgnoreMatcher {
    pub fn from_root(root: &Path, extra: &[String]) -> Result<Self, String> {
        let mut patterns = match fs::read_to_string(root.join(".snapshotignore")) {
            Ok(text) => text.lines().map(str::to_owned).collect::<Vec<_>>(),
            Err(error) if error.kind() == ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(format!("无法读取 .snapshotignore: {error}")),
        };
        patterns.extend_from_slice(extra);
        // The configuration file itself is never part of a capture.
        patterns.push("/.snapshotignore".into());
        Self::from_patterns(&patterns)
    }

    pub fn from_patterns(patterns: &[String]) -> Result<Self, String> {
        let mut rules = Vec::new();
        let mut effective_patterns = Vec::new();
        for raw in patterns {
            let raw = raw.trim().replace('\\', "/");
            if raw.is_empty() || raw.starts_with('#') {
                continue;
            }
            let negated = raw.starts_with('!');
            let pattern = raw.strip_prefix('!').unwrap_or(&raw);
            let directory_only = pattern.ends_with('/');
            let anchored = pattern.starts_with('/');
            let pattern = pattern.trim_start_matches('/').trim_end_matches('/');
            if pattern.is_empty() {
                return Err("忽略规则不能为空路径".into());
            }
            let basename = !anchored && !pattern.contains('/');
            let matcher = GlobBuilder::new(pattern)
                .literal_separator(true)
                .build()
                .map_err(|e| format!("无效路径规则 {raw}: {e}"))?
                .compile_matcher();
            rules.push(Rule {
                matcher,
                negated,
                directory_only,
                basename,
            });
            effective_patterns.push(raw);
        }
        let has_negations = rules.iter().any(|r| r.negated);
        Ok(Self {
            rules,
            effective_patterns,
            has_negations,
        })
    }

    pub fn is_ignored(&self, relative: &str, is_dir: bool) -> bool {
        if self.rules.is_empty() {
            return false;
        }
        let normalized = relative.replace('\\', "/");
        let segments: Vec<_> = normalized.split('/').collect();
        let mut ignored = false;
        for rule in &self.rules {
            for end in 1..=segments.len() {
                let directory = end < segments.len() || is_dir;
                if rule.directory_only && !directory {
                    continue;
                }
                let candidate = if rule.basename {
                    segments[end - 1].to_string()
                } else {
                    segments[..end].join("/")
                };
                if rule.matcher.is_match(candidate) {
                    ignored = !rule.negated;
                    break;
                }
            }
        }
        ignored
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn patterns_match_nested_files_and_directories_with_ordered_exceptions() {
        let matcher = IgnoreMatcher::from_patterns(&[
            "*.log".into(),
            "cache/".into(),
            "!cache/keep.txt".into(),
            "/temp/".into(),
        ])
        .unwrap();
        assert!(matcher.is_ignored("bin/debug.log", false));
        assert!(matcher.is_ignored("a/cache/shader.bin", false));
        assert!(!matcher.is_ignored("cache/keep.txt", false));
        assert!(matcher.is_ignored("temp/a", false));
        assert!(!matcher.is_ignored("a/temp/a", false));
        assert!(!matcher.is_ignored("cache.bin", false));
    }
    #[test]
    fn root_file_and_extra_rules_are_both_recorded() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join(".snapshotignore"), "# comment\n*.log\n").unwrap();
        let matcher = IgnoreMatcher::from_root(root.path(), &["cache/".into()]).unwrap();
        assert_eq!(
            matcher.effective_patterns,
            ["*.log", "cache/", "/.snapshotignore"]
        );
        assert!(matcher.is_ignored(".snapshotignore", false));
    }

    #[test]
    fn literal_focus_paths_can_include_brackets_and_braces() {
        let matcher =
            IgnoreMatcher::from_patterns(&["/mods/plugin[[]beta[]][{]x[}].dll".into()]).unwrap();
        assert!(matcher.is_ignored("mods/plugin[beta]{x}.dll", false));
        assert!(!matcher.is_ignored("mods/pluginbetax.dll", false));
    }
}
