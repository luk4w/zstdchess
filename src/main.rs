use clap::Parser;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use reqwest::blocking::Client;
use serde::Deserialize;
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::time::Instant;

#[derive(Parser, Debug)]
#[command(
    name = "ZstdChess",
    version = "1.0",
    about = "Fast Lichess PGN stream sampler"
)]
struct Args {
    /// Path to the JSON file containing the sampling configuration.
    #[arg(short, long, default_value = "config.json")]
    config: String,

    /// Delete existing bracket files in the output folder instead of refusing to run.
    #[arg(long)]
    overwrite: bool,
}

#[derive(Deserialize, Debug)]
struct GroupsConfig {
    /// Accepted Event header values (exact match). Empty = any.
    events: Vec<String>,
    /// Accepted TimeControl header values (exact match). Empty = any.
    #[serde(default)]
    time_controls: Vec<String>,
    /// Minimum number of plies (half-moves) in the movetext. 0 = no minimum.
    #[serde(default)]
    min_plies: u32,
    start: u32,
    end: u32,
    interval: u32,
    delta: i32,
    size: u32,
}

#[derive(Deserialize, Debug)]
struct PlayerConfig {
    quota: u32,
    banned_string: Vec<String>,
    required_string: Vec<String>,
}

#[derive(Deserialize, Debug)]
struct Config {
    zstd_database: Vec<String>,
    name: Option<String>,
    groups: GroupsConfig,
    player: PlayerConfig,
}

/// Discard reasons, in the order the filters are evaluated.
/// Each rejected game is counted only under the first filter it fails.
#[derive(Clone, Copy)]
enum Reject {
    Event,
    TimeControl,
    BannedTitle,
    RequiredTitle,
    InvalidHeaders,
    Delta,
    DifferentBrackets,
    OutOfRange,
    BracketFull,
    PlayerQuota,
    MinPlies,
}

const REJECT_COUNT: usize = 11;

const REJECT_LABELS: [&str; REJECT_COUNT] = [
    "Event not accepted",
    "TimeControl not accepted",
    "Banned title (e.g. BOT)",
    "Required title missing",
    "Missing/invalid player or rating headers",
    "Rating difference > delta",
    "Players in different brackets",
    "Bracket below start",
    "Bracket already full",
    "Player quota exhausted",
    "Too few plies",
];

fn parse_header<'a>(line: &'a str) -> Option<(&'a str, &'a str)> {
    if line.len() < 4 {
        return None;
    }
    // We only call this if line.starts_with('['), so we skip that check
    if !line.ends_with(']') {
        return None;
    }
    let inner = &line[1..line.len() - 1];
    let space_idx = inner.find(' ')?;
    let key = &inner[..space_idx];
    let value_part = &inner[space_idx + 1..];
    if value_part.starts_with('"') && value_part.ends_with('"') {
        let value = &value_part[1..value_part.len() - 1];
        return Some((key, value));
    }
    None
}

/// Counts SAN plies in a movetext, ignoring move numbers ("1.", "1..."),
/// comments ({ ... } and ; to end of line), variations ( ... ), NAGs ($1)
/// and the game termination marker (1-0, 0-1, 1/2-1/2, *).
/// Stops early once `limit` is reached (pass u32::MAX for a full count).
fn count_plies(movetext: &str, limit: u32) -> u32 {
    let bytes = movetext.as_bytes();
    let mut i = 0;
    let mut plies = 0u32;
    let mut var_depth = 0u32;

    while i < bytes.len() && plies < limit {
        match bytes[i] {
            b'{' => {
                while i < bytes.len() && bytes[i] != b'}' {
                    i += 1;
                }
                i += 1;
            }
            b';' => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'(' => {
                var_depth += 1;
                i += 1;
            }
            b')' => {
                var_depth = var_depth.saturating_sub(1);
                i += 1;
            }
            c if c.is_ascii_whitespace() => i += 1,
            _ => {
                let start = i;
                while i < bytes.len()
                    && !bytes[i].is_ascii_whitespace()
                    && !matches!(bytes[i], b'{' | b'}' | b'(' | b')' | b';')
                {
                    i += 1;
                }
                if var_depth > 0 {
                    continue;
                }
                let token = &movetext[start..i];
                if token.starts_with('$') {
                    continue;
                }
                if matches!(token, "1-0" | "0-1" | "1/2-1/2" | "*") {
                    continue;
                }
                // Strip a leading move number ("12." / "12..."), possibly glued to the move ("12.e4")
                let rest = token.trim_start_matches(|c: char| c.is_ascii_digit());
                let san = if rest.len() < token.len() && rest.starts_with('.') {
                    rest.trim_start_matches('.')
                } else {
                    token
                };
                if !san.is_empty() {
                    plies += 1;
                }
            }
        }
    }
    plies
}

