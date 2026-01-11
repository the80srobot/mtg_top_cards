use clap::Parser;
use rayon::prelude::*;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufReader, BufWriter, Write};
use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};
use walkdir::WalkDir;

const DEFAULT_DATA_REPO: &str = "https://github.com/barrins-project/mtg_decklist_cache.git";
const SCRYFALL_BULK_API: &str = "https://api.scryfall.com/bulk-data";
const SCRYFALL_CACHE_MAX_AGE_SECS: u64 = 7 * 24 * 60 * 60; // 7 days

#[derive(Parser)]
#[command(name = "top_cards")]
#[command(about = "MTG tournament deck analysis tool")]
struct Args {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Comma-separated list of formats
    #[arg(short, long, default_value = "Standard,Modern,Pioneer,Legacy", global = true)]
    formats: String,

    /// Base directory to search (defaults to ./data when --fetch is used)
    #[arg(short, long, global = true)]
    dir: Option<String>,

    /// Maximum age in days to include
    #[arg(short, long, default_value = "1825", global = true)]
    max_age: i64,

    /// Fetch/update the data repository before processing
    #[arg(short = 'F', long, global = true)]
    fetch: bool,

    /// Directory for the data repository (default: ./data)
    #[arg(long, default_value = "./data", global = true)]
    data_dir: String,

    /// Git URL for the data repository
    #[arg(long, default_value = DEFAULT_DATA_REPO, global = true)]
    data_repo: String,
}

#[derive(clap::Subcommand)]
enum Commands {
    /// Find top played cards across tournaments (default command)
    TopCards(TopCardsArgs),
    /// Search for decks containing specific cards
    SearchDecks(SearchDecksArgs),
}

#[derive(clap::Args)]
struct TopCardsArgs {
    /// Number of top cards to output
    #[arg(short, long, default_value = "5000")]
    num: usize,

    /// Output file (default: stdout)
    #[arg(short, long)]
    output: Option<String>,

    /// Half-life in days for time decay
    #[arg(short = 'l', long, default_value = "45")]
    half_life: f64,

    /// Disable time-based weighting
    #[arg(short = 'w', long)]
    no_weight: bool,

    /// Resolve back faces of double-faced cards via Scryfall
    #[arg(long, default_value = "true")]
    resolve_faces: bool,
}

#[derive(clap::Args)]
struct SearchDecksArgs {
    /// Cards to search for, format: "4 Lightning Bolt" or "Lightning Bolt"
    /// Multiple cards can be specified, all must match (AND logic)
    #[arg(required = true)]
    cards: Vec<String>,

    /// Require exact count match (default: at least N copies)
    #[arg(short, long)]
    exact: bool,

    /// Maximum number of decks to show
    #[arg(short, long, default_value = "50")]
    num: usize,

    /// Include sideboard in search
    #[arg(short, long)]
    sideboard: bool,
}

/// Parsed card search criterion
#[derive(Debug, Clone)]
struct CardCriterion {
    name: String,
    count: Option<u32>,
}

// Scryfall API types
#[derive(Deserialize)]
struct ScryfallBulkDataEntry {
    #[serde(rename = "type")]
    data_type: String,
    download_uri: String,
}

#[derive(Deserialize)]
struct ScryfallBulkDataResponse {
    data: Vec<ScryfallBulkDataEntry>,
}

#[derive(Deserialize)]
struct ScryfallCardFace {
    name: String,
}

#[derive(Deserialize)]
struct ScryfallCard {
    #[allow(dead_code)]
    name: Option<String>,
    layout: Option<String>,
    card_faces: Option<Vec<ScryfallCardFace>>,
}

#[derive(Deserialize, Serialize, Clone)]
struct Tournament {
    format: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    date: Option<String>,
}

#[derive(Deserialize, Serialize, Clone)]
struct Card {
    count: u32,
    name: String,
}

/// Deserialize a value that can be either a string or an integer into Option<String>
fn deserialize_string_or_int<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;
    let value: Option<serde_json::Value> = Option::deserialize(deserializer)?;
    match value {
        None => Ok(None),
        Some(serde_json::Value::String(s)) => Ok(Some(s)),
        Some(serde_json::Value::Number(n)) => Ok(Some(n.to_string())),
        Some(other) => Err(D::Error::custom(format!("expected string or number, got {:?}", other))),
    }
}

