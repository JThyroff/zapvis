mod app;
mod cli;
mod config;
mod image_cache;
mod image_util;
mod remote_worker;
mod sequence;

use anyhow::{anyhow, Result};
use clap::Parser;
use std::fs;
use crate::app::ZapVisApp;
use crate::cli::Args;
use crate::config::{config_path, load_config, maybe_add_pattern, pattern_matches_file, save_config};
use crate::remote_worker::{RemoteRange, spawn_remote_worker};
use crate::sequence::{
    file_name_from_path, file_name_from_str_path, parse_remote_input, pick_sequence, InputSpec,
    SequenceSource,
};
use zapvis::PersistentSsh;

fn main() -> Result<()> {
    let args = Args::parse();

    // Handle --config flag
    if args.config {
        let path = config_path()?;
        println!("Config path: {}", path.display());
        if path.exists() {
            let content = fs::read_to_string(&path)
                .context("Failed to read config file")?;
            println!("\nConfig content:\n{}", content);
        } else {
            println!("Config file does not exist.");
        }
        return Ok(());
    }

    // Input is required if not showing config
    let input = args
        .input
        .ok_or_else(|| anyhow!("Input file is required (unless using --config flag)"))?;
    let input_spec = if let Some((user_host, remote_path)) = parse_remote_input(&input) {
        let file_name = file_name_from_str_path(&remote_path)?;
        let dir = std::path::Path::new(&remote_path)
            .parent()
            .ok_or_else(|| anyhow!("Remote input has no parent directory"))?
            .to_string_lossy()
            .to_string();
        InputSpec {
            file_name,
            source: SequenceSource::Remote { user_host, dir },
        }
    } else {
        let input = std::path::PathBuf::from(&input);
        if !input.is_file() {
            return Err(anyhow!(
                "Input must be an image FILE path. Folder mode is intentionally not supported."
            ));
        }
        let file_name = file_name_from_path(&input)?;
        let dir = input
            .parent()
            .ok_or_else(|| anyhow!("Input has no parent directory"))?
            .to_path_buf();
        InputSpec {
            file_name,
            source: SequenceSource::Local(dir),
        }
    };

    let mut cfg = load_config().unwrap_or_default();

    // If user provided --pattern, try it first and store it if it works.
    if let Some(pat) = args.pattern.clone() {
        if pattern_matches_file(&pat, &input_spec.file_name)? {
            maybe_add_pattern(&mut cfg, pat);
            save_config(&cfg).ok(); // ignore save errors (still can run)
        } else {
            return Err(anyhow!(
                "Provided --pattern did not match the input filename. Nothing saved."
            ));
        }
    }

    // Establish persistent SSH early if remote (and spawn worker thread)
    let remote_range = RemoteRange::new();
    let remote_worker_tx = match &input_spec.source {
        SequenceSource::Remote { user_host, .. } => {
            match PersistentSsh::connect(user_host) {
                Ok(ssh) => {
                    eprintln!("[SSH] Connected to {}", user_host);
                    Some(spawn_remote_worker(ssh, remote_range.clone()))
                }
                Err(e) => {
                    eprintln!("Failed to establish persistent SSH: {}", e);
                    None
                }
            }
        }
        SequenceSource::Local(_) => None,
    };

    // Determine which pattern to use:
    let (pattern, seq) = match pick_sequence(&cfg, &input_spec, remote_worker_tx.clone()) {
        Ok(v) => v,
        Err(e) => {
            // Your rule: if no hits, quit. (No interactive prompt here.)
            eprintln!("{e}");
            eprintln!("\nKnown patterns in config:");
            for (i, p) in cfg.patterns.iter().enumerate() {
                eprintln!("  {}) {}", i + 1, p);
            }
            eprintln!("\nTip: run with --pattern \"########_#.png\" to add/try a new one.");
            return Err(anyhow!("No sequence pattern matched. Quitting."));
        }
    };

    let native_options = eframe::NativeOptions::default();
    eframe::run_native(
        "zapvis",
        native_options,
        Box::new(|cc| Ok(Box::new(ZapVisApp::new(cc, pattern, seq, remote_worker_tx, remote_range)))),
    )
    .map_err(|e| anyhow!(e.to_string()))?;

    Ok(())
}