struct GameHeaders {
    event: String,
    time_control: String,
    white_title: String,
    black_title: String,
    white: String,
    black: String,
    white_elo: String,
    black_elo: String,
}

impl GameHeaders {
    fn new() -> Self {
        Self {
            event: String::with_capacity(64),
            time_control: String::with_capacity(16),
            white_title: String::with_capacity(16),
            black_title: String::with_capacity(16),
            white: String::with_capacity(32),
            black: String::with_capacity(32),
            white_elo: String::with_capacity(8),
            black_elo: String::with_capacity(8),
        }
    }

    fn clear(&mut self) {
        self.event.clear();
        self.time_control.clear();
        self.white_title.clear();
        self.black_title.clear();
        self.white.clear();
        self.black.clear();
        self.white_elo.clear();
        self.black_elo.clear();
    }
}

struct Sampler<'a> {
    config: &'a Config,
    brackets: Vec<u32>,
    current_counts: HashMap<u32, u32>,
    player_counts: HashMap<String, u32>,
    open_files: HashMap<u32, BufWriter<File>>,
    bars: HashMap<u32, ProgressBar>,
    rejects: [u64; REJECT_COUNT],
}

impl Sampler<'_> {
    fn all_done(&self) -> bool {
        self.config.groups.size > 0
            && self
                .brackets
                .iter()
                .all(|b| self.current_counts.get(b).unwrap_or(&0) >= &self.config.groups.size)
    }

    fn process_game(&mut self, game_buffer: &str, hdrs: &GameHeaders) {
        match self.evaluate(game_buffer, hdrs) {
            Ok(bracket) => self.accept(game_buffer, hdrs, bracket),
            Err(reason) => self.rejects[reason as usize] += 1,
        }
    }

    /// Runs every filter without side effects. Cheap header checks come first;
    /// the movetext is only scanned for games that passed all of them.
    fn evaluate(&self, game_buffer: &str, hdrs: &GameHeaders) -> Result<u32, Reject> {
        let groups = &self.config.groups;
        let player = &self.config.player;

        if !groups.events.is_empty() && !groups.events.iter().any(|e| hdrs.event == *e) {
            return Err(Reject::Event);
        }

        if !groups.time_controls.is_empty()
            && !groups
                .time_controls
                .iter()
                .any(|tc| hdrs.time_control == *tc)
        {
            return Err(Reject::TimeControl);
        }

        if player
            .banned_string
            .iter()
            .any(|b| hdrs.white_title == *b || hdrs.black_title == *b)
        {
            return Err(Reject::BannedTitle);
        }

        if !player.required_string.is_empty()
            && !player
                .required_string
                .iter()
                .any(|r| hdrs.white_title == *r || hdrs.black_title == *r)
        {
            return Err(Reject::RequiredTitle);
        }

        if hdrs.white.is_empty() || hdrs.black.is_empty() {
            return Err(Reject::InvalidHeaders);
        }
        let w_rating: i32 = hdrs.white_elo.parse().map_err(|_| Reject::InvalidHeaders)?;
        let b_rating: i32 = hdrs.black_elo.parse().map_err(|_| Reject::InvalidHeaders)?;

        if (w_rating - b_rating).abs() > groups.delta {
            return Err(Reject::Delta);
        }

        let step = if groups.interval == 0 {
            200
        } else {
            groups.interval as i32
        };
        let end = groups.end as i32;
        let w_bracket = ((w_rating / step) * step).min(end);
        let b_bracket = ((b_rating / step) * step).min(end);

        if w_bracket != b_bracket {
            return Err(Reject::DifferentBrackets);
        }
        if w_bracket < groups.start as i32 || !self.brackets.contains(&(w_bracket as u32)) {
            return Err(Reject::OutOfRange);
        }
        let bracket = w_bracket as u32;

        if groups.size > 0 && *self.current_counts.get(&bracket).unwrap_or(&0) >= groups.size {
            return Err(Reject::BracketFull);
        }

        if player.quota > 0 {
            let w_count = *self.player_counts.get(&hdrs.white).unwrap_or(&0);
            let b_count = *self.player_counts.get(&hdrs.black).unwrap_or(&0);
            if w_count >= player.quota || b_count >= player.quota {
                return Err(Reject::PlayerQuota);
            }
        }

        if groups.min_plies > 0 {
            // Headers and movetext are separated by the first blank line
            let movetext = game_buffer
                .find("\n\n")
                .map_or("", |idx| &game_buffer[idx + 2..]);
            if count_plies(movetext, groups.min_plies) < groups.min_plies {
                return Err(Reject::MinPlies);
            }
        }

        Ok(bracket)
    }

    /// Commits an accepted game: consumes player quota and writes it to its single bracket.
    fn accept(&mut self, game_buffer: &str, hdrs: &GameHeaders, bracket: u32) {
        if self.config.player.quota > 0 {
            *self.player_counts.entry(hdrs.white.clone()).or_insert(0) += 1;
            *self.player_counts.entry(hdrs.black.clone()).or_insert(0) += 1;
        }

        if let Some(writer) = self.open_files.get_mut(&bracket) {
            let _ = writer.write_all(game_buffer.as_bytes());
        }

        *self.current_counts.entry(bracket).or_insert(0) += 1;

        if let Some(pb) = self.bars.get(&bracket) {
            pb.inc(1);
        }
    }
}

