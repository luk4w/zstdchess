# ZstdChess

Rust CLI that samples chess games from `.pgn.zst` dumps (e.g., Lichess monthly databases) into rating brackets, streaming and decompressing on the fly, without storing the full file.

Sources are read in order as a playlist; quotas and player counts carry over between files, and reading stops once every bracket is full.

## Usage

```bash
# uses config.json
cargo run --release

# uses classical.json
./target/release/zstdchess.exe --config ./classical.json

# replaces an existing output folder
./target/release/zstdchess.exe --config ./blitz.json --overwrite
```

Output: `output/<name>/bracket_*.pgn` and `summary.txt` (discards per filter, games per bracket).

## Configuration

```json
{
  "zstd_database": [
    "db/lichess_db_standard_rated_2026-08.pgn.zst",
    "https://database.lichess.org/standard/lichess_db_standard_rated_2026-07.pgn.zst"
  ],
  "name": "2026_rapid_256",
  "groups": {
    "events": ["Rated Rapid game"],
    "time_controls": ["600+0"],
    "min_plies": 5,
    "start": 600,
    "end": 2600,
    "interval": 200,
    "delta": 100,
    "size": 256
  },
  "player": {
    "quota": 3,
    "banned_string": ["BOT"],
    "required_string": []
  }
}
```

- `zstd_database`: local paths or HTTP(S) URLs, read in order.
- `name`: output subfolder in `output/`.
- `events`, `time_controls`: accepted `Event` / `TimeControl` values, exact match; empty = any. `"Rated Rapid game"` excludes tournament games.
- `min_plies`: minimum half-moves; `0` = none.
- `start`, `end`, `interval`: brackets of `interval` points from `start`; `end` and above go to the `plus` bracket.
- `delta`: maximum rating difference between the players.
- `size`: games per bracket; `0` = unlimited.
- `quota`: maximum games per player; `0` = unlimited.
- `banned_string`: titles to exclude. `required_string`: at least one player must have one of these titles.

## Brackets

Each game goes to the bracket of the higher-rated player, or to the lower-rated player's bracket when the first is full or out of range. The `start` bracket (`bracket_<start>_minus.pgn`) therefore also holds opponents rated below `start`.

Games are read once, in file order, and all brackets fill in parallel.
