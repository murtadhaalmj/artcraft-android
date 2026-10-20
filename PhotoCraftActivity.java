package com.artcraft.photocraft;

import android.content.ClipData;
import android.content.ContentResolver;
import android.content.Intent;
import android.content.SharedPreferences;
import android.database.Cursor;
import android.net.Uri;
import android.os.Build;
import android.os.Bundle;
import android.provider.OpenableColumns;
import android.text.InputType;
import android.util.Log;
import android.view.Gravity;
import android.view.KeyEvent;
import android.view.View;
import android.view.Window;
import android.view.WindowInsets;
import android.view.WindowInsetsController;
import android.view.WindowManager;
import android.view.inputmethod.EditorInfo;
import android.view.inputmethod.InputConnection;
import android.view.inputmethod.InputConnectionWrapper;
import android.view.inputmethod.InputMethodManager;
import android.widget.EditText;
import android.widget.FrameLayout;

import com.google.androidgamesdk.GameActivity;

import org.json.JSONArray;
import org.json.JSONObject;

import java.io.File;
import java.io.FileInputStream;
import java.io.FileOutputStream;
import java.io.InputStream;
import java.io.OutputStream;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.LinkedHashSet;
import java.util.List;
import java.util.Map;
import java.util.concurrent.ConcurrentLinkedQueue;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.TimeUnit;

/** GameActivity subclass that supplies Android SAF open/save results to the Rust UI shell. */
public final class PhotoCraftActivity extends GameActivity {
    private static final String TAG = "PhotoCraftSAF";
    private static final int REQ_OPEN = 0x5043;
    private static final int REQ_SAVE = 0x5044;
    private static final String PREFS = "photocraft_saf_uri_map";
    private final ConcurrentLinkedQueue<String> results = new ConcurrentLinkedQueue<>();
    private final Map<Integer, Long> requestIds = new HashMap<>();
    private final Map<Long, String> suggestedNames = new HashMap<>();
    private final ExecutorService io = Executors.newSingleThreadExecutor();

    // Soft-keyboard host. GameActivity, like the NativeActivity it replaces, has no editable
    // view for the system IME, so the keyboard opens unreliably and delivers no text. This
    // tiny, invisible but focusable EditText has a real InputConnection, so the keyboard
    // shows/dismisses on request and every commit/composition arrives here where imePoll()
    // hands it to the Rust shell as egui IME events.
    private final ConcurrentLinkedQueue<String> imeEvents = new ConcurrentLinkedQueue<>();
    private EditText imeView;
    // True between imeShow() and imeHide(); suppresses a deferred show that a quick hide overtook.
    private volatile boolean imeWanted = false;
    // True while an on-screen Ctrl/Alt is armed: letters are committed at once instead of being held
    // as a composing word, so each one can become a shortcut key press in the Rust shell.
    private volatile boolean shortcutMode = false;

    /** Called by the Rust shell via JNI when the on-screen Ctrl/Alt arms or disarms. */
    public void imeSetShortcutMode(boolean on) {
        shortcutMode = on;
        final EditText view = imeView;
        if (view != null) {
            view.post(() -> {
                try {
                    android.view.inputmethod.InputMethodManager imm =
                            (android.view.inputmethod.InputMethodManager) getSystemService(INPUT_METHOD_SERVICE);
                    if (imm != null) imm.restartInput(view);
                } catch (Throwable ignored) { }
            });
        }
    }

    // Documents the system asked this app to open (an ACTION_VIEW/ACTION_SEND it is registered for):
    // the private path of each staged copy, drained one per call by the Rust shell through
    // intentPoll() and opened exactly like a file chosen in the picker.
    private final ConcurrentLinkedQueue<String> intentPaths = new ConcurrentLinkedQueue<>();

    public String pcFilesDir() { return appStorageDir().getAbsolutePath(); }

    /**
     * The app's private storage root: the app-specific external directory
     * {@code /storage/emulated/0/Android/data/<pkg>/files} whenever the device has it, so the
     * preferences, exports and the working copies of SAF documents stay reachable over USB/MTP, and
     * the internal directory otherwise.
     */
    private File appStorageDir() {
        File external = getExternalFilesDir(null);
        return external != null ? external : getFilesDir();
    }

