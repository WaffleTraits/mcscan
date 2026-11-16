#!/usr/bin/env python3
"""List the block entities of a Minecraft jar and the NBT fields in which
they store item stacks.

    python3 tools/survey_block_entities.py ~/.minecraft/versions/26.3/26.3.jar

mcscan finds item stacks by their shape, wherever they are, so it does not
depend on this list. The survey is the cross-check: it shows every block
entity type the game registers and which of their classes handle item
stacks, so a new game version can be compared against the cases covered by
the `every_item_holding_block_entity` test in src/item.rs.

Needs a jar with readable class names (26.1 and later).
"""
import struct
import sys
import zipfile

PACKAGE = "net/minecraft/world/level/block/entity/"
# Referring to one of these means the class reads or writes item stacks.
ITEM_CLASSES = {
    "net/minecraft/world/item/ItemStack",
    "net/minecraft/world/ContainerHelper",
    "net/minecraft/world/ItemStackWithSlot",
    "net/minecraft/world/item/component/ItemContainerContents",
}


def constants(data):
    """(string constants, referenced class names) of a class file."""
    if data[:4] != b"\xca\xfe\xba\xbe":
        raise ValueError("not a class file")
    count = struct.unpack(">H", data[8:10])[0]
    pos, i = 10, 1
    utf, strings, classes = {}, [], []
    while i < count:
        tag = data[pos]
        if tag == 1:
            length = struct.unpack(">H", data[pos + 1:pos + 3])[0]
            utf[i] = data[pos + 3:pos + 3 + length].decode("utf-8", "replace")
            pos += 3 + length
        elif tag in (3, 4):
            pos += 5
        elif tag in (5, 6):
            pos += 9
            i += 1
        elif tag == 7:
            classes.append(struct.unpack(">H", data[pos + 1:pos + 3])[0])
            pos += 3
        elif tag == 8:
            strings.append(struct.unpack(">H", data[pos + 1:pos + 3])[0])
            pos += 3
        elif tag in (9, 10, 11, 12, 17, 18):
            pos += 5
        elif tag == 15:
            pos += 4
        elif tag in (16, 19, 20):
            pos += 3
        else:
            raise ValueError(f"unknown constant tag {tag}")
        i += 1
    return [utf[s] for s in strings], [utf[c] for c in classes]


def main():
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    jar = zipfile.ZipFile(sys.argv[1])
    names = [n for n in jar.namelist() if n.startswith(PACKAGE) and n.endswith(".class")]
    if not names:
        sys.exit("no readable block entity classes in this jar (obfuscated, i.e. older than 26.1?)")

    types = []
    holders = []
    for name in sorted(names):
        strings, classes = constants(jar.read(name))
        short = name[len(PACKAGE):-len(".class")]
        if short in ("BlockEntityTypeIds", "BlockEntityType"):
            types = types or [s for s in strings if s and s == s.lower() and " " not in s]
            continue
        if ITEM_CLASSES.intersection(classes):
            fields = [s for s in strings if s and len(s) < 40 and " " not in s and "." not in s and "%" not in s]
            holders.append((short, fields))

    print(f"{len(types)} block entity types registered:")
    print("  " + ", ".join(types))
    print()
    print("Classes that handle item stacks, with their string constants")
    print("(the NBT field names are among them; no constants = nothing stored):")
    for short, fields in holders:
        print(f"  {short:<45} {', '.join(fields)}")


if __name__ == "__main__":
    main()
