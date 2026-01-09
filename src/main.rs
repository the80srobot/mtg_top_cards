use clap::Parser;
use rayon::prelude::*;
use regex::Regex;
use serde::Deserialize;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, BufWriter, Write};
use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};
use walkdir::WalkDir;

const DEFAULT_DATA_REPO: &str = "https://github.com/barrins-project/mtg_decklist_cache.git";

#[derive(Parser)]
#[command(name = "top_cards")]
#[command(about = "Collects the most played cards across specified MTG formats")]
struct Args {
    /// Comma-separated list of formats
    #[arg(short, long, default_value = "Standard,Modern,Pioneer,Legacy")]
    formats: String,

    /// Number of top cards to output
    #[arg(short, long, default_value = "5000")]
    num: usize,

    /// Output file (default: stdout)
    #[arg(short, long)]
    output: Option<String>,

    /// Base directory to search (defaults to ./data when --fetch is used)
    #[arg(short, long)]
    dir: Option<String>,

    /// Half-life in days for time decay
    #[arg(short = 'l', long, default_value = "45")]
    half_life: f64,

    /// Maximum age in days to include
    #[arg(short, long, default_value = "1825")]
    max_age: i64,

    /// Disable time-based weighting
    #[arg(short = 'w', long)]
    no_weight: bool,

    /// Fetch/update the data repository before processing
    #[arg(short = 'F', long)]
    fetch: bool,

    /// Directory for the data repository (default: ./data)
    #[arg(long, default_value = "./data")]
    data_dir: String,

    /// Git URL for the data repository
    #[arg(long, default_value = DEFAULT_DATA_REPO)]
    data_repo: String,
}

#[derive(Deserialize)]
struct Tournament {
    format: Option<String>,
}

#[derive(Deserialize)]
struct Card {
    count: u32,
    name: String,
}

#[derive(Deserialize)]
struct Deck {
    mainboard: Option<Vec<Card>>,
    sideboard: Option<Vec<Card>>,
}

#[derive(Deserialize)]
struct DecklistFile {
    tournament: Tournament,
    decks: Option<Vec<Deck>>,
}

// Regex for extracting date from path
fn date_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"/(\d{4})/(\d{2})/(\d{2})/").unwrap())
}

fn days_since_epoch(year: i64, month: i64, day: i64) -> i64 {
    // Approximate days since epoch
    (year - 1970) * 365 + (year - 1969) / 4 + (month - 1) * 30 + day
}

fn today_days() -> i64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    now / 86400
}

fn extract_date_from_path(path: &str) -> Option<(i64, i64, i64)> {
    let caps = date_regex().captures(path)?;
    let year: i64 = caps.get(1)?.as_str().parse().ok()?;
    let month: i64 = caps.get(2)?.as_str().parse().ok()?;
    let day: i64 = caps.get(3)?.as_str().parse().ok()?;
    Some((year, month, day))
}

/// Fetch or update the data repository using sparse checkout
fn fetch_data_repo(data_dir: &str, repo_url: &str) -> Result<(), String> {
    let data_path = Path::new(data_dir);

    if data_path.join(".git").exists() {
        // Repository exists, update it
        eprintln!("Updating data repository in {}...", data_dir);
        let status = Command::new("git")
            .args(["pull", "--ff-only"])
            .current_dir(data_dir)
            .status()
            .map_err(|e| format!("Failed to run git pull: {}", e))?;

        if !status.success() {
            return Err("git pull failed".to_string());
        }
    } else {
        // Shallow clone (only recent history)
        eprintln!("Cloning data repository to {}...", data_dir);

        // Create parent directory if needed
        if let Some(parent) = data_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create directory: {}", e))?;
        }

        let status = Command::new("git")
            .args(["clone", "--depth=1", repo_url, data_dir])
            .status()
            .map_err(|e| format!("Failed to run git clone: {}", e))?;

        if !status.success() {
            return Err("git clone failed".to_string());
        }
    }

    eprintln!("Data repository ready.");
    Ok(())
}