    private void applyImmersiveFullscreen() {
        Window window = getWindow();
        if (window == null) {
            return;
        }
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            WindowInsetsController controller = window.getInsetsController();
            if (controller != null) {
                controller.hide(WindowInsets.Type.systemBars());
                controller.setSystemBarsBehavior(
                        WindowInsetsController.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE);
            }
        } else {
            window.getDecorView().setSystemUiVisibility(
                    View.SYSTEM_UI_FLAG_IMMERSIVE_STICKY
                            | View.SYSTEM_UI_FLAG_LAYOUT_STABLE
                            | View.SYSTEM_UI_FLAG_LAYOUT_HIDE_NAVIGATION
                            | View.SYSTEM_UI_FLAG_LAYOUT_FULLSCREEN
                            | View.SYSTEM_UI_FLAG_HIDE_NAVIGATION
                            | View.SYSTEM_UI_FLAG_FULLSCREEN);
        }
    }

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        applyImmersiveFullscreen();
        Window window = getWindow();
        if (window != null) {
            window.getDecorView().post(this::applyImmersiveFullscreen);
        }
        imeSetup();
        handleIncomingFiles(getIntent());
    }

    @Override
    protected void onNewIntent(Intent intent) {
        super.onNewIntent(intent);
        // singleTask launch mode: opening a registered file type while the app runs lands here
        // instead of onCreate, so stage it for the shell the same way.
        setIntent(intent);
        applyImmersiveFullscreen();
        handleIncomingFiles(intent);
    }

    /**
     * On a real finish, end the process instead of running GameActivity's teardown: the game
     * glue's destroy handshake waits for the Rust thread to acknowledge the destroy, which
     * winit's Android backend doesn't do once eframe's loop has stopped -- the main thread
     * would hang there, and every later launch in this process with it, since winit's event
     * loop cannot be created twice in one process. Staged file copies get a moment to land
     * before the process goes.
     */
    @Override
    protected void onDestroy() {
        if (isFinishing()) {
            io.shutdown();
            try {
                if (!io.awaitTermination(20, TimeUnit.SECONDS)) {
                    Log.w(TAG, "staged copies still copying after 20 s; ending anyway");
                }
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
            }
            Log.i(TAG, "finished; ending the process");
            android.os.Process.killProcess(android.os.Process.myPid());
        }
        super.onDestroy();
    }

    @Override
    public void onWindowFocusChanged(boolean hasFocus) {
        super.onWindowFocusChanged(hasFocus);
        if (hasFocus) {
            applyImmersiveFullscreen();
        }
    }

    /**
     * Install the invisible, focusable {@link EditText} that makes Android's soft keyboard usable.
     * Called from {@link #onCreate}; failures are logged and leave the app on the old (unreliable)
     * decor-view path, so a device that rejects the extra view still runs.
     */
    private void imeSetup() {
        try {
            // The IME height must not resize or pan the fullscreen native surface; the keyboard
            // simply overlays it. STATE_UNCHANGED keeps the initial hidden state under our control.
            Window window = getWindow();
            if (window != null) {
                window.setSoftInputMode(
                        WindowManager.LayoutParams.SOFT_INPUT_ADJUST_NOTHING
                                | WindowManager.LayoutParams.SOFT_INPUT_STATE_UNCHANGED);
            }
            imeView = new EditText(this) {
                @Override
                public InputConnection onCreateInputConnection(EditorInfo outAttrs) {
                    InputConnection base = super.onCreateInputConnection(outAttrs);
                    if (base == null) {
                        return null;
                    }
                    return new InputConnectionWrapper(base, true) {
                        @Override
                        public boolean commitText(CharSequence text, int newCursorPosition) {
                            if (text != null && text.length() > 0) {
                                imeEvents.offer("c" + text);
                            }
                            return super.commitText(text, newCursorPosition);
                        }

                        @Override
                        public boolean setComposingText(CharSequence text, int newCursorPosition) {
                            if (shortcutMode && text != null && text.length() > 0) {
                                imeEvents.offer("c" + text);
                                return super.commitText(text, newCursorPosition);
                            }
                            imeEvents.offer("p" + (text == null ? "" : text.toString()));
                            return super.setComposingText(text, newCursorPosition);
                        }

                        @Override
                        public boolean finishComposingText() {
                            imeEvents.offer("f");
                            return super.finishComposingText();
                        }

                        @Override
                        public boolean deleteSurroundingText(int beforeLength, int afterLength) {
                            if (beforeLength > 0 || afterLength > 0) {
                                imeEvents.offer("d" + beforeLength + "," + afterLength);
                            }
                            return super.deleteSurroundingText(beforeLength, afterLength);
                        }

                        @Override
                        public boolean performEditorAction(int actionCode) {
                            imeEvents.offer("a" + actionCode);
                            return super.performEditorAction(actionCode);
                        }

                        @Override
                        public boolean sendKeyEvent(KeyEvent event) {
                            if (event.getAction() == KeyEvent.ACTION_DOWN) {
                                int code = event.getKeyCode();
                                if (code == KeyEvent.KEYCODE_DEL) {
                                    imeEvents.offer("d1,0");
                                } else if (code == KeyEvent.KEYCODE_FORWARD_DEL) {
                                    imeEvents.offer("d0,1");
                                } else if (code == KeyEvent.KEYCODE_ENTER) {
                                    imeEvents.offer("a6");
                                }
                            }
                            return super.sendKeyEvent(event);
                        }
                    };
                }

                @Override
                public boolean onKeyDown(int keyCode, KeyEvent event) {
                    // Let hardware key events fall through to the native input queue (winit), exactly
                    // as they did before this view existed; the soft keyboard talks to us through the
                    // InputConnection instead. This keeps physical keyboards working.
                    return false;
                }

                @Override
                public boolean onKeyUp(int keyCode, KeyEvent event) {
                    return false;
                }

                @Override
                public boolean onTouchEvent(android.view.MotionEvent event) {
                    // Purely a keyboard target: never intercept touch, which belongs to the native
                    // surface underneath.
                    return false;
                }
            };
            imeView.setBackground(null);
            imeView.setCursorVisible(false);
            imeView.setTextColor(0);
            imeView.setAlpha(0.01f);
            imeView.setSoundEffectsEnabled(false);
            imeView.setInputType(InputType.TYPE_CLASS_TEXT | InputType.TYPE_TEXT_FLAG_MULTI_LINE);
            imeView.setImeOptions(EditorInfo.IME_FLAG_NO_FULLSCREEN);
            imeView.setImportantForAutofill(View.IMPORTANT_FOR_AUTOFILL_NO);
            FrameLayout.LayoutParams params = new FrameLayout.LayoutParams(1, 1);
            params.gravity = Gravity.TOP | Gravity.START;
            addContentView(imeView, params);
        } catch (Throwable error) {
            Log.w(TAG, "Could not install the soft-keyboard host view", error);
        }
    }

    /** Show the soft keyboard by focusing the IME host view. Called by the Rust shell via JNI. */
    public void imeShow() {
        final EditText view = imeView;
        if (view == null) {
            return;
        }
        imeWanted = true;
        runOnUiThread(() -> {
            if (!imeWanted) {
                return;
            }
            try {
                view.setFocusable(true);
                view.setFocusableInTouchMode(true);
                view.requestFocus();
                InputMethodManager imm =
                        (InputMethodManager) getSystemService(INPUT_METHOD_SERVICE);
                if (imm != null && imm.showSoftInput(view, InputMethodManager.SHOW_IMPLICIT)) {
                    return;
                }
                // Some devices ignore the request until focus and layout have settled; retry once on
                // the next message-loop turn (guarded so a following hide wins).
                view.post(() -> {
                    if (!imeWanted) {
                        return;
                    }
                    try {
                        InputMethodManager retry =
                                (InputMethodManager) getSystemService(INPUT_METHOD_SERVICE);
                        if (retry != null) {
                            retry.showSoftInput(view, InputMethodManager.SHOW_IMPLICIT);
                        }
                    } catch (Throwable error) {
                        Log.w(TAG, "Could not show the soft keyboard", error);
                    }
                });
            } catch (Throwable error) {
                Log.w(TAG, "Could not show the soft keyboard", error);
            }
        });
    }

    /** Hide the soft keyboard and release the IME host view. Called by the Rust shell via JNI. */
    public void imeHide() {
        imeWanted = false;
        final EditText view = imeView;
        if (view == null) {
            return;
        }
        runOnUiThread(() -> {
            try {
                InputMethodManager imm =
                        (InputMethodManager) getSystemService(INPUT_METHOD_SERVICE);
                if (imm != null) {
                    imm.hideSoftInputFromWindow(view.getWindowToken(), 0);
                }
                view.clearFocus();
                // Drop the scratch buffer between sessions so it cannot grow without bound.
                view.setText("");
            } catch (Throwable error) {
                Log.w(TAG, "Could not hide the soft keyboard", error);
            }
        });
    }

    /**
     * Hand the Rust shell at most one queued keyboard event per call. The first character tags the
     * event ("c" commit, "p" pre-edit, "f" finish, "d" delete-with-counts, "a" editor action) and
     * the remainder is the payload; returns {@code null} when the queue is empty.
     */
    public String imePoll() {
        return imeEvents.poll();
    }

    /**
     * Hand the Rust shell the private path of one file the system asked us to open (or that another
     * app shared with us), or {@code null} when nothing is queued. Each staged copy is delivered
     * once and then opened exactly like a file chosen in the open picker.
     */
    public String intentPoll() {
        return intentPaths.poll();
    }

    public String pcPollPickerResult() { return results.poll(); }

    public void pcRequestOpen(long id, boolean multiple) {
        runOnUiThread(() -> {
            try {
                Intent intent = new Intent(Intent.ACTION_OPEN_DOCUMENT);
                intent.addCategory(Intent.CATEGORY_OPENABLE);
                intent.setType("*/*");
                intent.putExtra(Intent.EXTRA_ALLOW_MULTIPLE, multiple);
                intent.addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION | Intent.FLAG_GRANT_WRITE_URI_PERMISSION | Intent.FLAG_GRANT_PERSISTABLE_URI_PERMISSION);
                requestIds.put(REQ_OPEN, id);
                startActivityForResult(intent, REQ_OPEN);
            } catch (Exception e) {
                enqueueError(id, "open", e.toString());
            }
        });
    }

    public void pcRequestSave(long id, String suggestedName) {
        runOnUiThread(() -> {
            try {
                String name = safeName(suggestedName == null ? "Untitled.pcraft" : suggestedName);
                Intent intent = new Intent(Intent.ACTION_CREATE_DOCUMENT);
                intent.addCategory(Intent.CATEGORY_OPENABLE);
                intent.setType(mimeFor(name));
                intent.putExtra(Intent.EXTRA_TITLE, name);
                intent.addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION | Intent.FLAG_GRANT_WRITE_URI_PERMISSION | Intent.FLAG_GRANT_PERSISTABLE_URI_PERMISSION);
                requestIds.put(REQ_SAVE, id);
                suggestedNames.put(id, name);
                startActivityForResult(intent, REQ_SAVE);
            } catch (Exception e) {
                enqueueError(id, "save", e.toString());
            }
        });
    }

    @Override
    protected void onActivityResult(int requestCode, int resultCode, Intent data) {
        super.onActivityResult(requestCode, resultCode, data);
        if (requestCode != REQ_OPEN && requestCode != REQ_SAVE) return;
        Long boxedId = requestIds.remove(requestCode);
        if (boxedId == null) return;
        final long id = boxedId;
        final String kind = requestCode == REQ_OPEN ? "open" : "save";
        if (resultCode != RESULT_OK || data == null) {
            suggestedNames.remove(id);
            enqueueCancel(id, kind);
            return;
        }
        if (requestCode == REQ_SAVE) {
            Uri uri = data.getData();
            if (uri == null) { enqueueCancel(id, kind); return; }
            persistPermission(uri, data);
            try {
                String name = displayName(uri);
                if (name == null || name.trim().isEmpty()) name = suggestedNames.getOrDefault(id, "Untitled.pcraft");
                File stage = newStageFile(name, uri);
                rememberUri(stage, uri);
                enqueueSave(id, stage.getAbsolutePath());
            } catch (Exception e) {
                enqueueError(id, kind, e.toString());
            } finally {
                suggestedNames.remove(id);
            }
            return;
        }

        List<Uri> uris = new ArrayList<>();
        ClipData clip = data.getClipData();
        if (clip != null) {
            for (int i = 0; i < clip.getItemCount(); i++) {
                Uri uri = clip.getItemAt(i).getUri();
                if (uri != null) uris.add(uri);
            }
        } else if (data.getData() != null) {
            uris.add(data.getData());
        }
        if (uris.isEmpty()) { enqueueCancel(id, kind); return; }
        for (Uri uri : uris) persistPermission(uri, data);
        io.execute(() -> {
            try {
                JSONArray paths = new JSONArray();
                for (Uri uri : uris) {
                    File stage = newStageFile(displayName(uri), uri);
                    copyUriToFile(uri, stage);
                    rememberUri(stage, uri);
                    paths.put(stage.getAbsolutePath());
                }
                JSONObject result = base(id, "open", "ok");
                result.put("paths", paths);
                results.offer(result.toString());
            } catch (Exception e) {
                enqueueError(id, "open", e.toString());
            }
        });
    }

    /** Commit an app-private staged file to its remembered SAF URI; used by Rust after every save. */
    public String pcCommitIfMapped(String path) {
        try {
            String uriText = getSharedPreferences(PREFS, MODE_PRIVATE).getString("uri:" + path, null);
            if (uriText == null) return "NONE";
            Uri uri = Uri.parse(uriText);
            OutputStream out = getContentResolver().openOutputStream(uri, "wt");
            if (out == null) out = getContentResolver().openOutputStream(uri, "w");
            if (out == null) return "ERROR: Android document provider did not open an output stream";
            try (InputStream in = new FileInputStream(new File(path)); OutputStream target = out) {
                byte[] buffer = new byte[64 * 1024];
                int count;
                while ((count = in.read(buffer)) != -1) target.write(buffer, 0, count);
                target.flush();
            }
            return "OK";
        } catch (Exception e) {
            Log.e(TAG, "Could not write a selected document", e);
            return "ERROR: Could not write the selected document: " + e.getMessage();
        }
    }

    /**
     * The working copy of a document the user chose through SAF. It lives under {@link #appStorageDir()}
     * so a file manager or a computer finds the file the app is editing, and inside a plain
     * {@code media} directory named after the document rather than a random per-open directory, so the
     * same document always lands on the same path.
     *
     * <p>A document that is opened again reuses its staged file; a different document whose name is
     * already taken gets a {@code " (2)"}-style suffix, because the staged path is the key that maps
     * back to the picked document and two documents sharing one path would overwrite each other.
     * A name whose staged copy has since been deleted (the app prunes working copies the recent list
     * let go of) is free again: the stale {@code uri:} mapping is harmless.
     */
    private File newStageFile(String name, Uri uri) throws Exception {
        File dir = new File(appStorageDir(), "media");
        if (!dir.mkdirs() && !dir.isDirectory()) throw new Exception("Could not create the media directory");
        String safe = safeName(name == null ? "imported-file" : name);
        String stem = safe, extension = "";
        int dot = safe.lastIndexOf('.');
        if (dot > 0) { stem = safe.substring(0, dot); extension = safe.substring(dot); }
        SharedPreferences prefs = getSharedPreferences(PREFS, MODE_PRIVATE);
        String owner = uri == null ? null : uri.toString();
        for (int attempt = 1; attempt <= 10000; attempt++) {
            String candidate = attempt == 1 ? safe : stem + " (" + attempt + ")" + extension;
            File file = new File(dir, candidate);
            String mapped = prefs.getString("uri:" + file.getAbsolutePath(), null);
            if (mapped == null || mapped.equals(owner) || !file.exists()) return file;
        }
        throw new Exception("Could not find a free name for the selected document in " + dir);
    }

    private static String safeName(String name) {
        String clean = name.replace('\\', '/');
        int slash = clean.lastIndexOf('/');
        if (slash >= 0) clean = clean.substring(slash + 1);
        clean = clean.replaceAll("[^A-Za-z0-9._() -]", "_").trim();
        if (clean.isEmpty() || clean.equals(".") || clean.equals("..")) return "untitled";
        return clean;
    }

    private String displayName(Uri uri) {
        try (Cursor cursor = getContentResolver().query(uri, null, null, null, null)) {
            if (cursor != null && cursor.moveToFirst()) {
                int index = cursor.getColumnIndex(OpenableColumns.DISPLAY_NAME);
                if (index >= 0) return safeName(cursor.getString(index));
            }
        } catch (Exception ignored) { }
        String segment = uri.getLastPathSegment();
        return segment == null ? "imported-file" : safeName(segment);
    }

    private void persistPermission(Uri uri, Intent data) {
        try {
            int flags = data.getFlags() & (Intent.FLAG_GRANT_READ_URI_PERMISSION | Intent.FLAG_GRANT_WRITE_URI_PERMISSION);
            if (flags != 0) getContentResolver().takePersistableUriPermission(uri, flags);
        } catch (Exception e) {
            Log.i(TAG, "Provider did not offer a persistable grant for " + uri, e);
        }
    }

    private void rememberUri(File path, Uri uri) {
        getSharedPreferences(PREFS, MODE_PRIVATE).edit().putString("uri:" + path.getAbsolutePath(), uri.toString()).apply();
    }

    private void copyUriToFile(Uri uri, File destination) throws Exception {
        InputStream input = getContentResolver().openInputStream(uri);
        if (input == null) throw new Exception("Android document provider did not open the selected file");
        try (InputStream in = input; FileOutputStream out = new FileOutputStream(destination)) {
            byte[] buffer = new byte[64 * 1024];
            int count;
            while ((count = in.read(buffer)) != -1) out.write(buffer, 0, count);
            out.flush();
        }
    }

    /**
     * Collect the file(s) an {@code ACTION_VIEW} / {@code ACTION_SEND} intent carries: the data URI
     * (opening a registered type), the intent's {@link ClipData} (modern shares) and the legacy
     * {@code EXTRA_STREAM} extra, de-duplicated. Anything else (in particular the launcher's MAIN
     * intent) is ignored so normal launches are untouched.
     */
    private static List<Uri> collectIncomingUris(Intent intent) {
        LinkedHashSet<Uri> uris = new LinkedHashSet<>();
        String action = intent.getAction();
        if (Intent.ACTION_VIEW.equals(action)) {
            if (intent.getData() != null) uris.add(intent.getData());
        } else if (Intent.ACTION_SEND.equals(action)) {
            Uri stream = intent.getParcelableExtra(Intent.EXTRA_STREAM);
            if (stream != null) uris.add(stream);
        } else if (Intent.ACTION_SEND_MULTIPLE.equals(action)) {
            ArrayList<Uri> streams = intent.getParcelableArrayListExtra(Intent.EXTRA_STREAM);
            if (streams != null) uris.addAll(streams);
        } else {
            return new ArrayList<>();
        }
        ClipData clip = intent.getClipData();
        if (clip != null) {
            for (int i = 0; i < clip.getItemCount(); i++) {
                Uri uri = clip.getItemAt(i).getUri();
                if (uri != null) uris.add(uri);
            }
        }
        return new ArrayList<>(uris);
    }

    /**
     * Copy every file the given intent carries into app-private staging and queue its path for the
     * Rust shell. Runs off the UI thread because the copy can be large; the shell polls
     * {@link #intentPoll()} until the path appears, so a slow copy never blocks startup.
     */
    private void handleIncomingFiles(Intent intent) {
        if (intent == null) return;
        List<Uri> uris = collectIncomingUris(intent);
        if (uris.isEmpty()) return;
        for (Uri uri : uris) persistPermission(uri, intent);
        io.execute(() -> {
            for (Uri uri : uris) {
                try {
                    File stage = newStageFile(displayName(uri), uri);
                    copyUriToFile(uri, stage);
                    rememberUri(stage, uri);
                    intentPaths.offer(stage.getAbsolutePath());
                } catch (Exception e) {
                    Log.e(TAG, "Could not open the file the system handed the app: " + uri, e);
                }
            }
        });
    }

    private JSONObject base(long id, String kind, String status) throws Exception {
        JSONObject value = new JSONObject();
        value.put("id", id);
        value.put("kind", kind);
        value.put("status", status);
        return value;
    }

    private void enqueueCancel(long id, String kind) {
        try { results.offer(base(id, kind, "cancel").toString()); }
        catch (Exception e) { Log.e(TAG, "Could not encode cancellation", e); }
    }

    private void enqueueError(long id, String kind, String message) {
        try {
            JSONObject value = base(id, kind, "error");
            value.put("message", message == null ? "Unknown Android file picker error" : message);
            results.offer(value.toString());
        } catch (Exception e) { Log.e(TAG, "Could not encode picker error", e); }
    }

    private void enqueueSave(long id, String path) {
        try {
            JSONObject value = base(id, "save", "ok");
            value.put("path", path);
            results.offer(value.toString());
        } catch (Exception e) { enqueueError(id, "save", e.toString()); }
    }

    private static String mimeFor(String fileName) {
        String lower = fileName.toLowerCase(java.util.Locale.ROOT);
        if (lower.endsWith(".png")) return "image/png";
        if (lower.endsWith(".jpg") || lower.endsWith(".jpeg")) return "image/jpeg";
        if (lower.endsWith(".webp")) return "image/webp";
        if (lower.endsWith(".gif")) return "image/gif";
        if (lower.endsWith(".tif") || lower.endsWith(".tiff")) return "image/tiff";
        if (lower.endsWith(".bmp")) return "image/bmp";
        if (lower.endsWith(".svg")) return "image/svg+xml";
        return "application/octet-stream";
    }
}
