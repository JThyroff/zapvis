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
    pub width: usize,
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
        format!("{}{:0width$}{}", self.prefix, idx, self.suffix, width = self.width)
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
/// - prefix/width/suffix for reconstruction
///
/// MVP limitation: supports exactly ONE contiguous # group.
pub fn compile_pattern(pat: &str) -> Result<(Regex, String, usize, String)> {
    let hash_runs: Vec<(usize, usize)> = find_hash_runs(pat);
    if hash_runs.len() != 1 {
        return Err(anyhow!(
            "Pattern must contain exactly ONE contiguous # run (for now). Got: {pat}"
        ));
    }
    let (start, end) = hash_runs[0];
    let prefix = &pat[..start];
    let suffix = &pat[end..];
    let width = end - start;

    // Build regex: escape prefix/suffix, capture digits of exact width
    let re_str = format!(
        "^{}(\\d{{{}}}){}$",
        regex::escape(prefix),
        width,
        regex::escape(suffix)
    );
    let re = Regex::new(&re_str).context("Failed to compile regex from pattern")?;
    Ok((re, prefix.to_string(), width, suffix.to_string()))
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
        let (re, prefix, width, suffix) = compile_pattern(pat)?;
        if let Some(cap) = re.captures(&file_name) {
            let idx_str = cap.get(1).unwrap().as_str();
            let idx: u64 = idx_str.parse().context("Failed to parse captured index")?;

            let spec = SequenceSpec {
                source: source.clone(),
                prefix,
                width,
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