fn process_file(
    path: &Path,
    format_patterns: &[String],
    today: i64,
    half_life: f64,
    max_age: i64,
    use_weight: bool,
) -> HashMap<String, f64> {
    let mut cards: HashMap<String, f64> = HashMap::new();

    let path_str = path.to_string_lossy();

    // Extract date from path
    let (year, month, day) = match extract_date_from_path(&path_str) {
        Some(d) => d,
        None => return cards,
    };

    let file_days = days_since_epoch(year, month, day);
    let age = today - file_days;

    // Skip if too old
    if age > max_age {
        return cards;
    }

    // Calculate weight
    let weight = if use_weight {
        2.0_f64.powf(-(age as f64) / half_life)
    } else {
        1.0
    };

    // Parse JSON file
    let file = match File::open(path) {
        Ok(f) => f,
        Err(_) => return cards,
    };
    let reader = BufReader::new(file);
    let data: DecklistFile = match serde_json::from_reader(reader) {
        Ok(d) => d,
        Err(_) => return cards,
    };

    // Check format
    let format = match &data.tournament.format {
        Some(f) => f.to_lowercase(),
        None => return cards,
    };

    let format_matches = format_patterns
        .iter()
        .any(|p| format.contains(&p.to_lowercase()));

    if !format_matches {
        return cards;
    }

    // Process decks
    if let Some(decks) = data.decks {
        for deck in decks {
            if let Some(mainboard) = deck.mainboard {
                for card in mainboard {
                    *cards.entry(card.name).or_insert(0.0) += card.count as f64 * weight;
                }
            }
            if let Some(sideboard) = deck.sideboard {
                for card in sideboard {
                    *cards.entry(card.name).or_insert(0.0) += card.count as f64 * weight;
                }
            }
        }
    }

    cards
}

fn main() {
    let args = Args::parse();

    // Fetch data repository if requested
    if args.fetch {
        if let Err(e) = fetch_data_repo(&args.data_dir, &args.data_repo) {
            eprintln!("Error fetching data: {}", e);
            std::process::exit(1);
        }
    }

    // Determine search directory
    let search_dir = args.dir.unwrap_or_else(|| {
        if args.fetch {
            args.data_dir.clone()
        } else {
            ".".to_string()
        }
    });

    let format_patterns: Vec<String> = args
        .formats
        .split(',')
        .map(|s| s.trim().to_string())
        .collect();
    let today = today_days();
    let use_weight = !args.no_weight;

    // Collect all JSON files
    let files: Vec<_> = WalkDir::new(&search_dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_type().is_file()
                && e.path().extension().map_or(false, |ext| ext == "json")
        })
        .map(|e| e.into_path())
        .collect();

    eprintln!("Processing {} files...", files.len());

    // Process files in parallel and merge results
    let card_counts: HashMap<String, f64> = files
        .par_iter()
        .map(|path| {
            process_file(
                path,
                &format_patterns,
                today,
                args.half_life,
                args.max_age,
                use_weight,
            )
        })
        .reduce(HashMap::new, |mut acc, map| {
            for (card, count) in map {
                *acc.entry(card).or_insert(0.0) += count;
            }
            acc
        });

    // Sort by count descending
    let mut sorted: Vec<_> = card_counts.into_iter().collect();
    sorted.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());

    // Output results
    let output: Box<dyn Write> = match &args.output {
        Some(path) => {
            let file = File::create(path).expect("Failed to create output file");
            Box::new(BufWriter::new(file))
        }
        None => Box::new(std::io::stdout()),
    };
    let mut writer = std::io::BufWriter::new(output);

    for (card, count) in sorted.into_iter().take(args.num) {
        writeln!(writer, "{:.2} {}", count, card).unwrap();
    }

    if let Some(path) = &args.output {
        eprintln!("Output written to {}", path);
    }
}
