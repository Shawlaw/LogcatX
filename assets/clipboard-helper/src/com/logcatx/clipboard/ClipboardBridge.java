package com.logcatx.clipboard;

import android.content.ClipData;
import android.content.ClipboardManager;
import android.content.Context;
import android.content.ContextWrapper;
import android.os.Looper;
import android.util.Base64;
import java.io.BufferedReader;
import java.io.FileDescriptor;
import java.io.FileOutputStream;
import java.io.InputStreamReader;
import java.io.PrintStream;
import java.lang.reflect.Field;
import java.lang.reflect.Method;
import java.nio.charset.StandardCharsets;

/**
 * One-shot clipboard bridge for LogcatX.
 *
 * <p>Android 10+ blocks ordinary background apps from reading the clipboard, but
 * the shell UID is allowed to use the clipboard service for testing. LogcatX
 * pushes this tiny dex to {@code /data/local/tmp} and runs it through
 * {@code app_process} as the shell user. It installs the shell package identity
 * on a system context, then either prints the current clipboard text as
 * base64 ({@code get}) or writes base64 text into the clipboard ({@code set}).
 *
 * <p>Protocol (stdout, one line):
 * <ul>
 *   <li>{@code CLIP <base64>} - current text clipboard</li>
 *   <li>{@code EMPTY} - no text clipboard</li>
 *   <li>{@code ACK} - write succeeded</li>
 *   <li>{@code ERROR <message>} - failure</li>
 * </ul>
 *
 * <p>This file is compiled to {@code assets/clipboard-helper.dex} by
 * {@code scripts/build_clipboard_helper.ps1}; keep both in sync.
 */
public final class ClipboardBridge {
    private static final String PACKAGE = "com.android.shell";
    private static final int MAX_INPUT_BYTES = 512 * 1024;
    private static PrintStream out;

    private ClipboardBridge() {
        // not instantiable
    }

    private static final class ShellContext extends ContextWrapper {
        ShellContext(Context base) {
            super(base);
        }

        @Override
        public String getPackageName() {
            return PACKAGE;
        }

        @Override
        public String getOpPackageName() {
            return PACKAGE;
        }

        @Override
        public Context getApplicationContext() {
            return this;
        }

        @Override
        public Object getSystemService(String name) {
            Object service = super.getSystemService(name);
            // ClipboardManager keeps a Context used for permission checks; make
            // it the shell identity instead of the system one. Samsung devices
            // expose an additional "semclipboard" service with the same field.
            if (service != null
                    && (Context.CLIPBOARD_SERVICE.equals(name) || "semclipboard".equals(name))) {
                try {
                    Field field = service.getClass().getDeclaredField("mContext");
                    field.setAccessible(true);
                    field.set(service, this);
                } catch (Throwable ignored) {
                    // Some ROMs use a different field; the shell UID is still
                    // allowed by the clipboard service on most devices.
                }
            }
            return service;
        }
    }

    /**
     * Build a system context the same way scrcpy does: app_process runs without
     * an Application, so ActivityThread has to be created and registered
     * manually. The looper must already be prepared (see main()).
     */
    private static Context systemContext() throws Exception {
        Class<?> cls = Class.forName("android.app.ActivityThread");
        Object thread = cls.getDeclaredConstructor().newInstance();
        Field current = cls.getDeclaredField("sCurrentActivityThread");
        current.setAccessible(true);
        current.set(null, thread);
        Field systemThread = cls.getDeclaredField("mSystemThread");
        systemThread.setAccessible(true);
        systemThread.setBoolean(thread, true);
        Method getSystemContext = cls.getDeclaredMethod("getSystemContext");
        getSystemContext.setAccessible(true);
        return (Context) getSystemContext.invoke(thread);
    }

    private static ClipboardManager openManager() throws Exception {
        Context context = new ShellContext(systemContext());
        return (ClipboardManager) context.getSystemService(Context.CLIPBOARD_SERVICE);
    }

    private static String readClipboardText() {
        try {
            ClipData clip = manager().getPrimaryClip();
            if (clip == null || clip.getItemCount() == 0) {
                return null;
            }
            CharSequence text = clip.getItemAt(0).getText();
            return text == null ? null : text.toString();
        } catch (Throwable ignored) {
            return null;
        }
    }

    private static ClipboardManager manager;

    private static ClipboardManager manager() {
        if (manager == null) {
            throw new IllegalStateException("clipboard service unavailable");
        }
        return manager;
    }

    private static void emitClip(String text) {
        byte[] bytes = text.getBytes(StandardCharsets.UTF_8);
        out.println("CLIP " + Base64.encodeToString(bytes, Base64.NO_WRAP));
    }

    private static void applyText(String encoded) {
        byte[] bytes = Base64.decode(encoded, Base64.DEFAULT);
        if (bytes.length > MAX_INPUT_BYTES) {
            fail("text too large");
        }
        String text = new String(bytes, StandardCharsets.UTF_8);
        manager.setPrimaryClip(ClipData.newPlainText("LogcatX", text));
        out.println("ACK");
    }

    private static void fail(String message) {
        String sanitized = message == null ? "unknown error" : message.replace('\n', ' ').replace('\r', ' ');
        if (sanitized.length() > 200) {
            sanitized = sanitized.substring(0, 200);
        }
        if (out != null) {
            out.println("ERROR " + sanitized);
        }
        System.exit(1);
    }

    public static void main(String[] args) {
        try {
            // ActivityThread's constructor creates a Handler, which requires a
            // prepared looper even for this short-lived process.
            Looper.prepareMainLooper();
            out = new PrintStream(new FileOutputStream(FileDescriptor.out), true, "UTF-8");
            manager = openManager();
            if (manager == null) {
                fail("no clipboard service");
            }

            if (args.length == 1 && "get".equals(args[0])) {
                String text = readClipboardText();
                if (text == null || text.isEmpty()) {
                    out.println("EMPTY");
                } else {
                    emitClip(text);
                }
                System.exit(0);
            }

            if (args.length == 1 && "set-stdin".equals(args[0])) {
                // Long text is piped through adb shell stdin instead of being
                // placed on the Windows command line (32K limit).
                BufferedReader reader = new BufferedReader(
                        new InputStreamReader(System.in, StandardCharsets.UTF_8));
                String line = reader.readLine();
                if (line == null || line.isEmpty()) {
                    fail("empty stdin");
                }
                applyText(line);
                System.exit(0);
            }

            if (args.length == 2 && "set".equals(args[0])) {
                applyText(args[1]);
                System.exit(0);
            }

            fail("usage: get | set <base64> | set-stdin");
        } catch (Throwable error) {
            String message = error.getClass().getSimpleName();
            if (error.getMessage() != null && !error.getMessage().isEmpty()) {
                message += ": " + error.getMessage();
            }
            fail(message);
        }
    }
}
