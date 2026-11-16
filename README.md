# mcscan

Fast, multi-threaded scanner and editor for items in Minecraft Java worlds.
It reads every inventory in a world folder — chests, shulker boxes (and what
is inside them), hoppers, item frames, dropped items, mobs, minecarts, player
inventories and ender chests — and reports the item stacks that match a rule
preset and/or your own filter expressions. A second step lets you review the
findings, select some, and delete or repair them.

Built for 26.3 and its world layout (`dimensions/<namespace>/<name>/…`,
`players/data/`), and equally happy with older worlds: the classic
`region/` / `DIM-1/` / `playerdata/` layout, the 1.20.5+ component item
format, and the pre-1.20.5 `tag` format are all understood.

## Install

```sh
cargo install --locked --git https://github.com/WaffleTraits/mcscan
```

This builds the optimised binary and puts `mcscan` in `~/.cargo/bin`. Run the
same command again to update; `cargo uninstall mcscan` removes it.

Needs a current stable Rust toolchain and a C linker. On a fresh
Debian/Ubuntu machine:

```sh
sudo apt install -y gcc curl
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

## Build from a checkout

```sh
git clone https://github.com/WaffleTraits/mcscan && cd mcscan
cargo build --release          # binary: target/release/mcscan
cargo install --locked --path . # or install it into ~/.cargo/bin
cargo test                     # unit tests
```

All dependencies are pure Rust.

Prebuilt x86-64 binaries are in `dist/`:

* `dist/mcscan` — linked against glibc, needs glibc 2.34 or newer (Ubuntu
  22.04, Debian 12, RHEL 9 and later). This is the fast one.
* `dist/mcscan-static` — fully static (musl), runs on any x86-64 Linux, but
  musl's allocator makes it several times slower when many threads are used.

Building on the machine that will run it is the best option:
`cargo build --release` there gives the fast build with no glibc concerns.

## Step 1 — scan

```sh
mcscan scan /srv/minecraft/world --preset illegal
```

writes `mcscan-results.jsonl` and prints a summary. Useful options:

| option | meaning |
| --- | --- |
| `-p, --preset illegal` | the built-in illegal-item rules (see below), or a preset JSON file |
| `-f, --filter EXPR` | filter expression; repeat to AND several |
| `--all-items` | list every stack (inventory dump) |
| `--strict`, `--enable R`, `--disable R` | switch preset rules on/off |
| `--only chunks,entities,players,level,data` | restrict what is read |
| `--dim the_nether` | restrict to dimensions |
| `-o FILE` | results file (`-` = stdout) |
| `--snbt` | store each item's full SNBT in the results |
| `-j N` | worker threads (default: all cores) |

A preset and filters combine: `--preset illegal -f 'source == player'` finds
illegal items in player inventories only.

Scanning never writes to the world and is safe while the server runs, but it
only sees what is on disk — run `save-all flush` first for an exact picture.

### Filters

```sh
mcscan scan world -f 'id == diamond_sword && ench(sharpness) > 5'
mcscan scan world -f 'id like *_shulker_box and depth > 0'
mcscan scan world -f 'has(unbreakable) or count > 64'
mcscan scan world -f 'id in (elytra, totem_of_undying)' -f 'source == player'
mcscan scan world -f 'name ~ "(?i)admin" or nbt(components.minecraft:custom_data.owner) == Steve'
```

`mcscan fields` prints the full language: fields (`id count slot damage name
depth holder source dim x y z uuid file max_ench ench_count snbt`), functions
(`ench() has() comp() nbt()`) and operators (`== != < <= > >= like ~ !~
contains in and or not`).

## Step 2 — review, select, act

```sh
mcscan list results.jsonl                      # table of findings, numbered
mcscan list results.jsonl --summary            # counts per rule / item
mcscan list results.jsonl --rule enchant_over_max --format csv
mcscan show results.jsonl -n 12                # full current NBT of finding 12
```

Selection options are shared by `list`, `show` and `apply`:
`-n 1,4,10-20`, `--exclude`, `--rule`, `--id '*_spawn_egg'`, `--holder chest`,
`--source player`, `--dim`, `--uuid`, `--all`. Different options narrow the
selection; several values for one option widen it.

```sh
mcscan apply results.jsonl --all --fix --dry-run     # preview
mcscan apply results.jsonl --all --fix               # repair everything
mcscan apply results.jsonl --rule unobtainable_item --delete
mcscan apply results.jsonl -n 3,7 --clamp-enchants --remove-component unbreakable
mcscan apply results.jsonl --id elytra --set-count 1 --set 'components.minecraft:damage=0'
mcscan apply results.jsonl --all --delete -i         # ask y/n per item
```

Actions: `--fix` (apply each rule's own remedy), `--delete`, `--set-count N`,
`--clamp-enchants`, `--strip-enchants`, `--remove-enchant NAME`,
`--set-enchant NAME=LEVEL`, `--remove-component NAME`, `--set PATH=SNBT`,
`--remove PATH`, `--set-id ID`.

Safety:

* **Stop the server before `apply`.** mcscan refuses to write while a
  game/server holds the world's `session.lock` (`--force` overrides).
* Every item is re-read and compared with the fingerprint taken by the scan;
  anything that changed since is skipped, never guessed at.
* Modified files are first copied to `<world>/mcscan-backups/<timestamp>/`
  (`--no-backup` to skip). Restoring is copying them back.
* Files are replaced atomically (write + rename) keeping owner and mode.
  Untouched chunks are copied byte-for-byte.
* `--dry-run` shows exactly what would happen; without `--yes` you are asked
  to confirm.

## The `illegal` preset

`mcscan rules` lists the rules and what `--fix` does for each:

| rule | catches | `--fix` |
| --- | --- | --- |
| `enchant_over_max` | level above the vanilla maximum (or below 1) | clamp |
| `enchant_unknown` | enchantment that is not in vanilla | remove it |
| `enchant_wrong_item` | enchantment the item cannot hold even via anvil | remove it |
| `enchant_conflict` | mutually exclusive enchantments together | remove the later one |
| `enchant_conflict_legacy` | exclusive now, but obtainable in old releases (stacked protections from 1.14–1.14.2, Infinity + Mending before 1.11.1) | remove the later one |
| `stack_oversize` | more in a stack than the item allows | clamp |
| `bad_count` | zero / negative count | delete |
| `unobtainable_item` | bedrock, barrier, command blocks, spawn eggs, spawners, … | delete |
| `denied_component` | `unbreakable`, `attribute_modifiers`, `max_stack_size`, … or a removed default component | remove it |
| `custom_marker` *(strict)* | `custom_data`, `custom_model_data`, `lore` | remove it |
| `potion_illegal` | custom effects / colour, or no potion type | strip / delete |
| `firework_flight` | flight duration above 3 | clamp to 3 |
| `negative_damage` | negative damage value | set to 0 |
| `nested_shulker` | shulker box inside a shulker box or bundle | delete the inner one |

Notes on judgement calls:

* `custom_marker` is off by default because plugins and data packs put
  `custom_data` and lore on perfectly intended items. Enable with `--strict`.
* If your server legitimately carries items from old versions, scan with
  `--disable enchant_conflict_legacy`.
* If a data pack adds enchantments, use `--disable enchant_unknown`.
* Villager trade offers are scanned too (they are item stacks on disk); the
  `holder` of such a finding is the villager.

### Following new game versions

Enchantment limits, which items each enchantment may sit on, and the exclusive
sets are read from the game jar, not hard-coded:

```sh
python3 tools/gen_preset.py ~/.minecraft/versions/26.4/26.4.jar > my-26.4.json
mcscan scan world --preset my-26.4.json
```

(or overwrite `presets/illegal.json` and rebuild to change the built-in one).
Stack sizes, the unobtainable list and the denied components are plain lists
at the top of that script.

## Results file

JSON Lines: a header line, then one object per finding with `n`, `id`,
`count`, `reasons`, `source`, `holder`, `dim`, `pos`, `slot`, `uuid`, `depth`,
`file`, `chunk`, `path` and `hash`. It is plain text, so `jq`, `grep` and
friends work on it, and `mcscan list - < file` reads it from a pipe.

## Performance notes

Region files are processed in parallel and so are the chunks inside each one.
Chunk data that cannot contain items (block states, heightmaps, light,
structure references) is skipped without being decoded, each worker reuses
one inflate state and buffer, and zlib, gzip, LZ4 and uncompressed chunks as
well as oversized external `.mcc` chunks are supported.

Measured on a 32-thread desktop with the files in the page cache (2.2 GiB,
1,200 region files, 403,650 chunks): 0.45 s with all threads (about 5 GiB/s),
9.6 s with `-j 1`. On a real server the disk is normally the limit.

`tools/make_test_world.py DIR` writes a small 26.x-layout world full of legal
and illegal items for trying things out safely.