#[derive(Deserialize, Serialize, Clone)]
struct Deck {
    #[serde(default)]
    player: Option<String>,
    #[serde(default, deserialize_with = "deserialize_string_or_int")]
    result: Option<String>,
    #[serde(default, alias = "anchor_uri")]
    url: Option<String>,
    mainboard: Option<Vec<Card>>,
    sideboard: Option<Vec<Card>>,
}

#[derive(Deserialize)]
struct DecklistFile {
    tournament: Tournament,
    decks: Option<Vec<Deck>>,
}

/// A matching deck with tournament context
#[derive(Serialize)]
struct DeckMatch {
    tournament: Tournament,
    file_date: String,
    player: Option<String>,
    result: Option<String>,
    url: Option<String>,
    mainboard: Vec<Card>,
    sideboard: Vec<Card>,
    matched_cards: Vec<CardMatchInfo>,
}

/// Info about a matched card criterion
#[derive(Serialize)]
struct CardMatchInfo {
    name: String,
    requested: Option<u32>,
    found_main: u32,
    found_side: u32,
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
        let output = Command::new("git")
            .args(["pull", "--ff-only"])
            .current_dir(data_dir)
            .stdout(std::process::Stdio::null())
            .status()
            .map_err(|e| format!("Failed to run git pull: {}", e))?;

        if !output.success() {
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

/// Get path to Scryfall bulk data cache file
fn scryfall_cache_path() -> std::path::PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".scryfall")
        .join("oracle-cards.json")
}

/// Check if cache file exists and is fresh enough
fn is_cache_fresh(path: &Path) -> bool {
    if let Ok(metadata) = std::fs::metadata(path) {
        if let Ok(modified) = metadata.modified() {
            if let Ok(age) = SystemTime::now().duration_since(modified) {
                return age.as_secs() < SCRYFALL_CACHE_MAX_AGE_SECS;
            }
        }
    }
    false
}

/// Fetch Scryfall bulk data and cache it locally
fn fetch_scryfall_bulk_data(cache_path: &Path) -> Result<(), String> {
    eprintln!("Fetching Scryfall bulk data index...");

    // Get the download URL for oracle_cards
    let bulk_response: ScryfallBulkDataResponse = ureq::get(SCRYFALL_BULK_API)
        .call()
        .map_err(|e| format!("Failed to fetch bulk data index: {}", e))?
        .into_json()
        .map_err(|e| format!("Failed to parse bulk data index: {}", e))?;

    let oracle_entry = bulk_response
        .data
        .iter()
        .find(|e| e.data_type == "oracle_cards")
        .ok_or("No oracle_cards entry in bulk data")?;

    eprintln!("Downloading oracle cards (~150MB)...");

    // Download the bulk data
    let response = ureq::get(&oracle_entry.download_uri)
        .call()
        .map_err(|e| format!("Failed to download bulk data: {}", e))?;

    // Create cache directory
    if let Some(parent) = cache_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create cache directory: {}", e))?;
    }

    // Write to cache file
    let mut file = File::create(cache_path)
        .map_err(|e| format!("Failed to create cache file: {}", e))?;
    std::io::copy(&mut response.into_reader(), &mut file)
        .map_err(|e| format!("Failed to write cache file: {}", e))?;

    eprintln!("Scryfall data cached at {}", cache_path.display());
    Ok(())
}

/// Build a map of front face name -> back face name from Scryfall bulk data.
fn load_back_faces_from_cache(cache_path: &Path) -> HashMap<String, String> {
    let mut back_faces = HashMap::new();
    let layouts_with_back_faces: HashSet<&str> =
        ["transform", "modal_dfc", "reversible_card"].into_iter().collect();

    let file = match File::open(cache_path) {
        Ok(f) => f,
        Err(_) => return back_faces,
    };
    let reader = BufReader::new(file);

    // Parse as array of cards
    let cards: Vec<ScryfallCard> = match serde_json::from_reader(reader) {
        Ok(c) => c,
        Err(_) => return back_faces,
    };

    for card in cards {
        let layout = match &card.layout {
            Some(l) => l.as_str(),
            None => continue,
        };

        if layouts_with_back_faces.contains(layout) {
            if let Some(faces) = card.card_faces {
                if faces.len() >= 2 {
                    back_faces.insert(faces[0].name.clone(), faces[1].name.clone());
                }
            }
        }
    }

    back_faces
}

