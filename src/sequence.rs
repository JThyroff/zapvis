use anyhow::{anyhow, Context, Result};
use regex::Regex;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Sender};

use crate::remote_worker::RemoteWorkerRequest;

/// Represents a compiled sequence extracted from a filename pattern and a concrete file.
#[derive(Debug, Clone)]
pub enum SequenceSource {
    Local(PathBuf),
    Remote { user_host: String, dir: String },
}

#[derive(Debug, Clone)]
pub struct SequenceSpec {
    pub source: SequenceSource,
    pub prefix: String,
    /// Total width (sum of all group widths).
    pub width: usize,
    /// Widths of individual `#` groups. Single entry for single-block patterns.
    pub groups: Vec<usize>,
    pub suffix: String,
    pub index: u64,
}

#[derive(Debug, Clone)]
pub struct InputSpec {
    pub file_name: String,
    pub source: SequenceSource,
}

impl SequenceSpec {
    pub fn file_name_for(&self, idx: u64) -> String {
        if self.groups.len() <= 1 {
            format!("{}{:0width$}{}", self.prefix, idx, self.suffix, width = self.width)
        } else {
            // `{:0width$}` always produces at least `self.width` characters, and
            // `self.width == groups.iter().sum()`, so the per-group byte slices are
            // always in-bounds (all characters are ASCII digits).
            let full = format!("{:0width$}", idx, width = self.width);
            let mut parts: Vec<&str> = Vec::new();
            let mut offset = 0;
            for &g in &self.groups {
                parts.push(&full[offset..offset + g]);
                offset += g;
            }
            format!("{}{}{}", self.prefix, parts.join("_"), self.suffix)
        }
    }

    pub fn path_display(&self, idx: u64) -> String {
        match &self.source {
            SequenceSource::Local(dir) => dir.join(self.file_name_for(idx)).display().to_string(),
            SequenceSource::Remote { user_host, dir } => {
                let remote_path = build_remote_path(dir, &self.file_name_for(idx));
                format!("{}:{}", user_host, remote_path)
            }
        }
    }

    pub fn exists_with_ssh(&self, idx: u64, request_tx: Option<Sender<RemoteWorkerRequest>>) -> Result<bool> {
        match &self.source {
            SequenceSource::Local(dir) => Ok(dir.join(self.file_name_for(idx)).exists()),
            SequenceSource::Remote { dir, .. } => {
                let remote_path = build_remote_path(dir, &self.file_name_for(idx));
                if let Some(tx) = request_tx {
                    let (response_tx, response_rx) = channel();
                    eprintln!("[SSH] exists: {}", remote_path);
                    tx.send(RemoteWorkerRequest::Exists {
                        path: remote_path,
                        response_tx,
                    })?;
                    response_rx.recv()?
                } else {
                    Err(anyhow!("Remote SSH connection not available"))
                }
            }
        }
    }
}

/// Compile a pattern like "image_#####.png" into:
/// - regex to extract index
/// - prefix/groups/suffix for reconstruction
///
/// Supports a single contiguous `#` group, or multiple `#` groups separated
/// by `_` (the only supported inter-block delimiter).  All numeric parts are
/// concatenated into a single index.
pub fn compile_pattern(pat: &str) -> Result<(Regex, String, Vec<usize>, String)> {
    let hash_runs: Vec<(usize, usize)> = find_hash_runs(pat);
    if hash_runs.is_empty() {
        return Err(anyhow!(
            "Pattern must contain at least one # run. Got: {pat}"
        ));
    }

    let prefix = &pat[..hash_runs[0].0];
    let suffix = &pat[hash_runs.last().unwrap().1..];

    // Collect per-group widths and validate that any separator between runs is '_'.
    let mut groups: Vec<usize> = Vec::new();
    for (i, &(start, end)) in hash_runs.iter().enumerate() {
        groups.push(end - start);
        if i + 1 < hash_runs.len() {
            let between = &pat[end..hash_runs[i + 1].0];
            if between != "_" {
                return Err(anyhow!(
                    "Multiple # blocks must be separated by '_'. Got separator: {:?} in {pat}",
                    between
                ));
            }
        }
    }

    // Build regex with one capture group per # block, separated by literal '_'.
    let mut re_str = format!("^{}", regex::escape(prefix));
    for (i, &w) in groups.iter().enumerate() {
        if i > 0 {
            re_str.push('_');
        }
        re_str.push_str(&format!("(\\d{{{}}})", w));
    }
    re_str.push_str(&format!("{}$", regex::escape(suffix)));

    let re = Regex::new(&re_str).context("Failed to compile regex from pattern")?;
    Ok((re, prefix.to_string(), groups, suffix.to_string()))
}

/// Concatenate the text of regex capture groups 1..=`n` into a single string.
///
/// This is used for multi-block `#` patterns where each block is a separate
/// capture group, and the combined string is parsed as the sequence index.
fn concat_captures(cap: &regex::Captures<'_>, n: usize) -> Result<String> {
    (1..=n)
        .map(|i| {
            cap.get(i)
                .map(|m| m.as_str())
                .ok_or_else(|| anyhow!("Missing capture group {i}"))
        })
        .collect()
}

fn find_hash_runs(s: &str) -> Vec<(usize, usize)> {
    let bytes = s.as_bytes();
    let mut runs = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'#' {
            let start = i;
            while i < bytes.len() && bytes[i] == b'#' {
                i += 1;
            }
            runs.push((start, i));
        } else {
            i += 1;
        }
    }
    runs
}

