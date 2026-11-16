#!/usr/bin/env python3
"""Create a tiny 26.x-layout world seeded with legal and illegal items.

    python3 tools/make_test_world.py /tmp/testworld

Used for end-to-end testing of mcscan; no game install needed.
"""
import gzip
import os
import struct
import sys
import zlib


class T:
    """Typed NBT value: T('b', 1), T('s', 5), T('l', 9), T('d', 1.5), T('I', [1, 2])."""

    def __init__(self, kind, value):
        self.kind, self.value = kind, value


def tag_id(v):
    if isinstance(v, T):
        return {"b": 1, "s": 2, "l": 4, "f": 5, "d": 6, "I": 11, "L": 12}[v.kind]
    if isinstance(v, bool):
        return 1
    if isinstance(v, int):
        return 3
    if isinstance(v, float):
        return 6
    if isinstance(v, str):
        return 8
    if isinstance(v, list):
        return 9
    if isinstance(v, dict):
        return 10
    raise TypeError(v)


def payload(v):
    if isinstance(v, T):
        if v.kind == "I":
            return struct.pack(">i", len(v.value)) + b"".join(struct.pack(">i", x) for x in v.value)
        if v.kind == "L":
            return struct.pack(">i", len(v.value)) + b"".join(struct.pack(">q", x) for x in v.value)
        return struct.pack({"b": ">b", "s": ">h", "l": ">q", "f": ">f", "d": ">d"}[v.kind], v.value)
    if isinstance(v, bool):
        return struct.pack(">b", int(v))
    if isinstance(v, int):
        return struct.pack(">i", v)
    if isinstance(v, float):
        return struct.pack(">d", v)
    if isinstance(v, str):
        raw = v.encode("utf-8")
        return struct.pack(">H", len(raw)) + raw
    if isinstance(v, list):
        elem = tag_id(v[0]) if v else 0
        return struct.pack(">bi", elem, len(v)) + b"".join(payload(x) for x in v)
    if isinstance(v, dict):
        out = b""
        for k, x in v.items():
            out += struct.pack(">b", tag_id(x)) + payload(k) + payload(x)
        return out + b"\x00"
    raise TypeError(v)


def nbt(root):
    return b"\x0a\x00\x00" + payload(root)


