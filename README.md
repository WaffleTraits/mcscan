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

## Full-screen mode

```sh
mcscan tui /srv/minecraft/world          # scan, then browse and fix
mcscan tui mcscan-results.jsonl          # review an earlier scan
```

One session for the whole flow. Given a world it scans it (with the
`illegal` preset unless you pass `--preset`, `--filter` or `--all-items`; the
other `scan` options work too), showing a progress bar, speed and the time
left. The findings then appear in a list you can filter, sort and tick, and
every action is first run as a dry run and shown as a preview — nothing is
written until you confirm that preview. The scan is also saved to the results
file, so the commands below keep working on it.

| key | does |
| --- | --- |
| `↑` `↓` `PgUp` `PgDn` `Home` `End` (or `j` `k` `g` `G`) | move |
| `Space`, `a`, `n`, `i` | select current / all shown / none / invert |
| `Enter` | full details and the item's current NBT on disk |
| `/` | filter: words that must all appear (item, place, rule, file) |
| `s`, `S` | next sort column, reverse |
| `f`, `d` | fix / delete the selection (or the current finding if none is selected) |
| `e` | more actions: clamp or strip enchantments, set count, set/remove NBT, … |
| `r` | rescan |
| `m` | mouse support on/off |
| `?`, `q` | help, quit |

The mouse is optional. With it on (the default), click a row to move to it,
its `[ ]` box or right-click to select it, double-click for details, a column
header to sort, any button along the bottom or in a popup instead of pressing
its key, and use the wheel to scroll. Press `m` (or start with `--no-mouse`)
to turn it off, which gives the terminal's own text selection back.

The same safety rules as `apply` hold: changes are refused while a game or
server has the world open (`--force` overrides), items that changed since the
scan are skipped, and modified files are backed up first (`--no-backup`).

## Step 1 — scan

```sh
mcscan scan /srv/minecraft/world --preset illegal
```

writes `mcscan-results.jsonl` and prints a summary. While it runs, a progress
line shows percent done, speed and the estimated time left. Useful options:

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

## What gets scanned

mcscan does not work from a list of container blocks. It reads every block
entity, entity and player file and recognises item stacks by their shape,
wherever they sit and however deeply they are nested, so a block added by a
new game version or a data pack is covered without an update.

As a cross-check, the block entities of the 26.3 jar were surveyed
(`tools/survey_block_entities.py <jar>` repeats that for any 26.1+ jar). Of
the 49 types the game registers, these store item stacks, and a test
(`every_item_holding_block_entity`) puts an illegal item in each one and
requires it to be found:

| stored in | blocks |
| --- | --- |
| `Items` | chest, trapped chest, barrel, shulker boxes, hopper, dropper, dispenser, crafter, furnace, blast furnace, smoker, brewing stand, campfire and soul campfire, chiseled bookshelf, shelf |
| `item` | decorated pot, suspicious sand and gravel |
| `Book`, `RecordItem` | lectern, jukebox |
| `key_item`, `items_to_eject`, `display_item` | vault |
| a stored mob's equipment | spawner, trial spawner, beehive and bee nest |

Ender chests keep their contents in the player files, which are scanned.
Containers held as items (a chest item with contents, a shulker box in a
bundle) are followed to any depth. Everything else that carries items is an
entity and is scanned the same way: item frames, armour stands, minecarts and
boats with chests or hoppers, dropped items, mobs and their equipment,
villagers, allays, donkeys and llamas.

Not scanned: structure templates under `generated/` (they are blueprints, not
placed blocks) and chunks the server has not written to disk yet.

If a scan finds fewer items than you expect, the summary helps tell why:

```
Looked inside 382 placed block(s) with data (chests, barrels, furnaces, ...), 291 entities and 1 player file(s)
note: 254 container(s) still have an unopened loot table - the game only creates their contents when ...
```

* **Unopened loot.** Chests, barrels, dispensers and pots generated with a
  structure hold only the name of a loot table until a player opens them.
  Their items do not exist yet, so there is nothing to find.
* **`--preset illegal` lists illegal items only.** Use `--all-items` (or a
  `--filter`) to see everything that is stored.
* **Paper/Spigot keep the Nether and the End in separate folders**
  (`world_nether`, `world_the_end`). mcscan points them out when they sit
  next to the world you scan; scan each one.
* **A running server** has recent changes in memory. Run `save-all flush`
  first.

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
