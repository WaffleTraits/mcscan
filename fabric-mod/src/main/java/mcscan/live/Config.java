package mcscan.live;

import java.io.IOException;
import java.io.Reader;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.attribute.PosixFilePermissions;
import java.security.SecureRandom;
import java.util.HexFormat;

import com.google.gson.Gson;
import com.google.gson.GsonBuilder;
import com.google.gson.JsonParseException;

/** config/mcscan-live.json. Created with a random token on first start. */
final class Config {
	boolean enabled = true;
	/** Address to listen on. Leave it on the loopback address unless mcscan runs on another machine. */
	String bind = "127.0.0.1";
	int port = 25599;
	/** Shared secret; mcscan reads it from this file. Anyone who knows it can edit items. */
	String token = "";

	static Config load(Path file) throws IOException {
		Gson gson = new GsonBuilder().setPrettyPrinting().disableHtmlEscaping().create();
		Config config = null;
		if (Files.exists(file)) {
			try (Reader reader = Files.newBufferedReader(file, StandardCharsets.UTF_8)) {
				config = gson.fromJson(reader, Config.class);
			} catch (JsonParseException e) {
				throw new IOException(file + " is not valid JSON (" + e.getMessage() + ")");
			}
		}
		boolean write = config == null;
		if (config == null) {
			config = new Config();
		}
		if (config.bind == null || config.bind.isBlank()) {
			config.bind = "127.0.0.1";
			write = true;
		}
		if (config.port < 1 || config.port > 65535) {
			throw new IOException(file + ": port must be between 1 and 65535");
		}
		if (config.token == null || config.token.length() < 16) {
			byte[] random = new byte[32];
			new SecureRandom().nextBytes(random);
			config.token = HexFormat.of().formatHex(random);
			write = true;
		}
		if (write) {
			Files.createDirectories(file.getParent());
			Files.writeString(file, gson.toJson(config) + "\n", StandardCharsets.UTF_8);
			try {
				Files.setPosixFilePermissions(file, PosixFilePermissions.fromString("rw-------"));
			} catch (UnsupportedOperationException | IOException e) {
				// Not a POSIX file system (Windows): the file keeps the folder's permissions.
			}
		}
		return config;
	}
}
