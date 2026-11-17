use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use clap::{Parser, ValueEnum};

use cs2_voice_extractor::demo;
use cs2_voice_extractor::voice::{self, Layout, Timeline, Track};

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Mode {
    /// One file per player, spanning the whole demo (voice at its real time, silence elsewhere).
    #[value(alias = "split-full")]
    Full,
    /// One file per player with only the voice, silence removed.
    #[value(alias = "split-compact", alias = "remove-empty")]
    Compact,
    /// One file with all selected players mixed, spanning the whole demo (needs ffmpeg).
    #[value(alias = "single-full", alias = "merged")]
    Merge,
}

/// Extracts player voice chat from CS2 demos without parsing game state.
///
/// Files are written as Ogg Opus by remuxing the original voice packets (no
/// re-encoding, no native codecs). Other formats (wav, mp3, flac, ...) are produced
/// by piping through ffmpeg.
#[derive(Debug, Parser)]
#[command(version)]
struct Cli {
    /// CS2 demo file(s) (.dem). Use `-` to stream a demo from stdin.
    #[arg(required = true)]
    demos: Vec<PathBuf>,

    /// Base name for output files when reading from stdin. Default: "stdin".
    #[arg(long, value_name = "STEM")]
    name: Option<String>,

    /// SteamID64 of the player(s) to extract. Repeat or comma-separate. Default: everyone.
    #[arg(short, long = "steam-id", value_delimiter = ',')]
    steam_ids: Vec<u64>,

    /// Output layout.
    #[arg(short, long, value_enum, default_value_t = Mode::Compact)]
    mode: Mode,

    /// Output format / file extension. ogg and opus are written natively; anything else uses ffmpeg.
    #[arg(short, long, default_value = "ogg")]
    format: String,

    /// Output directory.
    #[arg(short, long, default_value = ".")]
    out: PathBuf,

    /// Only list players who talked (SteamID64 and seconds of voice), don't write files.
    #[arg(short, long)]
    list: bool,

    /// Print a JSON summary to stdout (logs go to stderr).
    #[arg(long)]
    json: bool,

    /// Path to ffmpeg (only needed for non-ogg formats and merge mode).
    #[arg(long, default_value = "ffmpeg")]
    ffmpeg: PathBuf,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    if cli.demos.iter().filter(|d| is_stdin(d)).count() > 1 {
        eprintln!("error: stdin (-) can only be given once");
        return ExitCode::from(1);
    }
    let mut json_items = Vec::new();
    let mut found_any = false;
    let mut failed = false;
    for demo in &cli.demos {
        match process(&cli, demo) {
            Ok((json, found)) => {
                json_items.push(json);
                found_any |= found;
            }
            Err(e) => {
                eprintln!("error: {}: {e:#}", demo.display());
                json_items.push(format!(
                    "{{\"demo\":{},\"error\":{}}}",
                    json_str(&demo.display().to_string()),
                    json_str(&format!("{e:#}"))
                ));
                failed = true;
            }
        }
    }
    if cli.json {
        println!("[{}]", json_items.join(","));
    }
    if failed {
        ExitCode::from(1)
    } else if !found_any {
        ExitCode::from(2)
    } else {
        ExitCode::SUCCESS
    }
}