fn bracket_filename(base_out: &str, b: u32, step: u32, end: u32) -> String {
    if b < end {
        format!("{}/bracket_{}_{}.pgn", base_out, b, b + step - 1)
    } else {
        format!("{}/bracket_{}_plus.pgn", base_out, b)
    }
}

/// Refuses to reuse an output folder that already contains files, unless
/// `overwrite` is set, in which case previous bracket files are removed.
fn prepare_output_dir(base_out: &str, overwrite: bool) -> Result<(), Box<dyn std::error::Error>> {
    fs::create_dir_all(base_out)?;

    let existing: Vec<String> = fs::read_dir(base_out)?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();

    if existing.is_empty() {
        return Ok(());
    }

    if !overwrite {
        return Err(format!(
            "Output folder '{}' already contains {} file(s) (e.g. {}). \
             Refusing to run so previous samples are not mixed with new ones. \
             Choose another \"name\" in the config, empty the folder, or rerun with --overwrite.",
            base_out,
            existing.len(),
            existing[0]
        )
        .into());
    }

    for name in &existing {
        if (name.starts_with("bracket_") && name.ends_with(".pgn")) || name == "summary.txt" {
            fs::remove_file(format!("{}/{}", base_out, name))?;
        }
    }
    println!(
        "--overwrite: removed previous bracket files from '{}'",
        base_out
    );
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    // Determine the actual path for config.json
    let config_path = if args.config == "config.json" {
        if let Ok(mut exe_path) = std::env::current_exe() {
            exe_path.pop(); // Remove executable name from path
            exe_path.push("config.json");
            if exe_path.exists() {
                exe_path.to_string_lossy().to_string()
            } else {
                args.config.clone()
            }
        } else {
            args.config.clone()
        }
    } else {
        args.config.clone()
    };

    println!("Loading config from {}...", config_path);
    let config_content = fs::read_to_string(&config_path)
        .map_err(|e| format!("Failed to read config file ({}): {}", config_path, e))?;
    let config: Config = serde_json::from_str(&config_content)
        .map_err(|e| format!("Failed to parse JSON config: {}", e))?;

    // Create output directory based on config
    let base_out = match &config.name {
        Some(name) if !name.is_empty() => format!("output/{}", name),
        _ => "output".to_string(),
    };
    prepare_output_dir(&base_out, args.overwrite)?;

    let client = Client::new();
    let urls = if !config.zstd_database.is_empty() {
        println!(
            "Loaded {} database URL(s) from config.json",
            config.zstd_database.len()
        );
        config.zstd_database.clone()
    } else {
        return Err("No database URL provided! Please provide it in config.json.".into());
    };

    // Initialize brackets and cache file handles (Persists across all URLs)
    let mut sampler = Sampler {
        config: &config,
        brackets: Vec::new(),
        current_counts: HashMap::new(),
        player_counts: HashMap::new(),
        open_files: HashMap::new(),
        bars: HashMap::new(),
        rejects: [0; REJECT_COUNT],
    };

    let m = MultiProgress::new();

    let step = if config.groups.interval == 0 {
        200
    } else {
        config.groups.interval
    };
    let mut b = config.groups.start;

    while b <= config.groups.end {
        sampler.brackets.push(b);
        sampler.current_counts.insert(b, 0);

        let filename = bracket_filename(&base_out, b, step, config.groups.end);
        // Truncate: output must contain only games from this run
        let f = File::create(&filename)?;
        sampler.open_files.insert(b, BufWriter::new(f));

        let max_games = if config.groups.size == 0 {
            u64::MAX
        } else {
            config.groups.size as u64
        };
        let pb = m.add(ProgressBar::new(max_games));

        if config.groups.size == 0 {
            pb.set_style(
                ProgressStyle::with_template("[{prefix:4}] {spinner:.cyan} {pos} games collected")
                    .unwrap(),
            );
        } else {
            pb.set_style(
                ProgressStyle::with_template("[{prefix:4}] [{bar:20.cyan/blue}] {pos}/{len}")
                    .unwrap()
                    .progress_chars("##-"),
            );
        }

        pb.set_prefix(b.to_string());
        sampler.bars.insert(b, pb);

        b += step;
    }

    let spinner = m.add(ProgressBar::new_spinner());
    spinner.set_style(
        ProgressStyle::with_template("{spinner:.green} Games rapidly skipped: {msg}").unwrap(),
    );

    // State that persists across all files in the playlist
    let mut current_game_buffer = String::with_capacity(16 * 1024); // 16KB per game
    let mut headers = GameHeaders::new();
    let mut line_buffer = String::with_capacity(1024);
    let mut games_processed = 0u64;
    let mut all_done_global = false;

    let start_time = Instant::now();

    for url in urls {
        spinner.set_message(format!("Connecting to {}", url));

        let stream: Box<dyn std::io::Read> =
            if url.starts_with("http://") || url.starts_with("https://") {
                let response = match client.get(&url).send() {
                    Ok(res) => match res.error_for_status() {
                        Ok(r) => r,
                        Err(e) => {
                            println!("Failed to connect to {}: {}", url, e);
                            continue;
                        }
                    },
                    Err(e) => {
                        println!("Request failed for {}: {}", url, e);
                        continue;
                    }
                };
                Box::new(response)
            } else {
                match File::open(&url) {
                    Ok(f) => Box::new(f),
                    Err(e) => {
                        println!("Failed to open local file {}: {}", url, e);
                        continue;
                    }
                }
            };

        // Setup on-the-fly decompression for current URL or File
        let decoder = match zstd::Decoder::new(stream) {
            Ok(d) => d,
            Err(e) => {
                println!("ZSTD decoding failed for {}: {}", url, e);
                continue;
            }
        };
        let mut reader = BufReader::with_capacity(1024 * 1024 * 8, decoder); // 8MB read buffer

        let mut in_headers = true;

        loop {
            line_buffer.clear();
            let eof = match reader.read_line(&mut line_buffer) {
                Ok(0) => true, // EOF for this specific URL
                Ok(_) => false,
                Err(e) => {
                    println!("Error reading stream for {}: {}", url, e);
                    true
                }
            };

            let line = line_buffer.trim_end(); // remove \n or \r\n

            // A new game starts (or the file ended): the buffered game is complete
            if eof || line.starts_with("[Event ") {
                if !current_game_buffer.is_empty() {
                    games_processed += 1;

                    if games_processed % 5_000 == 0 {
                        spinner.set_message(games_processed.to_string());
                    }

                    sampler.process_game(&current_game_buffer, &headers);

                    current_game_buffer.clear();
                    headers.clear();
                    in_headers = true;

                    if sampler.all_done() {
                        all_done_global = true;
                        break;
                    }
                }
                if eof {
                    break;
                }
            }

            if in_headers {
                if line.is_empty() {
                    in_headers = false;
                } else if line.starts_with('[') {
                    if let Some((k, v)) = parse_header(line) {
                        match k {
                            "Event" => headers.event.replace_range(.., v),
                            "TimeControl" => headers.time_control.replace_range(.., v),
                            "WhiteTitle" => headers.white_title.replace_range(.., v),
                            "BlackTitle" => headers.black_title.replace_range(.., v),
                            "White" => headers.white.replace_range(.., v),
                            "Black" => headers.black.replace_range(.., v),
                            "WhiteElo" => headers.white_elo.replace_range(.., v),
                            "BlackElo" => headers.black_elo.replace_range(.., v),
                            _ => {}
                        }
                    }
                }
            }

            current_game_buffer.push_str(line);
            current_game_buffer.push('\n');
        }

        if all_done_global {
            break;
        }
    } // End of URLs loop

    for writer in sampler.open_files.values_mut() {
        writer.flush()?;
    }
    spinner.finish_with_message(format!("Games parsed: {}", games_processed));

    let summary = build_summary(&sampler, &base_out, step, games_processed, all_done_global);
    println!("\n{}", summary);
    fs::write(format!("{}/summary.txt", base_out), &summary)?;
    println!(
        "Stream finished in {:.2?}. Summary saved to {}/summary.txt",
        start_time.elapsed(),
        base_out
    );

    Ok(())
}

