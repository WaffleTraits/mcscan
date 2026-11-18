package mcscan.live;

import java.io.IOException;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.StandardCopyOption;
import java.nio.file.StandardOpenOption;
import java.time.Instant;
import java.util.LinkedHashSet;
import java.util.Objects;
import java.util.Set;
import java.util.UUID;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.function.Supplier;

import net.minecraft.SharedConstants;
import net.minecraft.core.BlockPos;
import net.minecraft.core.registries.Registries;
import net.minecraft.nbt.CompoundTag;
import net.minecraft.nbt.ListTag;
import net.minecraft.nbt.NbtAccounter;
import net.minecraft.nbt.NbtIo;
import net.minecraft.nbt.Tag;
import net.minecraft.resources.Identifier;
import net.minecraft.resources.ResourceKey;
import net.minecraft.server.MinecraftServer;
import net.minecraft.server.commands.data.BlockDataAccessor;
import net.minecraft.server.commands.data.EntityDataAccessor;
import net.minecraft.server.level.ServerLevel;
import net.minecraft.server.level.ServerPlayer;
import net.minecraft.util.ProblemReporter;
import net.minecraft.world.ItemStackWithSlot;
import net.minecraft.world.entity.Entity;
import net.minecraft.world.entity.EntityEquipment;
import net.minecraft.world.entity.EquipmentSlot;
import net.minecraft.world.entity.player.Inventory;
import net.minecraft.world.entity.player.Player;
import net.minecraft.world.item.ItemStack;
import net.minecraft.world.level.block.entity.BlockEntity;
import net.minecraft.world.level.storage.LevelResource;
import net.minecraft.world.level.storage.TagValueInput;
import net.minecraft.world.level.storage.ValueInput;

/**
 * Carries out requests. Everything that touches the game runs on the server
 * thread, between ticks, so one request sees and changes a consistent state.
 *
 * <p>Requests name a <em>holder</em>:
 * <ul>
 * <li>{@code {type:"block", dim, x, y, z}} - a block entity (its chunk is loaded if needed)</li>
 * <li>{@code {type:"entity", dim, uuid, cx, cz}} - an entity (chunk cx,cz is loaded if it is not around)</li>
 * <li>{@code {type:"player", uuid}} - a player, online or not</li>
 * </ul>
 * {@code get} returns the holder's NBT, shaped like the game saves it.
 * {@code cas} (compare and swap) replaces some of its top-level keys, but
 * only if each of them still equals what the caller expects; otherwise
 * nothing is changed and the caller is told to look again.
 */
final class Handler {
	/** The parts of an online player that can be replaced without reloading the whole player. */
	private static final Set<String> ONLINE_PLAYER_KEYS = Set.of("Inventory", "EnderItems", "equipment");

	private final MinecraftServer server;
	private final Path journal;
	private final String modVersion;
	final AtomicInteger edits = new AtomicInteger();

	Handler(MinecraftServer server, Path journal, String modVersion) {
		this.server = server;
		this.journal = journal;
		this.modVersion = modVersion;
	}

	private interface Target {
		CompoundTag read() throws Exception;

		void write(CompoundTag merged, Set<String> keys) throws Exception;

		/** False when what {@link #read} returns after a write says nothing about how the game took it. */
		default boolean verifiable() {
			return true;
		}
	}

	static CompoundTag ok() {
		CompoundTag tag = new CompoundTag();
		tag.putBoolean("ok", true);
		return tag;
	}

	static CompoundTag error(String message) {
		CompoundTag tag = new CompoundTag();
		tag.putBoolean("ok", false);
		tag.putString("error", message);
		return tag;
	}

	private static CompoundTag missing(String why) {
		CompoundTag tag = ok();
		tag.putBoolean("found", false);
		tag.putString("why", why);
		return tag;
	}

