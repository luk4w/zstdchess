# ZstdChess

ZstdChess is a high-performance, zero-allocation Rust CLI tool for sampling chess games from massive `.pgn.zst` database dumps (such as Lichess monthly dumps) without downloading the entire file to disk.

## Architecture

It streams the compressed databases sequentially over HTTP, decompresses them on-the-fly using Zstandard, and parses PGN headers using a custom short-circuiting state machine. 

Matched games are written to disk concurrently. To save bandwidth and CPU cycles, the connection is instantly terminated as soon as all target game quotas are met across all brackets. If multiple URLs are provided, it will process them sequentially as a playlist, carrying over quotas and player appearance counts between files.

## Usage

1. Configure your parameters in `config.json`.
2. Run the application:

```bash
cargo run --release
```

You can optionally override the configuration file path via CLI arguments:

```bash
zstdchess.exe --config "my_config.json"
```

## Configuration

The behavior is controlled entirely by the JSON configuration file (`config.json`). 

Example structure:
```json
{
  "zstd_database": [
    "db/lichess_db_standard_rated_2026-08.pgn.zst",
    "https://database.lichess.org/standard/lichess_db_standard_rated_2026-07.pgn.zst"
  ],
  "name": "2026_batch",
  "groups": {
    "events": ["Rated Rapid game"],
    "time_controls": ["600+0"],
    "min_plies": 20,
    "start": 600,
    "end": 2600,
    "interval": 200,
    "delta": 100,
    "size": 64
  },
  "player": {
    "quota": 1,
    "banned_string": ["BOT"],
    "required_string": []
  }
}
```

### Parameters

- `zstd_database`: An array of targets to process sequentially. Targets can be either **HTTP/HTTPS URLs** (e.g., `"https://..."`) or **local file paths** relative to the directory where the tool is executed (e.g., `"db/file.pgn.zst"`). You can mix local and network targets in the same batch.
- `name`: (Optional) The name of the output subfolder inside the `output/` directory (e.g. `"2026_Batch"`). If omitted, saves directly to `output/`.

**groups:**
- `events`: Accepted values of the Event header, compared **exactly** (e.g., `["Rated Rapid game"]` excludes arena/swiss tournament games). Empty array = any event.
- `time_controls`: Accepted values of the TimeControl header, compared exactly (e.g., `["600+0"]`). Empty array or omitted = any time control.
- `min_plies`: Minimum number of plies (half-moves) in the game (e.g., `20` = at least 10 moves by each side). `0` or omitted = no minimum.
- `start`, `end`, `interval`: Defines the rating intervals for the brackets. Both players must be in the same bracket; ratings at or above `end` go to the `end` ("plus") bracket.
- `delta`: Maximum allowed rating difference between the two players.
- `size`: The amount of games to sample per interval bracket. Set `size: 0` to collect an unlimited amount of games until the end of the file.

**player:**
- `quota`: Maximum number of times a single player can appear in the entire dataset. Set to `1` for strictly unique players. Set to `0` for unlimited appearances.
- `banned_string`: Player titles or names to ignore (e.g., `["BOT"]`).
- `required_string`: If provided, at least one of the players must have a title matching one of these strings (e.g., `["GM", "IM"]`).