fn build_summary(
    sampler: &Sampler,
    base_out: &str,
    step: u32,
    games_processed: u64,
    complete: bool,
) -> String {
    let groups = &sampler.config.groups;
    let mut s = String::new();

    s.push_str("=== Sampling summary ===\n");
    s.push_str(&format!(
        "Sources: {}\n",
        sampler.config.zstd_database.join(", ")
    ));
    s.push_str(&format!("Events accepted: {:?}\n", groups.events));
    s.push_str(&format!(
        "TimeControls accepted: {:?}\n",
        groups.time_controls
    ));
    s.push_str(&format!("Min plies: {}\n", groups.min_plies));
    s.push_str(&format!("Max rating delta: {}\n", groups.delta));
    s.push_str(&format!("Player quota: {}\n", sampler.config.player.quota));
    s.push_str(&format!("Games parsed: {}\n", games_processed));
    s.push_str(&format!(
        "All brackets full: {}\n\n",
        if groups.size == 0 {
            "n/a (unlimited size)"
        } else if complete {
            "yes"
        } else {
            "NO"
        }
    ));

    s.push_str("Discards (each game counted under the first filter it failed, in this order):\n");
    let mut total_rejected = 0u64;
    for (label, count) in REJECT_LABELS.iter().zip(sampler.rejects.iter()) {
        s.push_str(&format!("  {:<42} {:>12}\n", label, count));
        total_rejected += count;
    }
    s.push_str(&format!(
        "  {:<42} {:>12}\n\n",
        "Total discarded", total_rejected
    ));

    s.push_str("Accepted per bracket:\n");
    let mut total_accepted = 0u32;
    for &b in &sampler.brackets {
        let count = *sampler.current_counts.get(&b).unwrap_or(&0);
        total_accepted += count;
        let file = bracket_filename(base_out, b, step, groups.end);
        s.push_str(&format!("  {:<42} {:>12}\n", file, count));
    }
    s.push_str(&format!(
        "  {:<42} {:>12}\n",
        "Total accepted", total_accepted
    ));
    s
}

#[cfg(test)]
mod tests {
    use super::count_plies;

    #[test]
    fn counts_lichess_movetext_with_clocks() {
        let mt =
            "1. e4 { [%clk 0:10:00] } 1... e5 { [%clk 0:10:00] } 2. Nf3 { [%clk 0:09:58] } 1-0";
        assert_eq!(count_plies(mt, u32::MAX), 3);
    }

    #[test]
    fn ignores_variations_nags_and_results() {
        let mt = "1.e4 $1 e5 (1... c5 2. Nf3) 2. Nf3 $2 Nc6 ; comment 3. Bb5\n3. Bb5 a6 1/2-1/2";
        assert_eq!(count_plies(mt, u32::MAX), 6);
    }

    #[test]
    fn empty_and_result_only() {
        assert_eq!(count_plies("", u32::MAX), 0);
        assert_eq!(count_plies("0-1", u32::MAX), 0);
        assert_eq!(count_plies("*", u32::MAX), 0);
    }

    #[test]
    fn stops_at_limit() {
        assert_eq!(count_plies("1. e4 e5 2. Nf3 Nc6", 2), 2);
    }
}