	CompoundTag hello() {
		CompoundTag tag = ok();
		tag.putInt("protocol", McscanLive.PROTOCOL);
		tag.putString("mod", modVersion);
		tag.putString("minecraft", SharedConstants.getCurrentVersion().name());
		tag.putInt("data_version", SharedConstants.getCurrentVersion().dataVersion().version());
		tag.putString("world", realPath(server.getWorldPath(LevelResource.ROOT)));
		tag.putString("level_name", server.getWorldData().getLevelName());
		tag.putString("journal", journal.toAbsolutePath().normalize().toString());
		return tag;
	}

	private static String realPath(Path path) {
		try {
			return path.toRealPath().toString();
		} catch (IOException e) {
			return path.toAbsolutePath().normalize().toString();
		}
	}

	private <T> T onServer(Supplier<T> task, int timeoutSeconds) throws Exception {
		if (!server.isRunning()) {
			throw new IOException("the server is shutting down");
		}
		return server.submit(task).get(timeoutSeconds, TimeUnit.SECONDS);
	}

	/** Called on a connection thread. */
	CompoundTag handle(CompoundTag request) throws Exception {
		String op = request.getStringOr("op", "");
		switch (op) {
			case "ping":
				return ok();
			case "save": {
				// The same as the `save-all flush` command.
				boolean saved = onServer(() -> server.saveEverything(true, true, true), 1800);
				CompoundTag tag = ok();
				tag.putBoolean("saved", saved);
				return tag;
			}
			case "get":
			case "cas":
				return onHolder(request, op.equals("cas"));
			default:
				return error("unknown request \"" + op + "\"");
		}
	}

	private CompoundTag onHolder(CompoundTag request, boolean cas) throws Exception {
		CompoundTag holder = request.getCompoundOrEmpty("holder");
		// An entity in an unloaded chunk only shows up a few ticks after the chunk is asked for.
		for (int attempt = 0;; attempt++) {
			CompoundTag response = onServer(() -> {
				try {
					Target target = resolve(holder);
					if (target == null) {
						return null;
					}
					return cas ? swap(holder, target, request) : get(target);
				} catch (Refusal e) {
					return e.response;
				} catch (Exception e) {
					McscanLive.LOGGER.warn("mcscan live: {} failed for {}", cas ? "edit" : "read", holder, e);
					return error(String.valueOf(e.getMessage() == null ? e : e.getMessage()));
				}
			}, 120);
			if (response != null) {
				return response;
			}
			if (attempt >= 60) {
				return missing("the entity is not in the world (it did not appear after loading its chunk)");
			}
			Thread.sleep(50);
		}
	}

	/** Thrown to answer a request early. */
	private static final class Refusal extends Exception {
		final transient CompoundTag response;

		Refusal(CompoundTag response) {
			super(null, null, false, false);
			this.response = response;
		}
	}

	private ServerLevel level(CompoundTag holder) throws Refusal {
		String name = holder.getStringOr("dim", "");
		Identifier id = Identifier.tryParse(name);
		ServerLevel level = id == null ? null : server.getLevel(ResourceKey.create(Registries.DIMENSION, id));
		if (level == null) {
			throw new Refusal(missing("this server has no dimension \"" + name + "\""));
		}
		return level;
	}

	private static UUID uuid(CompoundTag holder) throws Refusal {
		try {
			return UUID.fromString(holder.getStringOr("uuid", ""));
		} catch (IllegalArgumentException e) {
			throw new Refusal(error("bad uuid \"" + holder.getStringOr("uuid", "") + "\""));
		}
	}