def region(chunks):
    """chunks: {(local_x, local_z): root_dict} -> bytes of an .mca file."""
    header = bytearray(8192)
    body = b""
    for (x, z), root in chunks.items():
        data = zlib.compress(nbt(root))
        rec = struct.pack(">ib", len(data) + 1, 2) + data
        rec += b"\x00" * (-len(rec) % 4096)
        sector = 2 + len(body) // 4096
        idx = x + z * 32
        header[idx * 4:idx * 4 + 4] = struct.pack(">i", (sector << 8) | (len(rec) // 4096))
        body += rec
    return bytes(header) + body


DV = 5023


def ench(**levels):
    return {"minecraft:enchantments": {"minecraft:" + k: v for k, v in levels.items()}}


def item(id, count=1, slot=None, comps=None):
    d = {}
    if slot is not None:
        d["Slot"] = T("b", slot)
    d["id"] = "minecraft:" + id
    d["count"] = count
    if comps:
        d["components"] = comps
    return d


def main():
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    w = sys.argv[1]
    over = os.path.join(w, "dimensions", "minecraft", "overworld")
    nether = os.path.join(w, "dimensions", "minecraft", "the_nether")
    for d in (os.path.join(over, "region"), os.path.join(over, "entities"), os.path.join(nether, "region"),
              os.path.join(w, "players", "data"), os.path.join(w, "data", "minecraft")):
        os.makedirs(d, exist_ok=True)

    heavy = {"sections": [{"Y": T("b", 0), "block_states": {"palette": [{"Name": "minecraft:stone"}],
                                                             "data": T("L", list(range(256)))}}],
             "Heightmaps": {"WORLD_SURFACE": T("L", [7] * 37)}}

    chest = {
        "id": "minecraft:chest", "x": 5, "y": 64, "z": 9, "keepPacked": T("b", 0),
        "Items": [
            item("diamond_sword", slot=0, comps=ench(sharpness=255, unbreaking=3)),          # over max
            item("diamond_sword", slot=1, comps=ench(sharpness=5, looting=3)),               # legal
            item("bedrock", 64, slot=2),                                                     # unobtainable
            item("cobblestone", 64, slot=3),                                                 # legal
            item("netherite_pickaxe", slot=4, comps={"minecraft:unbreakable": {}}),          # denied component
            item("red_shulker_box", slot=5, comps={"minecraft:container": [
                {"slot": 0, "item": item("barrier")},                                        # nested unobtainable
                {"slot": 1, "item": item("diamond", 64)},                                    # legal
                {"slot": 2, "item": item("shulker_box")},                                    # nested shulker
            ]}),
            item("ender_pearl", 64, slot=6),                                                 # oversize (max 16)
            item("potion", slot=7, comps={"minecraft:potion_contents": {
                "custom_effects": [{"id": "minecraft:instant_damage", "amplifier": T("b", 125)}]}}),
            item("firework_rocket", 3, slot=8, comps={"minecraft:fireworks": {"flight_duration": T("b", 127)}}),
            item("diamond_chestplate", slot=9, comps=ench(protection=4, blast_protection=4)),  # legacy conflict
            item("paper", slot=10, comps={"minecraft:custom_data": {"vote": T("b", 1)}}),      # strict only
        ],
    }
    barrel = {"id": "minecraft:barrel", "x": 20, "y": 70, "z": 20, "Items": [
        item("elytra", slot=0), item("stick", slot=1, comps=ench(knockback=2))]}              # wrong item
    old_chest = {"id": "minecraft:chest", "x": 40, "y": 60, "z": 40, "Items": [
        {"Slot": T("b", 0), "id": "minecraft:diamond_sword", "Count": T("b", 1),
         "tag": {"Enchantments": [{"id": "minecraft:sharpness", "lvl": T("s", 32767)}], "Unbreakable": T("b", 1)}},
        {"Slot": T("b", 1), "id": "minecraft:stone", "Count": T("b", 64)}]}

    with open(os.path.join(over, "region", "r.0.0.mca"), "wb") as f:
        f.write(region({
            (0, 0): {"DataVersion": DV, "xPos": 0, "yPos": -4, "zPos": 0, "Status": "minecraft:full",
                     "block_entities": [chest], **heavy},
            (1, 1): {"DataVersion": DV, "xPos": 1, "yPos": -4, "zPos": 1, "Status": "minecraft:full",
                     "block_entities": [barrel], **heavy},
            (2, 2): {"DataVersion": 3465, "xPos": 2, "yPos": -4, "zPos": 2, "Status": "minecraft:full",
                     "block_entities": [old_chest], **heavy},
        }))
    with open(os.path.join(nether, "region", "r.-1.0.mca"), "wb") as f:
        f.write(region({
            (31, 0): {"DataVersion": DV, "xPos": -1, "yPos": 0, "zPos": 0, "Status": "minecraft:full",
                      "block_entities": [{"id": "minecraft:hopper", "x": -3, "y": 30, "z": 4,
                                          "Items": [item("command_block", slot=0)]}], **heavy},
        }))
    with open(os.path.join(over, "entities", "r.0.0.mca"), "wb") as f:
        f.write(region({
            (0, 0): {"DataVersion": DV, "Position": T("I", [0, 0]), "Entities": [
                {"id": "minecraft:item_frame", "Pos": [T("d", 3.5), T("d", 65.0), T("d", 3.5)],
                 "UUID": T("I", [1, 2, 3, 4]), "Item": item("zombie_spawn_egg")},
                {"id": "minecraft:item", "Pos": [T("d", 8.2), T("d", 64.0), T("d", 8.9)],
                 "UUID": T("I", [1, 2, 3, 5]), "Item": item("dirt", 12)},
                {"id": "minecraft:experience_orb", "Pos": [T("d", 1.0), T("d", 64.0), T("d", 1.0)],
                 "UUID": T("I", [1, 2, 3, 6]), "Count": 5, "Value": T("s", 3)},
                {"id": "minecraft:zombie", "Pos": [T("d", 2.0), T("d", 64.0), T("d", 2.0)],
                 "UUID": T("I", [1, 2, 3, 7]),
                 "equipment": {"mainhand": item("iron_sword", comps=ench(sharpness=10))}},
            ]},
        }))

    player = {
        "DataVersion": DV, "Dimension": "minecraft:the_nether",
        "Pos": [T("d", 100.5), T("d", 64.0), T("d", -20.5)],
        "Inventory": [item("mace", slot=0, comps=ench(density=5, breach=4)),       # conflict
                      item("bread", 32, slot=1)],
        "EnderItems": [item("totem_of_undying", 5, slot=0)],                       # oversize
        "equipment": {"head": item("diamond_helmet", comps=ench(protection=4))},
    }
    with open(os.path.join(w, "players", "data", "11111111-2222-3333-4444-555555555555.dat"), "wb") as f:
        f.write(gzip.compress(nbt(player)))
    with open(os.path.join(w, "level.dat"), "wb") as f:
        f.write(gzip.compress(nbt({"Data": {"DataVersion": DV, "LevelName": "mcscan test",
                                            "Version": {"Name": "26.3", "Id": DV}}})))
    with open(os.path.join(w, "data", "minecraft", "random_sequences.dat"), "wb") as f:
        f.write(gzip.compress(nbt({"DataVersion": DV, "data": {}})))
    open(os.path.join(w, "session.lock"), "wb").write(b"\xe2\x98\x83")
    print(f"wrote test world to {w}")


if __name__ == "__main__":
    main()
