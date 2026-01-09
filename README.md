# MTG Top Cards

Aggregates the most played Magic: The Gathering cards from tournament data.

## Usage

```bash
# Build
cargo build --release

# Fetch data and show top 20 cards across all formats
./target/release/top_cards --fetch -n 20

# Show top cards for Modern only
./target/release/top_cards --fetch -f Modern -n 100

# Use existing data directory
./target/release/top_cards -d ./data -n 50
```

## Options

| Flag | Description | Default |
|------|-------------|---------|
| `-F, --fetch` | Fetch/update data repository before processing | off |
| `-f, --formats` | Comma-separated formats to include | Standard,Modern,Pioneer,Legacy |
| `-n, --num` | Number of top cards to output | 5000 |
| `-o, --output` | Output file (stdout if not specified) | - |
| `-d, --dir` | Directory to search for JSON files | ./data (with --fetch) or . |
| `-l, --half-life` | Half-life in days for time decay | 45 |
| `-m, --max-age` | Maximum age in days to include | 1825 |
| `-w, --no-weight` | Disable time-based weighting | off |
| `--data-dir` | Directory for data repository | ./data |
| `--data-repo` | Git URL for data repository | barrins-project/mtg_decklist_cache |

## Data Management

The `--fetch` flag uses sparse checkout to efficiently clone only the tournament data files. To purge the data:

```bash
rm -rf data/
```

## Data Source

Tournament data from [barrins-project/mtg_decklist_cache](https://github.com/barrins-project/mtg_decklist_cache).