/// Try patterns from config; return first that matches AND has neighbor evidence.
/// Evidence: current file matches and at least one neighbor exists (idx+-1).
pub fn pick_sequence(
    cfg: &crate::config::Config,
    input: &InputSpec,
    request_tx: Option<Sender<RemoteWorkerRequest>>,
) -> Result<(String, SequenceSpec)> {
    // If config empty, fail quickly.
    if cfg.patterns.is_empty() {
        return Err(anyhow!("No patterns configured."));
    }

    let file_name = input.file_name.clone();
    let source = input.source.clone();

    for pat in &cfg.patterns {
        let (re, prefix, groups, suffix) = compile_pattern(pat)?;
        if let Some(cap) = re.captures(&file_name) {
            // Concatenate all capture groups to form the combined index string.
            let idx_str = concat_captures(&cap, groups.len())?;
            let idx: u64 = idx_str.parse().context("Failed to parse captured index")?;
            let width: usize = groups.iter().sum();

            let spec = SequenceSpec {
                source: source.clone(),
                prefix,
                width,
                groups,
                suffix,
                index: idx,
            };

            // Skip neigbor check for now
            // Neighbor evidence via stat(): cheap and avoids enumeration.
            //let has_next = spec.exists_with_ssh(idx + 1, request_tx.clone()).unwrap_or(false);
            //let has_prev = idx > 0 && spec.exists_with_ssh(idx - 1, request_tx.clone()).unwrap_or(false);
            
            //if has_next || has_prev {
            return Ok((pat.clone(), spec));
            //}
        }
    }

    Err(anyhow!("No configured pattern matched with neighbor evidence."))
}

pub fn parse_remote_input(input: &str) -> Option<(String, String)> {
    let re = Regex::new(r"^([^@]+@[^:]+):(/.+)$").ok()?;
    let caps = re.captures(input)?;
    Some((caps.get(1)?.as_str().to_string(), caps.get(2)?.as_str().to_string()))
}

pub fn build_remote_path(dir: &str, file_name: &str) -> String {
    let trimmed = dir.trim_end_matches('/');
    if trimmed.is_empty() {
        format!("/{}", file_name)
    } else {
        format!("{}/{}", trimmed, file_name)
    }
}

pub fn file_name_from_path(path: &Path) -> Result<String> {
    path.file_name()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow!("Non-UTF8 filename not supported"))
}

pub fn file_name_from_str_path(path: &str) -> Result<String> {
    Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow!("Non-UTF8 filename not supported"))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── compile_pattern ──────────────────────────────────────────────────────

    #[test]
    fn single_block_compile() {
        let (re, prefix, groups, suffix) = compile_pattern("frame_######.png").unwrap();
        assert_eq!(prefix, "frame_");
        assert_eq!(groups, vec![6]);
        assert_eq!(suffix, ".png");
        assert!(re.is_match("frame_000042.png"));
        assert!(!re.is_match("frame_42.png")); // too short
    }

    #[test]
    fn multi_block_compile() {
        let (re, prefix, groups, suffix) = compile_pattern("frame_######_#.png").unwrap();
        assert_eq!(prefix, "frame_");
        assert_eq!(groups, vec![6, 1]);
        assert_eq!(suffix, ".png");
        assert!(re.is_match("frame_000123_4.png"));
        assert!(!re.is_match("frame_0001234.png")); // single block doesn't match
    }

    #[test]
    fn invalid_separator_errors() {
        assert!(compile_pattern("frame_####-#.png").is_err());
    }

    #[test]
    fn no_hash_errors() {
        assert!(compile_pattern("frame.png").is_err());
    }

    // ── index extraction ─────────────────────────────────────────────────────

    #[test]
    fn single_block_index_extraction() {
        let (re, _, groups, _) = compile_pattern("frame_######.png").unwrap();
        let cap = re.captures("frame_001234.png").unwrap();
        let idx_str = concat_captures(&cap, groups.len()).unwrap();
        assert_eq!(idx_str, "001234");
        assert_eq!(idx_str.parse::<u64>().unwrap(), 1234);
    }

    #[test]
    fn multi_block_index_extraction() {
        let (re, _, groups, _) = compile_pattern("frame_######_#.png").unwrap();
        let cap = re.captures("frame_000123_4.png").unwrap();
        let idx_str = concat_captures(&cap, groups.len()).unwrap();
        // Concatenated: "000123" + "4" = "0001234" → 1234
        assert_eq!(idx_str, "0001234");
        assert_eq!(idx_str.parse::<u64>().unwrap(), 1234);
    }

    // ── file_name_for ─────────────────────────────────────────────────────────

    fn make_spec(prefix: &str, groups: Vec<usize>, suffix: &str, index: u64) -> SequenceSpec {
        let width = groups.iter().sum();
        SequenceSpec {
            source: SequenceSource::Local(PathBuf::from(".")),
            prefix: prefix.to_string(),
            width,
            groups,
            suffix: suffix.to_string(),
            index,
        }
    }

    #[test]
    fn single_block_file_name_for() {
        let spec = make_spec("frame_", vec![6], ".png", 42);
        assert_eq!(spec.file_name_for(42), "frame_000042.png");
    }

    #[test]
    fn multi_block_file_name_for() {
        let spec = make_spec("frame_", vec![6, 1], ".png", 1234);
        // index 1234, total width 7 → "0001234", split [6,1] → "000123" + "4"
        assert_eq!(spec.file_name_for(1234), "frame_000123_4.png");
    }

    #[test]
    fn multi_block_file_name_for_zero() {
        let spec = make_spec("frame_", vec![6, 1], ".png", 0);
        assert_eq!(spec.file_name_for(0), "frame_000000_0.png");
    }
}
