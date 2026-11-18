package mcscan.live;

import java.io.BufferedInputStream;
import java.io.BufferedOutputStream;
import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.Closeable;
import java.io.DataInputStream;
import java.io.DataOutputStream;
import java.io.EOFException;
import java.io.IOException;
import java.net.InetAddress;
import java.net.InetSocketAddress;
import java.net.ServerSocket;
import java.net.Socket;
import java.nio.charset.StandardCharsets;
import java.security.MessageDigest;
import java.util.Set;
import java.util.concurrent.ConcurrentHashMap;

import net.minecraft.nbt.CompoundTag;
import net.minecraft.nbt.NbtAccounter;
import net.minecraft.nbt.NbtIo;

/**
 * The socket side. Every message is one frame: a 4-byte big-endian length
 * followed by that many bytes of uncompressed NBT (a compound with an empty
 * name), the same encoding the game uses in its own files. The first frame
 * of a connection must be {@code {op:"hello", token:"..."}}.
 */
final class LiveServer implements Closeable {
	private static final int MAX_FRAME = 256 << 20;
	private static final int MAX_CLIENTS = 8;

	private final ServerSocket socket;
	private final Handler handler;
	private final byte[] token;
	private final Set<Socket> clients = ConcurrentHashMap.newKeySet();
	private volatile boolean closed;

	LiveServer(Config config, Handler handler) throws IOException {
		this.handler = handler;
		this.token = config.token.getBytes(StandardCharsets.UTF_8);
		this.socket = new ServerSocket();
		this.socket.setReuseAddress(true);
		try {
			this.socket.bind(new InetSocketAddress(InetAddress.getByName(config.bind), config.port), MAX_CLIENTS);
		} catch (IOException e) {
			this.socket.close();
			throw new IOException("cannot listen on " + config.bind + ":" + config.port + " (" + e.getMessage() + ")");
		}
		Thread thread = new Thread(this::acceptLoop, "mcscan-live-accept");
		thread.setDaemon(true);
		thread.start();
	}

	private void acceptLoop() {
		while (!closed) {
			try {
				Socket client = socket.accept();
				if (clients.size() >= MAX_CLIENTS) {
					client.close();
					continue;
				}
				clients.add(client);
				Thread thread = new Thread(() -> serve(client), "mcscan-live-client");
				thread.setDaemon(true);
				thread.start();
			} catch (IOException e) {
				if (!closed) {
					McscanLive.LOGGER.warn("mcscan live: accept failed: {}", e.toString());
				}
			}
		}
	}

	private void serve(Socket client) {
		String who = String.valueOf(client.getRemoteSocketAddress());
		int editsBefore = handler.edits.get();
		boolean greeted = false;
		try (client) {
			client.setTcpNoDelay(true);
			client.setSoTimeout(15_000);
			DataInputStream in = new DataInputStream(new BufferedInputStream(client.getInputStream()));
			DataOutputStream out = new DataOutputStream(new BufferedOutputStream(client.getOutputStream()));

			CompoundTag hello = readFrame(in);
			byte[] given = hello.getStringOr("token", "").getBytes(StandardCharsets.UTF_8);
			if (!hello.getStringOr("op", "").equals("hello") || !MessageDigest.isEqual(given, token)) {
				McscanLive.LOGGER.warn("mcscan live: refused {} (wrong token)", who);
				Thread.sleep(1000);
				writeFrame(out, Handler.error("wrong token - use the one in the server's config/mcscan-live.json"));
				return;
			}
			if (hello.getIntOr("protocol", McscanLive.PROTOCOL) != McscanLive.PROTOCOL) {
				writeFrame(out, Handler.error("this mod speaks protocol " + McscanLive.PROTOCOL
						+ "; update mcscan and the mod to matching versions"));
				return;
			}
			writeFrame(out, handler.hello());
			greeted = true;
			McscanLive.LOGGER.info("mcscan connected from {}", who);

			client.setSoTimeout(30 * 60_000);
			while (!closed) {
				CompoundTag request;
				try {
					request = readFrame(in);
				} catch (EOFException e) {
					break;
				}
				CompoundTag response;
				try {
					response = handler.handle(request);
				} catch (Exception e) {
					McscanLive.LOGGER.warn("mcscan live: request failed", e);
					response = Handler.error(String.valueOf(e));
				}
				writeFrame(out, response);
			}
		} catch (IOException e) {
			if (!closed && greeted) {
				McscanLive.LOGGER.info("mcscan live: connection from {} ended: {}", who, e.toString());
			}
		} catch (InterruptedException e) {
			Thread.currentThread().interrupt();
		} finally {
			clients.remove(client);
			if (greeted) {
				McscanLive.LOGGER.info("mcscan disconnected ({} edit(s) applied)", handler.edits.get() - editsBefore);
			}
		}
	}

	private static CompoundTag readFrame(DataInputStream in) throws IOException {
		int length = in.readInt();
		if (length <= 0 || length > MAX_FRAME) {
			throw new IOException("bad frame length " + length);
		}
		byte[] bytes = in.readNBytes(length);
		if (bytes.length != length) {
			throw new EOFException();
		}
		return NbtIo.read(new DataInputStream(new ByteArrayInputStream(bytes)), NbtAccounter.unlimitedHeap());
	}

	private static void writeFrame(DataOutputStream out, CompoundTag tag) throws IOException {
		ByteArrayOutputStream bytes = new ByteArrayOutputStream(1024);
		NbtIo.write(tag, new DataOutputStream(bytes));
		out.writeInt(bytes.size());
		bytes.writeTo(out);
		out.flush();
	}

	@Override
	public void close() {
		closed = true;
		try {
			socket.close();
		} catch (IOException e) {
			// closing anyway
		}
		for (Socket client : clients) {
			try {
				client.close();
			} catch (IOException e) {
				// closing anyway
			}
		}
	}
}
