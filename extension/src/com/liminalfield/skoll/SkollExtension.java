package com.liminalfield.skoll;

import com.bitwig.extension.controller.ControllerExtension;
import com.bitwig.extension.controller.api.ControllerHost;
import com.bitwig.extension.controller.api.Transport;
import java.nio.charset.StandardCharsets;
import java.util.HashMap;
import java.util.Iterator;
import java.util.Locale;
import java.util.Map;

/**
 * Sends Bitwig's playhead to Skoll plugin instances.
 *
 * <p>Bitwig does not update the song position it gives plugins while the transport is stopped,
 * so a plugin cannot follow the playhead then. The controller API can.
 *
 * <p>Protocol, all UDP on 127.0.0.1, ASCII:
 *
 * <ul>
 *   <li>Each plugin instance sends {@code hello <port>} to {@link #PORT} every second.
 *   <li>The extension replies to each hello, and sends every transport change to every instance
 *       heard from in the last {@link #SUBSCRIBER_TIMEOUT_MS} ms, as {@code skoll1 <playing 0|1>
 *       <playhead seconds> <play-start seconds>}.
 * </ul>
 */
public class SkollExtension extends ControllerExtension {
    static final int PORT = 58730;
    static final long SUBSCRIBER_TIMEOUT_MS = 3000;

    /** Plugin instance UDP port to the time of its last hello. */
    private final Map<Integer, Long> subscribers = new HashMap<>();

    private boolean playing;
    private double playhead;
    private double playStart;

    protected SkollExtension(SkollExtensionDefinition definition, ControllerHost host) {
        super(definition, host);
    }

    @Override
    public void init() {
        ControllerHost host = getHost();
        Transport transport = host.createTransport();
        transport.isPlaying().addValueObserver(value -> {
            playing = value;
            broadcast();
        });
        transport.playPositionInSeconds().addValueObserver(value -> {
            playhead = value;
            broadcast();
        });
        transport.playStartPositionInSeconds().addValueObserver(value -> {
            playStart = value;
            broadcast();
        });

        if (host.addDatagramPacketObserver("Skoll", PORT, this::onDatagram)) {
            host.println("Skoll Transport: listening on UDP port " + PORT);
        } else {
            host.errorln("Skoll Transport: UDP port " + PORT + " is in use");
        }
    }

    @Override
    public void exit() {}

    @Override
    public void flush() {}

    private synchronized void onDatagram(byte[] data) {
        String message = new String(data, StandardCharsets.US_ASCII).trim();
        if (!message.startsWith("hello ")) {
            return;
        }
        int port;
        try {
            port = Integer.parseInt(message.substring("hello ".length()));
        } catch (NumberFormatException e) {
            return;
        }
        if (port < 1024 || port > 65535) {
            return;
        }
        subscribers.put(port, System.currentTimeMillis());
        send(port, state());
    }

    private synchronized void broadcast() {
        long now = System.currentTimeMillis();
        byte[] state = state();
        Iterator<Map.Entry<Integer, Long>> it = subscribers.entrySet().iterator();
        while (it.hasNext()) {
            Map.Entry<Integer, Long> entry = it.next();
            if (now - entry.getValue() > SUBSCRIBER_TIMEOUT_MS) {
                it.remove();
            } else {
                send(entry.getKey(), state);
            }
        }
    }

    private byte[] state() {
        return String.format(
                        Locale.ROOT, "skoll1 %d %.6f %.6f", playing ? 1 : 0, playhead, playStart)
                .getBytes(StandardCharsets.US_ASCII);
    }

    private void send(int port, byte[] data) {
        getHost().sendDatagramPacket("127.0.0.1", port, data);
    }
}
