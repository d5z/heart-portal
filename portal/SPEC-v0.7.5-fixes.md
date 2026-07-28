# Portal v0.7.5 — Three Fixes

## Fix 1: portal_file_read offset/limit for large files

**File**: `portal/src/tools/file.rs` — `pub async fn read()`

Current behavior: Files > 100K chars are truncated with a message. No way to read the rest.

**Changes**:
1. Add two optional parameters to `read()`:
   - `offset` (integer): byte offset to start reading from (default: 0)  
   - `limit` (integer): max bytes to return (default: 100000, max: 1000000)
2. When offset/limit are provided, read the file, slice `[offset..offset+limit]`, return the slice
3. Always include metadata in response: `total_size`, `offset`, `limit`, `truncated` (bool)
4. The existing truncation logic stays as the DEFAULT behavior (offset=0, limit=100000)

**Schema change** in `portal/src/tools/mod.rs` — `portal_file_read` schema:
Add properties:
```json
"offset": { "type": "integer", "description": "Byte offset to start reading from (default: 0)" },
"limit": { "type": "integer", "description": "Max bytes to return (default: 100000, max: 1000000)" }
```

## Fix 2: `version` subcommand

**File**: `portal/src/main.rs`

Current: `heart-portal version` is parsed as a config file path. Only `--version` works.

**Changes**:
1. Add `Version` variant to the `Commands` enum:
   ```rust
   /// Print version and exit
   Version,
   ```
2. In `main()`, handle `Some(Commands::Version)` by printing version string and exiting:
   ```rust
   Some(Commands::Version) => {
       println!("heart-portal {}", PORTAL_VERSION);
       return Ok(());
   }
   ```

## Fix 3: Bump version to 0.7.5

**File**: `portal/src/protocol.rs` — find `PORTAL_VERSION` const, change to `"0.7.5"`
**File**: `portal/Cargo.toml` — change `version = "0.7.4"` to `version = "0.7.5"`

## Testing

After changes, run:
```bash
cd /Users/d5/heart-portal && cargo test
```

All existing tests must pass. The file.rs tests don't need changes (they test path resolution, not read content).