	/** Find the holder. Null means "an entity that may still be loading - ask again". */
	private Target resolve(CompoundTag holder) throws Exception {
		switch (holder.getStringOr("type", "")) {
			case "block": {
				ServerLevel level = level(holder);
				if (holder.getInt("x").isEmpty() || holder.getInt("y").isEmpty() || holder.getInt("z").isEmpty()) {
					throw new Refusal(error("block holder without x, y, z"));
				}
				BlockPos pos = new BlockPos(holder.getIntOr("x", 0), holder.getIntOr("y", 0), holder.getIntOr("z", 0));
				level.getChunk(pos.getX() >> 4, pos.getZ() >> 4);
				BlockEntity blockEntity = level.getBlockEntity(pos);
				if (blockEntity == null) {
					throw new Refusal(missing("there is no block entity at " + pos.toShortString() + " any more"));
				}
				BlockDataAccessor accessor = new BlockDataAccessor(blockEntity, pos);
				return new Target() {
					@Override
					public CompoundTag read() {
						return accessor.getData();
					}

					@Override
					public void write(CompoundTag merged, Set<String> keys) {
						// What `/data merge block` does: reload, mark dirty, tell the clients.
						accessor.setData(merged);
					}
				};
			}
			case "entity": {
				UUID uuid = uuid(holder);
				Entity found = null;
				for (ServerLevel level : server.getAllLevels()) {
					found = level.getEntity(uuid);
					if (found != null) {
						break;
					}
				}
				if (found == null) {
					ServerLevel level = level(holder);
					if (holder.getInt("cx").isEmpty() || holder.getInt("cz").isEmpty()) {
						throw new Refusal(missing("the entity is not loaded"));
					}
					level.getChunk(holder.getIntOr("cx", 0), holder.getIntOr("cz", 0));
					return null;
				}
				if (found instanceof Player) {
					throw new Refusal(error("players are edited as type \"player\""));
				}
				EntityDataAccessor accessor = new EntityDataAccessor(found);
				return new Target() {
					@Override
					public CompoundTag read() {
						return accessor.getData();
					}

					@Override
					public void write(CompoundTag merged, Set<String> keys) throws Exception {
						// What `/data merge entity` does.
						accessor.setData(merged);
					}
				};
			}
			case "player": {
				UUID uuid = uuid(holder);
				ServerPlayer player = server.getPlayerList().getPlayer(uuid);
				return player != null ? onlinePlayer(player) : offlinePlayer(uuid);
			}
			default:
				throw new Refusal(error("unknown holder type \"" + holder.getStringOr("type", "") + "\""));
		}
	}

	private Target onlinePlayer(ServerPlayer player) {
		return new Target() {
			@Override
			public CompoundTag read() {
				CompoundTag tag = new EntityDataAccessor(player).getData();
				// Added for commands only; it is not part of what gets saved.
				tag.remove("SelectedItem");
				return tag;
			}

			@Override
			public void write(CompoundTag merged, Set<String> keys) throws Exception {
				for (String key : keys) {
					if (!ONLINE_PLAYER_KEYS.contains(key)) {
						throw new Refusal(error("\"" + key + "\" of an online player cannot be changed while they are "
								+ "playing (only Inventory, EnderItems and equipment can)"));
					}
				}
				// The game refuses to reload a whole player from NBT, so only the
				// item-holding parts are read back in, the way a login reads them.
				ValueInput input = TagValueInput.create(ProblemReporter.DISCARDING, server.registryAccess(), merged);
				if (keys.contains("Inventory")) {
					player.getInventory().load(input.listOrEmpty("Inventory", ItemStackWithSlot.CODEC));
				}
				if (keys.contains("EnderItems")) {
					player.getEnderChestInventory().fromSlots(input.listOrEmpty("EnderItems", ItemStackWithSlot.CODEC));
				}
				if (keys.contains("equipment")) {
					EntityEquipment wanted = input.read("equipment", EntityEquipment.CODEC).orElseGet(EntityEquipment::new);
					// Armour, off hand, body and saddle; the main hand is an inventory slot.
					for (EquipmentSlot slot : Inventory.EQUIPMENT_SLOT_MAPPING.values()) {
						ItemStack stack = wanted.get(slot);
						if (!ItemStack.matches(player.getItemBySlot(slot), stack)) {
							player.setItemSlot(slot, stack.copy());
						}
					}
				}
				player.inventoryMenu.broadcastChanges();
				if (player.containerMenu != player.inventoryMenu) {
					player.containerMenu.broadcastChanges();
				}
			}
		};
	}