/// Get back faces map, fetching bulk data if needed.
fn resolve_back_faces() -> HashMap<String, String> {
    let cache_path = scryfall_cache_path();

    if !is_cache_fresh(&cache_path) {
        if let Err(e) = fetch_scryfall_bulk_data(&cache_path) {
            eprintln!("Warning: Failed to fetch Scryfall data: {}", e);
            // Try to use stale cache if it exists
            if !cache_path.exists() {
                return HashMap::new();
            }
            eprintln!("Using stale cache...");
        }
    }

    load_back_faces_from_cache(&cache_path)
}

/// Parse card criterion from string like "4 Lightning Bolt" or "Lightning Bolt"
fn parse_card_criterion(input: &str) -> CardCriterion {
    let input = input.trim();

    // Try to parse leading number
    let mut chars = input.chars().peekable();
    let mut num_str = String::new();

    while let Some(&c) = chars.peek() {
        if c.is_ascii_digit() {
            num_str.push(c);
            chars.next();
        } else {
            break;
        }
    }

    if !num_str.is_empty() {
        // Skip whitespace after number
        while let Some(&c) = chars.peek() {
            if c.is_whitespace() {
                chars.next();
            } else {
                break;
            }
        }

        let name: String = chars.collect();
        if !name.is_empty() {
            return CardCriterion {
                name,
                count: num_str.parse().ok(),
            };
        }
    }

    // No leading number, entire input is card name
    CardCriterion {
        name: input.to_string(),
        count: None,
    }
}

/// Check if a deck matches all card criteria
fn deck_matches_criteria(
    deck: &Deck,
    criteria: &[CardCriterion],
    exact: bool,
    include_sideboard: bool,
) -> Option<Vec<CardMatchInfo>> {
    let mut match_info = Vec::new();

    // Build card count maps for the deck
    let mut main_counts: HashMap<String, u32> = HashMap::new();
    let mut side_counts: HashMap<String, u32> = HashMap::new();

    if let Some(mainboard) = &deck.mainboard {
        for card in mainboard {
            *main_counts.entry(card.name.to_lowercase()).or_insert(0) += card.count;
        }
    }

    if let Some(sideboard) = &deck.sideboard {
        for card in sideboard {
            *side_counts.entry(card.name.to_lowercase()).or_insert(0) += card.count;
        }
    }

    // Check each criterion
    for criterion in criteria {
        let name_lower = criterion.name.to_lowercase();
        let found_main = main_counts.get(&name_lower).copied().unwrap_or(0);
        let found_side = side_counts.get(&name_lower).copied().unwrap_or(0);

        let total = if include_sideboard {
            found_main + found_side
        } else {
            found_main
        };

        let matches = match criterion.count {
            Some(required) => {
                if exact {
                    total == required
                } else {
                    total >= required
                }
            }
            None => total > 0,
        };

        if !matches {
            return None;
        }

        match_info.push(CardMatchInfo {
            name: criterion.name.clone(),
            requested: criterion.count,
            found_main,
            found_side,
        });
    }

    Some(match_info)
}

/// Search a single file for matching decks
fn search_file_for_decks(
    path: &Path,
    format_patterns: &[String],
    today: i64,
    max_age: i64,
    criteria: &[CardCriterion],
    exact: bool,
    include_sideboard: bool,
) -> Vec<DeckMatch> {
    let mut matches = Vec::new();
    let path_str = path.to_string_lossy();

    // Extract date from path
    let (year, month, day) = match extract_date_from_path(&path_str) {
        Some(d) => d,
        None => return matches,
    };

    let file_days = days_since_epoch(year, month, day);
    let age = today - file_days;

    // Skip if too old
    if age > max_age {
        return matches;
    }

    let file_date = format!("{:04}-{:02}-{:02}", year, month, day);

    // Parse JSON file
    let file = match File::open(path) {
        Ok(f) => f,
        Err(_) => return matches,
    };
    let reader = BufReader::new(file);
    let data: DecklistFile = match serde_json::from_reader(reader) {
        Ok(d) => d,
        Err(_) => return matches,
    };

    // Check format
    let format = match &data.tournament.format {
        Some(f) => f.to_lowercase(),
        None => return matches,
    };

    let format_matches = format_patterns
        .iter()
        .any(|p| format.contains(&p.to_lowercase()));

    if !format_matches {
        return matches;
    }

    // Search each deck
    if let Some(decks) = data.decks {
        for deck in decks {
            if let Some(matched_cards) = deck_matches_criteria(&deck, criteria, exact, include_sideboard) {
                matches.push(DeckMatch {
                    tournament: data.tournament.clone(),
                    file_date: file_date.clone(),
                    player: deck.player.clone(),
                    result: deck.result.clone(),
                    url: deck.url.clone(),
                    mainboard: deck.mainboard.clone().unwrap_or_default(),
                    sideboard: deck.sideboard.clone().unwrap_or_default(),
                    matched_cards,
                });
            }
        }
    }

    matches
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

/// Collect JSON files from a directory
fn collect_json_files(search_dir: &str) -> Vec<std::path::PathBuf> {
    WalkDir::new(search_dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_type().is_file()
                && e.path().extension().map_or(false, |ext| ext == "json")
        })
        .map(|e| e.into_path())
        .collect()
}

