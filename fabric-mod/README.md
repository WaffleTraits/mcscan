# mcscan-live

Optional server-side Fabric mod for Minecraft 26.3 that lets
[mcscan](../README.md) work with a running server: `mcscan scan --live` has
the server save before the files are scanned, and `mcscan apply --live`
deletes or modifies the items inside the running game instead of in the
world files.

Requires Fabric Loader 0.20.1+, Fabric API and Java 25. Server-side only;
clients do not need it. It also works in a single-player world.

## Build

```sh
./gradlew build        # -> build/libs/mcscan-live-1.0.0.jar
```

`gradle.properties` holds the Minecraft, loader, Loom and Fabric API
versions. The mod uses the game's own (unobfuscated) names and no mixins.

## Configuration

`config/mcscan-live.json`, created on first start:

```json
{
  "enabled": true,
  "bind": "127.0.0.1",
  "port": 25599,
  "token": "<64 random hex characters>"
}
```

The file is created readable by the server's user only. mcscan reads the
port and the token from it. Leave `bind` on the loopback address unless
mcscan has to run on another machine; the token is the only thing that
protects the port.

Every change is appended to `logs/mcscan-live.log` (time, holder, key, the
value before and after, as SNBT), unless mcscan is run with `--no-backup`.
Connections and the number of edits are written to the server log.

## What it does

Three things, all on the server thread between two ticks:

| request | effect |
| --- | --- |
| `save` | the same as the `save-all flush` command |
| `get` | return the NBT of one *holder*, shaped as the game saves it |
| `cas` | replace some top-level keys of a holder, but only if each still equals the value the caller expects |

A holder is one of:

* `{type:"block", dim, x, y, z}` — a block entity. Its chunk is loaded if it
  is not. Read and written the way `/data get|merge block` does.
* `{type:"entity", dim, uuid, cx, cz}` — an entity, looked up by UUID in
  every dimension; if it is not loaded, chunk `cx,cz` is loaded and the mod
  waits up to three seconds for it to appear. Read and written the way
  `/data get|merge entity` does.
* `{type:"player", uuid}` — a player. Online: only `Inventory`, `EnderItems`
  and `equipment` can be replaced (the game does not allow reloading a whole
  player), and they are read back in the way a login reads them. Offline:
  the player's `.dat` file is rewritten.

`cas` never merges blindly: if any of the keys changed since the caller
fetched them, nothing is written and the reply says so. That is what keeps
an edit from undoing something a hopper or a player did a moment earlier.

## Protocol

TCP. Each message is a 4-byte big-endian length followed by that many bytes
of uncompressed NBT: one compound with an empty name, the encoding of the
game's own files.

The first message must be `{op:"hello", token:"…", protocol:1}`; the reply
carries `protocol`, `mod`, `minecraft`, `data_version`, `world` (the world
folder's path) and `journal`. Every reply has `ok` (byte) and, when it is 0,
`error` (string). After a wrong token the connection is closed.

```
{op:"save"}                                   -> {ok:1b, saved:1b}
{op:"get", holder:{…}}                        -> {ok:1b, found:1b, data:{…}}
                                              -> {ok:1b, found:0b, why:"…"}
{op:"cas", holder:{…}, keys:["Items"],
 expect:{Items:[…]}, replace:{Items:[…]},
 journal:1b}                                  -> {ok:1b, found:1b, applied:1b}
                                              -> {ok:1b, found:1b, applied:0b, conflict:"Items"}
{op:"ping"}                                   -> {ok:1b}
```

In `cas`, a key listed in `keys` but absent from `expect` must be absent in
the game, and one absent from `replace` is removed. If the game changed what
it was given while reading it back (it drops items it cannot decode), the
reply also carries `adjusted:"<key>"`.