	private Target offlinePlayer(UUID uuid) throws Refusal {
		Path file = server.getWorldPath(LevelResource.PLAYER_DATA_DIR).resolve(uuid + ".dat");
		if (!Files.isRegularFile(file)) {
			throw new Refusal(missing("no player " + uuid + " is online or saved in this world"));
		}
		return new Target() {
			@Override
			public CompoundTag read() throws IOException {
				return NbtIo.readCompressed(file, NbtAccounter.unlimitedHeap());
			}

			@Override
			public void write(CompoundTag merged, Set<String> keys) throws IOException {
				// The player is offline (checked just now, on the server thread), so the
				// game is not holding this data in memory. Write a sibling and move it
				// into place so a crash cannot leave half a file.
				Path temp = Files.createTempFile(file.getParent(), uuid + "-", ".dat");
				try {
					NbtIo.writeCompressed(merged, temp);
					Files.move(temp, file, StandardCopyOption.REPLACE_EXISTING);
				} finally {
					Files.deleteIfExists(temp);
				}
			}

			@Override
			public boolean verifiable() {
				return false;
			}
		};
	}

	private static CompoundTag get(Target target) throws Exception {
		CompoundTag tag = ok();
		tag.putBoolean("found", true);
		tag.put("data", target.read());
		return tag;
	}

	private CompoundTag swap(CompoundTag holder, Target target, CompoundTag request) throws Exception {
		Set<String> keys = new LinkedHashSet<>();
		ListTag list = request.getListOrEmpty("keys");
		for (int i = 0; i < list.size(); i++) {
			list.getString(i).ifPresent(keys::add);
		}
		if (keys.isEmpty()) {
			return error("no keys to replace");
		}
		CompoundTag expect = request.getCompoundOrEmpty("expect");
		CompoundTag replace = request.getCompoundOrEmpty("replace");

		CompoundTag live = target.read();
		for (String key : keys) {
			if (!Objects.equals(live.get(key), expect.get(key))) {
				CompoundTag tag = ok();
				tag.putBoolean("found", true);
				tag.putBoolean("applied", false);
				tag.putString("conflict", key);
				return tag;
			}
		}
		CompoundTag merged = live.copy();
		for (String key : keys) {
			Tag value = replace.get(key);
			if (value == null) {
				merged.remove(key);
			} else {
				merged.put(key, value.copy());
			}
		}
		if (request.getBooleanOr("journal", true)) {
			record(holder, keys, live, replace);
		}
		target.write(merged, keys);
		edits.incrementAndGet();

		CompoundTag tag = ok();
		tag.putBoolean("found", true);
		tag.putBoolean("applied", true);
		if (target.verifiable()) {
			// The game drops what it cannot read (an unknown item id, say). Tell the caller.
			CompoundTag after = target.read();
			for (String key : keys) {
				if (!Objects.equals(after.get(key), replace.get(key))) {
					tag.putString("adjusted", key);
					break;
				}
			}
		}
		return tag;
	}

	/** Append what is about to be replaced to logs/mcscan-live.log, so it can be put back by hand. */
	private synchronized void record(CompoundTag holder, Set<String> keys, CompoundTag live, CompoundTag replace)
			throws IOException {
		StringBuilder text = new StringBuilder();
		for (String key : keys) {
			Tag before = live.get(key);
			Tag after = replace.get(key);
			text.append(Instant.now()).append('\t').append(holder).append('\t').append(key)
					.append("\tbefore=").append(before == null ? "(absent)" : before)
					.append("\tafter=").append(after == null ? "(removed)" : after).append('\n');
		}
		try {
			Files.createDirectories(journal.getParent());
			Files.writeString(journal, text, StandardCharsets.UTF_8, StandardOpenOption.CREATE, StandardOpenOption.APPEND);
		} catch (IOException e) {
			throw new IOException("cannot write the journal " + journal + " (" + e.getMessage()
					+ "); nothing was changed. Pass --no-backup to edit without one");
		}
	}
}