/// Run the top-cards command
fn run_top_cards(args: &Args, top_args: &TopCardsArgs) {
    let search_dir = args.dir.clone().unwrap_or_else(|| {
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
    let use_weight = !top_args.no_weight;

    let files = collect_json_files(&search_dir);
    eprintln!("Processing {} files...", files.len());

    // Process files in parallel and merge results
    let card_counts: HashMap<String, f64> = files
        .par_iter()
        .map(|path| {
            process_file(
                path,
                &format_patterns,
                today,
                top_args.half_life,
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

    // Take top N cards
    let top_cards: Vec<_> = sorted.into_iter().take(top_args.num).collect();

    // Resolve back faces if requested
    let back_faces = if top_args.resolve_faces {
        eprintln!("Loading double-faced card data...");
        let faces = resolve_back_faces();
        eprintln!("Loaded {} double-faced cards", faces.len());
        faces
    } else {
        HashMap::new()
    };

    // Build final output: each card, plus back face if it has one
    let mut final_cards: Vec<(String, f64)> = Vec::new();
    for (name, count) in top_cards {
        final_cards.push((name.clone(), count));
        if let Some(back_face) = back_faces.get(&name) {
            final_cards.push((back_face.clone(), count));
        }
    }

    // Output results
    let output: Box<dyn Write> = match &top_args.output {
        Some(path) => {
            let file = File::create(path).expect("Failed to create output file");
            Box::new(BufWriter::new(file))
        }
        None => Box::new(std::io::stdout()),
    };
    let mut writer = std::io::BufWriter::new(output);

    for (card, count) in final_cards {
        writeln!(writer, "{:.2} {}", count, card).unwrap();
    }

    if let Some(path) = &top_args.output {
        eprintln!("Output written to {}", path);
    }
}

/// Run the search-decks command
fn run_search_decks(args: &Args, search_args: &SearchDecksArgs) {
    let search_dir = args.dir.clone().unwrap_or_else(|| {
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

    // Parse card criteria
    let criteria: Vec<CardCriterion> = search_args
        .cards
        .iter()
        .map(|s| parse_card_criterion(s))
        .collect();

    eprintln!("Searching for decks containing:");
    for c in &criteria {
        match c.count {
            Some(n) => eprintln!("  - {} {} ({})", n, c.name, if search_args.exact { "exact" } else { "at least" }),
            None => eprintln!("  - {} (any count)", c.name),
        }
    }

    let files = collect_json_files(&search_dir);
    eprintln!("Searching {} files...", files.len());

    // Search files in parallel
    let mut all_matches: Vec<DeckMatch> = files
        .par_iter()
        .flat_map(|path| {
            search_file_for_decks(
                path,
                &format_patterns,
                today,
                args.max_age,
                &criteria,
                search_args.exact,
                search_args.sideboard,
            )
        })
        .collect();

    // Sort by date (most recent first)
    all_matches.sort_by(|a, b| b.file_date.cmp(&a.file_date));

    // Limit results
    all_matches.truncate(search_args.num);

    eprintln!("Found {} matching decks", all_matches.len());

    if all_matches.is_empty() {
        return;
    }

    // Output results
    println!();
    for (i, deck_match) in all_matches.iter().enumerate() {
        println!("=== Deck {} ===", i + 1);
        println!("Date: {}", deck_match.file_date);
        if let Some(name) = &deck_match.tournament.name {
            println!("Tournament: {}", name);
        }
        if let Some(format) = &deck_match.tournament.format {
            println!("Format: {}", format);
        }
        if let Some(player) = &deck_match.player {
            println!("Player: {}", player);
        }
        if let Some(result) = &deck_match.result {
            println!("Result: {}", result);
        }
        if let Some(url) = &deck_match.url {
            println!("URL: {}", url);
        }

        println!("\nMatched cards:");
        for m in &deck_match.matched_cards {
            let req = match m.requested {
                Some(n) => format!(" (requested: {})", n),
                None => String::new(),
            };
            println!("  {} (main: {}, side: {}){}", m.name, m.found_main, m.found_side, req);
        }

        println!("\nMainboard ({} cards):", deck_match.mainboard.iter().map(|c| c.count).sum::<u32>());
        for card in &deck_match.mainboard {
            println!("  {} {}", card.count, card.name);
        }

        if !deck_match.sideboard.is_empty() {
            println!("\nSideboard ({} cards):", deck_match.sideboard.iter().map(|c| c.count).sum::<u32>());
            for card in &deck_match.sideboard {
                println!("  {} {}", card.count, card.name);
            }
        }
        println!();
    }
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

    match &args.command {
        Some(Commands::TopCards(top_args)) => {
            run_top_cards(&args, top_args);
        }
        Some(Commands::SearchDecks(search_args)) => {
            run_search_decks(&args, search_args);
        }
        None => {
            // Default to top-cards with default arguments
            let default_args = TopCardsArgs {
                num: 5000,
                output: None,
                half_life: 45.0,
                no_weight: false,
                resolve_faces: true,
            };
            run_top_cards(&args, &default_args);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    // ==================== Unit Tests for parse_card_criterion ====================

    #[test]
    fn test_parse_card_criterion_with_count() {
        let criterion = parse_card_criterion("4 Lightning Bolt");
        assert_eq!(criterion.name, "Lightning Bolt");
        assert_eq!(criterion.count, Some(4));
    }

    #[test]
    fn test_parse_card_criterion_without_count() {
        let criterion = parse_card_criterion("Lightning Bolt");
        assert_eq!(criterion.name, "Lightning Bolt");
        assert_eq!(criterion.count, None);
    }

    #[test]
    fn test_parse_card_criterion_with_extra_whitespace() {
        let criterion = parse_card_criterion("  4   Ragavan, Nimble Pilferer  ");
        assert_eq!(criterion.name, "Ragavan, Nimble Pilferer");
        assert_eq!(criterion.count, Some(4));
    }

    #[test]
    fn test_parse_card_criterion_single_copy() {
        let criterion = parse_card_criterion("1 Emrakul, the Aeons Torn");
        assert_eq!(criterion.name, "Emrakul, the Aeons Torn");
        assert_eq!(criterion.count, Some(1));
    }

    #[test]
    fn test_parse_card_criterion_card_starting_with_number() {
        // Card name that might look like it starts with a number
        let criterion = parse_card_criterion("97th Regiment");
        assert_eq!(criterion.name, "th Regiment");
        assert_eq!(criterion.count, Some(97));
    }

    #[test]
    fn test_parse_card_criterion_empty_string() {
        let criterion = parse_card_criterion("");
        assert_eq!(criterion.name, "");
        assert_eq!(criterion.count, None);
    }

    // ==================== Unit Tests for deck_matches_criteria ====================

    fn create_test_deck(mainboard: Vec<(&str, u32)>, sideboard: Vec<(&str, u32)>) -> Deck {
        Deck {
            player: Some("TestPlayer".to_string()),
            result: Some("1st".to_string()),
            url: Some("https://example.com/deck/123".to_string()),
            mainboard: Some(
                mainboard
                    .into_iter()
                    .map(|(name, count)| Card {
                        name: name.to_string(),
                        count,
                    })
                    .collect(),
            ),
            sideboard: Some(
                sideboard
                    .into_iter()
                    .map(|(name, count)| Card {
                        name: name.to_string(),
                        count,
                    })
                    .collect(),
            ),
        }
    }

    #[test]
    fn test_deck_matches_single_card_present() {
        let deck = create_test_deck(
            vec![("Lightning Bolt", 4), ("Mountain", 20)],
            vec![],
        );
        let criteria = vec![CardCriterion {
            name: "Lightning Bolt".to_string(),
            count: None,
        }];

        let result = deck_matches_criteria(&deck, &criteria, false, false);
        assert!(result.is_some());
        let matches = result.unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].found_main, 4);
    }

    #[test]
    fn test_deck_matches_single_card_missing() {
        let deck = create_test_deck(
            vec![("Mountain", 20)],
            vec![],
        );
        let criteria = vec![CardCriterion {
            name: "Lightning Bolt".to_string(),
            count: None,
        }];

        let result = deck_matches_criteria(&deck, &criteria, false, false);
        assert!(result.is_none());
    }

    #[test]
    fn test_deck_matches_with_count_satisfied() {
        let deck = create_test_deck(
            vec![("Lightning Bolt", 4)],
            vec![],
        );
        let criteria = vec![CardCriterion {
            name: "Lightning Bolt".to_string(),
            count: Some(4),
        }];

        let result = deck_matches_criteria(&deck, &criteria, false, false);
        assert!(result.is_some());
    }

    #[test]
    fn test_deck_matches_with_count_not_satisfied() {
        let deck = create_test_deck(
            vec![("Lightning Bolt", 2)],
            vec![],
        );
        let criteria = vec![CardCriterion {
            name: "Lightning Bolt".to_string(),
            count: Some(4),
        }];

        let result = deck_matches_criteria(&deck, &criteria, false, false);
        assert!(result.is_none());
    }

    #[test]
    fn test_deck_matches_exact_count_satisfied() {
        let deck = create_test_deck(
            vec![("Lightning Bolt", 2)],
            vec![],
        );
        let criteria = vec![CardCriterion {
            name: "Lightning Bolt".to_string(),
            count: Some(2),
        }];

        let result = deck_matches_criteria(&deck, &criteria, true, false);
        assert!(result.is_some());
    }

    #[test]
    fn test_deck_matches_exact_count_not_satisfied() {
        let deck = create_test_deck(
            vec![("Lightning Bolt", 4)],
            vec![],
        );
        let criteria = vec![CardCriterion {
            name: "Lightning Bolt".to_string(),
            count: Some(2),
        }];

        // exact=true, so 4 != 2
        let result = deck_matches_criteria(&deck, &criteria, true, false);
        assert!(result.is_none());
    }

    #[test]
    fn test_deck_matches_sideboard_included() {
        let deck = create_test_deck(
            vec![("Mountain", 20)],
            vec![("Blood Moon", 2)],
        );
        let criteria = vec![CardCriterion {
            name: "Blood Moon".to_string(),
            count: None,
        }];

        // Without sideboard
        let result = deck_matches_criteria(&deck, &criteria, false, false);
        assert!(result.is_none());

        // With sideboard
        let result = deck_matches_criteria(&deck, &criteria, false, true);
        assert!(result.is_some());
        let matches = result.unwrap();
        assert_eq!(matches[0].found_side, 2);
    }

    #[test]
    fn test_deck_matches_multiple_criteria_all_match() {
        let deck = create_test_deck(
            vec![("Lightning Bolt", 4), ("Ragavan, Nimble Pilferer", 4)],
            vec![],
        );
        let criteria = vec![
            CardCriterion {
                name: "Lightning Bolt".to_string(),
                count: Some(4),
            },
            CardCriterion {
                name: "Ragavan, Nimble Pilferer".to_string(),
                count: Some(4),
            },
        ];

        let result = deck_matches_criteria(&deck, &criteria, false, false);
        assert!(result.is_some());
        let matches = result.unwrap();
        assert_eq!(matches.len(), 2);
    }

    #[test]
    fn test_deck_matches_multiple_criteria_one_missing() {
        let deck = create_test_deck(
            vec![("Lightning Bolt", 4)],
            vec![],
        );
        let criteria = vec![
            CardCriterion {
                name: "Lightning Bolt".to_string(),
                count: Some(4),
            },
            CardCriterion {
                name: "Ragavan, Nimble Pilferer".to_string(),
                count: Some(4),
            },
        ];

        let result = deck_matches_criteria(&deck, &criteria, false, false);
        assert!(result.is_none());
    }

    #[test]
    fn test_deck_matches_case_insensitive() {
        let deck = create_test_deck(
            vec![("Lightning Bolt", 4)],
            vec![],
        );
        let criteria = vec![CardCriterion {
            name: "LIGHTNING BOLT".to_string(),
            count: None,
        }];

        let result = deck_matches_criteria(&deck, &criteria, false, false);
        assert!(result.is_some());
    }

    // ==================== Integration Tests ====================

    fn create_test_tournament_file(dir: &Path, date_path: &str, content: &str) {
        let full_path = dir.join(date_path);
        std::fs::create_dir_all(full_path.parent().unwrap()).unwrap();
        let mut file = File::create(&full_path).unwrap();
        file.write_all(content.as_bytes()).unwrap();
    }

    fn sample_tournament_json() -> &'static str {
        r#"{
            "tournament": {
                "name": "Test Tournament",
                "format": "Modern",
                "date": "2025-01-10"
            },
            "decks": [
                {
                    "player": "Alice",
                    "result": "1st",
                    "mainboard": [
                        {"count": 4, "name": "Lightning Bolt"},
                        {"count": 4, "name": "Ragavan, Nimble Pilferer"},
                        {"count": 20, "name": "Mountain"}
                    ],
                    "sideboard": [
                        {"count": 2, "name": "Blood Moon"}
                    ]
                },
                {
                    "player": "Bob",
                    "result": "2nd",
                    "mainboard": [
                        {"count": 2, "name": "Lightning Bolt"},
                        {"count": 4, "name": "Thoughtseize"},
                        {"count": 20, "name": "Swamp"}
                    ],
                    "sideboard": []
                }
            ]
        }"#
    }

    #[test]
    fn test_search_file_for_decks_finds_matching_deck() {
        let temp_dir = TempDir::new().unwrap();
        create_test_tournament_file(
            temp_dir.path(),
            "2025/01/10/tournament.json",
            sample_tournament_json(),
        );

        let criteria = vec![CardCriterion {
            name: "Lightning Bolt".to_string(),
            count: Some(4),
        }];

        let matches = search_file_for_decks(
            &temp_dir.path().join("2025/01/10/tournament.json"),
            &["Modern".to_string()],
            today_days(),
            1825,
            &criteria,
            false,
            false,
        );

        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].player, Some("Alice".to_string()));
    }

    #[test]
    fn test_search_file_for_decks_respects_format_filter() {
        let temp_dir = TempDir::new().unwrap();
        create_test_tournament_file(
            temp_dir.path(),
            "2025/01/10/tournament.json",
            sample_tournament_json(),
        );

        let criteria = vec![CardCriterion {
            name: "Lightning Bolt".to_string(),
            count: None,
        }];

        // Search with wrong format
        let matches = search_file_for_decks(
            &temp_dir.path().join("2025/01/10/tournament.json"),
            &["Standard".to_string()],
            today_days(),
            1825,
            &criteria,
            false,
            false,
        );

        assert_eq!(matches.len(), 0);
    }

    #[test]
    fn test_process_file_aggregates_card_counts() {
        let temp_dir = TempDir::new().unwrap();
        create_test_tournament_file(
            temp_dir.path(),
            "2025/01/10/tournament.json",
            sample_tournament_json(),
        );

        let counts = process_file(
            &temp_dir.path().join("2025/01/10/tournament.json"),
            &["Modern".to_string()],
            today_days(),
            45.0,
            1825,
            false, // no weight for easier testing
        );

        // Lightning Bolt: 4 (Alice) + 2 (Bob) = 6
        assert_eq!(counts.get("Lightning Bolt"), Some(&6.0));
        // Mountain: 20 (Alice only)
        assert_eq!(counts.get("Mountain"), Some(&20.0));
        // Swamp: 20 (Bob only)
        assert_eq!(counts.get("Swamp"), Some(&20.0));
    }

    #[test]
    fn test_extract_date_from_path() {
        let date = extract_date_from_path("/data/2025/01/15/tournament.json");
        assert_eq!(date, Some((2025, 1, 15)));

        let no_date = extract_date_from_path("/data/tournament.json");
        assert_eq!(no_date, None);
    }

    #[test]
    fn test_days_since_epoch_ordering() {
        let day1 = days_since_epoch(2025, 1, 1);
        let day2 = days_since_epoch(2025, 1, 2);
        let day_later = days_since_epoch(2025, 6, 15);

        assert!(day2 > day1);
        assert!(day_later > day2);
    }
}

