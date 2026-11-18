package mcscan.live;

import java.io.IOException;
import java.nio.file.Path;

import net.fabricmc.api.ModInitializer;
import net.fabricmc.fabric.api.event.lifecycle.v1.ServerLifecycleEvents;
import net.fabricmc.loader.api.FabricLoader;
import net.minecraft.server.MinecraftServer;
import org.slf4j.Logger;
import org.slf4j.LoggerFactory;

/**
 * Companion of the mcscan command-line tool. While a world is running it
 * accepts connections from mcscan (localhost by default, token required) and
 * carries out three things on the server thread: save the world, read one
 * block entity / entity / player, and replace parts of one if they still are
 * what mcscan last saw.
 */
public final class McscanLive implements ModInitializer {
	public static final String MOD_ID = "mcscan-live";
	public static final Logger LOGGER = LoggerFactory.getLogger(MOD_ID);
	/** Version of the wire protocol; mcscan refuses to talk to a different one. */
	public static final int PROTOCOL = 1;

	private LiveServer live;

	@Override
	public void onInitialize() {
		ServerLifecycleEvents.SERVER_STARTED.register(this::start);
		ServerLifecycleEvents.SERVER_STOPPING.register(server -> stop());
	}

	private synchronized void start(MinecraftServer server) {
		stop();
		Path file = FabricLoader.getInstance().getConfigDir().resolve("mcscan-live.json");
		try {
			Config config = Config.load(file);
			if (!config.enabled) {
				LOGGER.info("mcscan live is switched off in {}", file);
				return;
			}
			String version = FabricLoader.getInstance().getModContainer(MOD_ID)
					.map(mod -> mod.getMetadata().getVersion().getFriendlyString()).orElse("?");
			Path journal = FabricLoader.getInstance().getGameDir().resolve("logs").resolve("mcscan-live.log");
			live = new LiveServer(config, new Handler(server, journal, version));
			LOGGER.info("mcscan live is listening on {}:{} (token in {})", config.bind, config.port, file);
		} catch (IOException e) {
			LOGGER.error("mcscan live could not start: {}", e.toString());
		}
	}

	private synchronized void stop() {
		if (live != null) {
			live.close();
			live = null;
		}
	}
}