/// Returns (json, found_voice).
fn process(cli: &Cli, demo_path: &Path) -> Result<(String, bool)> {
    let started = Instant::now();
    let wanted = &cli.steam_ids;
    let want = |xuid| wanted.is_empty() || wanted.contains(&xuid);
    let scan = if is_stdin(demo_path) {
        demo::scan_reader(BufReader::with_capacity(1 << 20, std::io::stdin().lock()), &want)?
    } else {
        let file = File::open(demo_path).context("could not open demo")?;
        demo::scan_reader(BufReader::with_capacity(1 << 20, file), &want)?
    };
    if scan.truncated {
        eprintln!("warning: {}: demo is truncated, extracted what was there", demo_path.display());
    }

    let (tracks, stats) = voice::collect(&scan.voice);
    if stats.unsupported_format > 0 {
        eprintln!("warning: {} voice messages in an unsupported codec were skipped", stats.unsupported_format);
    }
    if stats.bad_packets > 0 {
        eprintln!("warning: {} malformed voice packets were skipped", stats.bad_packets);
    }

    let timeline = Timeline {
        origin_tick: scan.first_tick.unwrap_or(0),
        end_tick: scan.last_tick,
        tick_interval: scan.tick_interval,
    };
    let demo_secs = timeline.end_tick.saturating_sub(timeline.origin_tick) as f64 * scan.tick_interval as f64;
    let stem = match (&cli.name, is_stdin(demo_path)) {
        (Some(n), true) => n.clone(),
        (None, true) => "stdin".to_string(),
        _ => demo_path.file_stem().unwrap_or_default().to_string_lossy().into_owned(),
    };

    for id in wanted {
        if !tracks.contains_key(id) {
            eprintln!("warning: {}: no voice found for {id}", demo_path.display());
        }
    }

    let mut players_json = Vec::new();
    let mut files: BTreeMap<u64, PathBuf> = BTreeMap::new();
    let mut timing: BTreeMap<u64, voice::Rendered> = BTreeMap::new();
    // Segment file offsets refer to the layout of the file that is (or would be) written.
    let layout = if matches!(cli.mode, Mode::Compact) { Layout::Compact } else { Layout::Full };

    if !cli.list && !tracks.is_empty() {
        std::fs::create_dir_all(&cli.out).context("could not create output directory")?;
        let ext = cli.format.trim_start_matches('.').to_ascii_lowercase();
        match cli.mode {
            Mode::Full | Mode::Compact => {
                for (&id, track) in &tracks {
                    let path = cli.out.join(format!("{stem}_{id}.{ext}"));
                    let (ogg, info) = render_track(id, track, layout, &timeline, &stem)?;
                    write_output(cli, &ogg, &path, &ext)?;
                    timing.insert(id, info);
                    files.insert(id, path);
                }
            }
            Mode::Merge => {
                let path = cli.out.join(format!("{stem}.{ext}"));
                merge(cli, &tracks, &timeline, &stem, &path, &ext)?;
                for &id in tracks.keys() {
                    files.insert(id, path.clone());
                }
            }
        }
    }

    for (&id, track) in &tracks {
        if let std::collections::btree_map::Entry::Vacant(e) = timing.entry(id) {
            e.insert(voice::render(track, layout, &timeline, std::io::sink(), 0, &[])?);
        }
    }

    for (&id, track) in &tracks {
        let secs = track.voice_samples() as f64 / 48000.0;
        let segments = timing[&id]
            .segments
            .iter()
            .map(|s| {
                format!(
                    "{{\"tick\":{},\"start\":{:.3},\"duration\":{:.3},\"file_offset\":{:.3}}}",
                    s.tick,
                    s.demo_start as f64 / 48000.0,
                    s.samples as f64 / 48000.0,
                    s.file_start as f64 / 48000.0
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let file = files.get(&id);
        if !cli.json {
            match file {
                Some(f) => eprintln!("{id}  {secs:>8.1}s voice  -> {}", f.display()),
                None => println!("{id}  {secs:>8.1}s voice"),
            }
        }
        players_json.push(format!(
            "{{\"steam_id\":\"{id}\",\"voice_seconds\":{secs:.2},\"file\":{},\"segments\":[{segments}]}}",
            file.map(|f| json_str(&f.display().to_string())).unwrap_or_else(|| "null".into())
        ));
    }

    if tracks.is_empty() {
        eprintln!("{}: no voice data found", demo_path.display());
    } else if !cli.json {
        eprintln!(
            "{}: {} player(s), demo {:.0}s, done in {:.2}s",
            demo_path.display(),
            tracks.len(),
            demo_secs,
            started.elapsed().as_secs_f64()
        );
    }

    let json = format!(
        "{{\"demo\":{},\"map\":{},\"duration_seconds\":{demo_secs:.2},\"tick_interval\":{},\"start_tick\":{},\"end_tick\":{},\"players\":[{}]}}",
        json_str(&demo_path.display().to_string()),
        scan.map_name.as_deref().map(json_str).unwrap_or_else(|| "null".into()),
        scan.tick_interval,
        timeline.origin_tick,
        timeline.end_tick,
        players_json.join(",")
    );
    Ok((json, !tracks.is_empty()))
}

fn render_track(
    id: u64,
    track: &Track,
    layout: Layout,
    timeline: &Timeline,
    stem: &str,
) -> Result<(Vec<u8>, voice::Rendered)> {
    let comments = vec![format!("TITLE={stem} {id}"), format!("STEAMID={id}")];
    let mut buf = Vec::new();
    let info = voice::render(track, layout, timeline, &mut buf, id as u32, &comments)?;
    Ok((buf, info))
}

fn is_stdin(p: &Path) -> bool {
    p.as_os_str() == "-"
}

fn is_native(ext: &str) -> bool {
    matches!(ext, "ogg" | "opus" | "oga")
}

fn write_output(cli: &Cli, ogg: &[u8], path: &Path, ext: &str) -> Result<()> {
    if is_native(ext) {
        std::fs::write(path, ogg).with_context(|| format!("could not write {}", path.display()))?;
        return Ok(());
    }
    let mut child = ffmpeg_cmd(cli)
        .args(["-f", "ogg", "-i", "pipe:0"])
        .args(codec_args(ext))
        .arg(path)
        .stdin(Stdio::piped())
        .spawn()
        .with_context(|| ffmpeg_missing(cli))?;
    child.stdin.take().unwrap().write_all(ogg)?;
    let status = child.wait()?;
    if !status.success() {
        bail!("ffmpeg failed converting to {}", path.display());
    }
    Ok(())
}

fn merge(cli: &Cli, tracks: &BTreeMap<u64, Track>, timeline: &Timeline, stem: &str, path: &Path, ext: &str) -> Result<()> {
    let tmp = std::env::temp_dir().join(format!("cs2-voice-{}", std::process::id()));
    std::fs::create_dir_all(&tmp)?;
    let result = (|| {
        let mut cmd = ffmpeg_cmd(cli);
        for (&id, track) in tracks {
            let p = tmp.join(format!("{id}.ogg"));
            std::fs::write(&p, render_track(id, track, Layout::Full, timeline, stem)?.0)?;
            cmd.arg("-i").arg(&p);
        }
        let n = tracks.len();
        if n > 1 {
            cmd.args(["-filter_complex", &format!("amix=inputs={n}:duration=longest:normalize=0,alimiter=limit=0.95")]);
        }
        let status = cmd
            .args(codec_args(ext))
            .arg(path)
            .status()
            .with_context(|| ffmpeg_missing(cli))?;
        if !status.success() {
            bail!("ffmpeg failed mixing into {}", path.display());
        }
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&tmp);
    result
}

fn ffmpeg_cmd(cli: &Cli) -> Command {
    let mut c = Command::new(&cli.ffmpeg);
    c.args(["-hide_banner", "-loglevel", "error", "-y"]);
    c
}

fn codec_args(ext: &str) -> Vec<&'static str> {
    match ext {
        "ogg" | "opus" | "oga" => vec!["-c:a", "libopus", "-b:a", "64k"],
        "mp3" => vec!["-c:a", "libmp3lame", "-q:a", "2"],
        "wav" => vec!["-c:a", "pcm_s16le"],
        _ => vec![],
    }
}

fn ffmpeg_missing(cli: &Cli) -> String {
    format!(
        "could not run ffmpeg ({}). Install ffmpeg or pass --ffmpeg <path>, or use --format ogg which needs nothing extra",
        cli.ffmpeg.display()
    )
}

fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
